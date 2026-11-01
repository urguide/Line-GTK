use crate::store::{Cursor, Gap, Page, Store, id};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

mod expressions;
mod receipts;
mod scheduler;

/// Implemented by the host's IPC transport; no GTK or JavaScript scheduling here.
pub trait Transport: Send + Sync + 'static {
    fn call(&self, method: &str, params: Value, generation: u64) -> Result<Value>;
    fn call_cancellable(
        &self,
        method: &str,
        params: Value,
        generation: u64,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Value> {
        self.call(method, params, generation)
    }
    fn materialize(&self, message: &Value, generation: u64) -> Result<Value>;
    fn emit(&self, event: Value);
    fn memory_media(&self, _account: &str, _chat: &str) -> bool {
        false
    }
    fn media_visible(&self, _chat: &str, _id: &str) -> bool {
        false
    }
    fn session(&self, _account: &str, _generation: u64) {}
    fn application(&self, method: &str, params: Value, generation: u64) -> Result<Value> {
        self.call(method, params, generation)
    }
}

pub enum Command {
    FocusChat {
        chat: String,
        generation: u64,
    },
    PeerRead {
        chat: String,
        reader: String,
        message: String,
        generation: u64,
    },
    SyncReceipts {
        chat: String,
        generation: u64,
    },
    FlushReceipts {
        done: mpsc::SyncSender<Result<()>>,
    },
    Operation {
        id: u64,
        method: String,
        params: Value,
        generation: u64,
    },
    Session {
        account: String,
        generation: u64,
    },
    Fetch {
        id: u64,
        chat: String,
        before: Option<Cursor>,
        generation: u64,
    },
    Download {
        id: u64,
        chat: String,
        message: String,
        generation: u64,
    },
    Live {
        chat: String,
        message: Value,
        generation: u64,
    },
    Unsend {
        chat: String,
        message: String,
        generation: u64,
    },
    Invalidate {
        chat: String,
        generation: u64,
    },
    Clear {
        id: u64,
        generation: u64,
    },
    Retention(String),
    VisibleMedia {
        chat: String,
        ids: Vec<String>,
        generation: u64,
    },
    Stop,
}

enum Work {
    Receipts {
        chat: String,
        generation: u64,
    },
    Expression {
        generation: u64,
    },
    Media {
        chat: String,
        row: Value,
        generation: u64,
    },
    Gap {
        chat: String,
        generation: u64,
    },
    Advance {
        chat: String,
        gap: Gap,
        next: Option<Cursor>,
        generation: u64,
    },
}

pub struct Engine<T: Transport> {
    policy: crate::cache_policy::CachePolicy,
    store: Store,
    transport: Arc<T>,
    generation: Arc<AtomicU64>,
    session_generation: u64,
    account: String,
    legacy: std::path::PathBuf,
    background: VecDeque<Work>,
    queued_media: HashSet<(String, String)>,
    queued_gaps: HashSet<String>,
    last_request: Option<Instant>,
    last_media: Option<Instant>,
    page_pause: Duration,
    media_pause: Duration,
    expression_jobs: Vec<crate::expression_jobs::DownloadJob>,
    expression_scheduled: bool,
    receipts: HashMap<(String, String), crate::receipts::Receipts>,
    dirty_receipts: HashSet<(String, String)>,
    receipts_flush_at: Instant,
    receipts_synced: HashMap<String, Instant>,
    queued_receipts: HashSet<String>,
    scheduler: scheduler::Scheduler,
}

impl<T: Transport> Engine<T> {
    pub fn new(
        store: Store,
        transport: T,
        generation: Arc<AtomicU64>,
        legacy: std::path::PathBuf,
    ) -> Self {
        Self {
            policy: crate::cache_policy::CachePolicy::default(),
            store,
            transport: Arc::new(transport),
            generation,
            legacy,
            session_generation: 0,
            account: String::new(),
            background: VecDeque::new(),
            queued_media: HashSet::new(),
            queued_gaps: HashSet::new(),
            last_request: None,
            last_media: None,
            page_pause: Duration::from_secs(1),
            media_pause: Duration::from_millis(500),
            expression_jobs: Vec::new(),
            expression_scheduled: false,
            receipts: HashMap::new(),
            dirty_receipts: HashSet::new(),
            receipts_flush_at: Instant::now(),
            receipts_synced: HashMap::new(),
            queued_receipts: HashSet::new(),
            scheduler: scheduler::Scheduler::default(),
        }
    }

    fn valid(&self, generation: u64) -> Result<()> {
        if self.generation.load(Ordering::SeqCst) != generation
            || self.session_generation != generation
            || self.account.is_empty()
        {
            bail!("session_changed");
        }
        Ok(())
    }

    fn pace(&self, last: Option<Instant>, pause: Duration, generation: u64) -> Result<()> {
        if let Some(last) = last {
            while last.elapsed() < pause {
                self.valid(generation)?;
                thread::sleep(
                    pause
                        .saturating_sub(last.elapsed())
                        .min(Duration::from_millis(50)),
                );
            }
        }
        self.valid(generation)
    }

    fn fetch(&mut self, chat: &str, before: Option<&Cursor>, generation: u64) -> Result<Page> {
        self.valid(generation)?;
        if let Some(cursor) = before {
            cursor.validate()?;
        }
        if let Some(mut page) = self.store.page(chat, before, self.policy.messages)? {
            self.decorate_receipts(chat, &mut page.messages)?;
            return Ok(page);
        }
        self.pace(self.last_request, self.page_pause, generation)?;
        let result = self.transport.call(
            "bridge_fetch_page",
            json!({"chatMid":chat,"before":before}),
            generation,
        );
        self.last_request = Some(Instant::now());
        self.valid(generation)?;
        let mut page: Page = serde_json::from_value(result?)?;
        for row in &mut page.messages {
            *row = crate::message::from_envelope(row, &self.account);
        }
        if page.messages.len() > 20 {
            bail!("protocol returned oversized history page");
        }
        if let Some(cursor) = &page.next_cursor {
            cursor.validate()?;
        }
        let mut page = self.store.save_page(chat, before, &page)?;
        self.decorate_receipts(chat, &mut page.messages)?;
        Ok(page)
    }

    fn queue_media(&mut self, chat: &str, rows: &[Value], generation: u64) {
        for row in rows.iter().rev() {
            if self.transport.memory_media(&self.account, chat)
                && matches!(row["contentType"].as_str(), Some("IMAGE" | "VIDEO"))
            {
                continue;
            }
            if row["decodeError"].is_null()
                && matches!(
                    row["contentType"].as_str(),
                    Some("IMAGE" | "VIDEO" | "AUDIO" | "STICKER")
                )
                && self
                    .queued_media
                    .insert((chat.to_owned(), id(row).to_owned()))
            {
                self.background.push_back(Work::Media {
                    chat: chat.to_owned(),
                    row: row.clone(),
                    generation,
                });
            }
        }
    }

    fn queue_gap(&mut self, chat: &str, generation: u64) {
        if self.transport.memory_media(&self.account, chat) {
            return;
        }
        if self.queued_gaps.insert(chat.to_owned()) {
            self.background.push_back(Work::Gap {
                chat: chat.to_owned(),
                generation,
            });
        }
    }

    fn response(&self, id: u64, result: Result<Value>, generation: u64) {
        if id == 0 {
            return;
        }
        if self.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        match result {
            Ok(result) => self
                .transport
                .emit(json!({"id":id,"ok":true,"result":result})),
            Err(error) => self
                .transport
                .emit(json!({"id":id,"ok":false,"error":error.to_string()})),
        }
    }

    fn media(&mut self, chat: &str, row: Value, generation: u64) -> Result<Value> {
        self.pace(self.last_media, self.media_pause, generation)?;
        let Some(mut current) = self.store.message(chat, id(&row))? else {
            bail!("message unavailable");
        };
        // Use the authoritative conversation, not the sender (or our own MID
        // in incoming direct messages), when choosing a media cache directory.
        current["chatMid"] = json!(chat);
        let memory = self.transport.memory_media(&self.account, chat)
            && matches!(current["contentType"].as_str(), Some("IMAGE" | "VIDEO"));
        if memory && !self.transport.media_visible(chat, id(&current)) {
            bail!("media_not_visible");
        }
        let result = self.transport.materialize(&current, generation);
        self.last_media = Some(Instant::now());
        self.valid(generation)?;
        let updated = result?;
        let memory_now = self.transport.memory_media(&self.account, chat)
            && matches!(updated["contentType"].as_str(), Some("IMAGE" | "VIDEO"));
        if !memory && !memory_now {
            self.store
                .save_messages(chat, std::slice::from_ref(&updated))?;
        }
        self.transport.emit(json!({"event":"media_ready","chatMid":chat,"messageId":id(&updated),"imagePath":updated["imagePath"],"audioPath":updated["audioPath"],"filePath":updated["filePath"]}));
        Ok(updated)
    }

    fn mark_read(&mut self, params: &Value, generation: u64) -> Value {
        if self.valid(generation).is_err() {
            return json!({"marked":false,"skipped":"not_logged_in"});
        }
        let chat = params["chatMid"].as_str().unwrap_or_default();
        let message = params["lastMessageId"].as_str().unwrap_or_default();
        if chat.trim().is_empty() || message.trim().is_empty() {
            return json!({"marked":false,"skipped":"missing_ids"});
        }
        if !message.bytes().all(|b| b.is_ascii_digit()) || message.parse::<u64>().is_err() {
            return json!({"marked":false,"skipped":"invalid_message_id"});
        }
        let result: Result<Value> = (|| {
            self.pace(self.last_request, self.page_pause, generation)?;
            let response = self.transport.call(
                "bridge_mark_read",
                json!({"chatMid":chat,"lastMessageId":message}),
                generation,
            );
            self.last_request = Some(Instant::now());
            self.valid(generation)?;
            let response = response?;
            if response["marked"] != true {
                return Ok(
                    json!({"marked":false,"error":response["error"].as_str().unwrap_or("read receipt not acknowledged")}),
                );
            }
            // The server already accepted the receipt. Do not misreport a local
            // cache failure as a rejected receipt, and never turn it into a toast.
            let cache_error = self
                .store
                .mark_chat_read(chat)
                .err()
                .map(|error| error.to_string());
            Ok(
                json!({"marked":true,"chatMid":chat,"lastMessageId":message,"cacheUpdated":cache_error.is_none(),"cacheError":cache_error}),
            )
        })();
        match result {
            Ok(value) => value,
            Err(error) => json!({"marked":false,"error":format!("{error}")}),
        }
    }

    fn command(&mut self, command: Command) -> Result<bool> {
        match command {
            Command::FocusChat { chat, generation } => {
                self.valid(generation)?;
                self.scheduler.focus(chat);
                self.scheduler.enabled = true;
            }
            Command::FlushReceipts { done } => {
                let _ = done.send(self.flush_receipts());
            }
            Command::SyncReceipts { chat, generation } => {
                self.valid(generation)?;
                self.emit_receipts(&chat)?;
                self.queue_receipts(&chat, generation);
            }
            Command::PeerRead {
                chat,
                reader,
                message,
                generation,
            } => {
                self.valid(generation)?;
                if chat.is_empty() || (chat.starts_with('u') && reader != chat) {
                    return Ok(true);
                }
                self.load_receipts(&chat)?;
                let key = (self.account.clone(), chat.clone());
                if self.receipts.get_mut(&key).unwrap().peer_read(
                    &reader,
                    &message,
                    &self.account,
                    chat.starts_with('u'),
                ) {
                    self.dirty_receipts.insert(key);
                    self.emit_receipts(&chat)?;
                    self.queue_receipts(&chat, generation);
                }
            }
            Command::Operation {
                id,
                method,
                mut params,
                generation,
            } => {
                if self.scheduler.enabled
                    && matches!(
                        method.as_str(),
                        "sender_profile"
                            | "profile_avatar"
                            | "expression_icon"
                            | "emoji_images"
                            | "list_chat_members"
                    )
                {
                    self.scheduler
                        .operations
                        .push_back((id, method, params, generation));
                    return Ok(true);
                }
                if method == "mark_read" {
                    if self.scheduler.enabled {
                        self.scheduler.reads.push_back((id, params, generation));
                        return Ok(true);
                    }
                    let result = self.mark_read(&params, generation);
                    self.response(id, Ok(result), generation);
                    return Ok(true);
                }
                let upload = matches!(method.as_str(), "send_media" | "send_audio");
                let upload_chat = params["chatMid"].as_str().unwrap_or_default().to_owned();
                if upload && self.generation.load(Ordering::SeqCst) == generation {
                    self.transport.emit(crate::attachments::upload_progress(
                        &upload_chat,
                        0.02,
                        "Preparing…",
                        false,
                    ));
                }
                let result = (|| {
                    self.valid(generation)?;
                    if method == "reply_original" {
                        let chat = params["chatMid"].as_str().context("missing chat ID")?;
                        let id = params["messageId"].as_str().context("missing message ID")?;
                        let mut message = self.store.message(chat, id)?;
                        if self.transport.memory_media(&self.account, chat)
                            && let Some(row) = message.as_mut()
                            && matches!(row["contentType"].as_str(), Some("IMAGE" | "VIDEO"))
                        {
                            row["imagePath"] = Value::Null;
                            row["filePath"] = Value::Null;
                        }
                        // A quote only reads the original cache entry. Never materialize
                        // media under the replying message's ID or refresh its cache age.
                        return Ok(json!({"message":message}));
                    }
                    if matches!(
                        method.as_str(),
                        "expression_local"
                            | "expression_queue"
                            | "enqueue_expressions"
                            | "stop_expressions"
                    ) {
                        return self.expression_operation(&method, &params, generation);
                    }
                    if method == "mute_chat" {
                        let mid = params["mid"].as_str().unwrap_or_default();
                        if !mid.starts_with('u') || mid.len() < 2 {
                            bail!("mute_dm_only");
                        }
                        if !params["muted"].is_boolean() {
                            bail!("invalid_mute_state");
                        }
                    }
                    if method == "invalidate_catalogs" {
                        self.store.invalidate_catalogs()?;
                        return Ok(json!({}));
                    }
                    let cached_kind = matches!(
                        method.as_str(),
                        "list_chats" | "list_friends" | "list_stickers"
                    );
                    if cached_kind
                        && !params["force"].as_bool().unwrap_or(false)
                        && let Some(mut value) = self
                            .store
                            .catalog(&method, self.policy.catalog_ttl(&method))?
                        && !value["partial"].as_bool().unwrap_or(false)
                    {
                        self.store.decorate_chats(&mut value)?;
                        return Ok(value);
                    }
                    params["account"] = json!(self.account);
                    if matches!(
                        method.as_str(),
                        "expression_library" | "download_expression"
                    ) {
                        let kind = if params["kind"] == "emoji" {
                            "emoji"
                        } else {
                            "sticker"
                        };
                        let owned_key = format!("expression_owned_{kind}");
                        let installed_key = format!("expression_installed_{kind}");
                        let cached = if params["force"] == true {
                            None
                        } else {
                            self.store.catalog(&owned_key, self.policy.owned_stickers)?
                        };
                        let (owned, stale) = if let Some(value) = cached {
                            (value, false)
                        } else {
                            let fetched = crate::expressions::owned(|offset| {
                                self.valid(generation)?;
                                self.pace(self.last_request, self.page_pause, generation)?;
                                let result = self.transport.call(
                                    "bridge_owned_expressions",
                                    json!({"kind":kind,"offset":offset}),
                                    generation,
                                );
                                self.last_request = Some(Instant::now());
                                result
                            });
                            self.valid(generation)?;
                            match fetched {
                                Ok(value) => {
                                    self.store.save_catalog(&owned_key, &value)?;
                                    (value, false)
                                }
                                Err(error) => match self
                                    .store
                                    .stale_catalog(&owned_key, self.policy.disk_chats)?
                                {
                                    Some(value) if method == "expression_library" => (value, true),
                                    _ => return Err(error),
                                },
                            }
                        };
                        let mut installed = self
                            .store
                            .catalog(&installed_key, i64::MAX)?
                            .or(if kind == "sticker" {
                                self.store.stale_catalog("list_stickers", i64::MAX)?
                            } else {
                                None
                            })
                            .unwrap_or(json!({"packs":[]}));
                        if kind == "sticker" {
                            installed["packs"]
                                .as_array_mut()
                                .context("invalid installed library")?
                                .retain(|p| p["id"] != "__recent__");
                        }
                        if method == "download_expression" {
                            let product = owned["products"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .find(|p| p["id"] == params["packageId"])
                                .context("product not owned")?;
                            params["product"] = product.clone();
                            let pack =
                                self.transport
                                    .application(&method, params.clone(), generation)?;
                            self.valid(generation)?;
                            let packs = installed["packs"]
                                .as_array_mut()
                                .context("invalid installed library")?;
                            packs.retain(|p| p["id"] != pack["id"]);
                            packs.push(pack.clone());
                            self.store.save_catalog(&installed_key, &installed)?;
                            return Ok(pack);
                        }
                        let ids: std::collections::HashSet<_> = owned["products"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|p| p["id"].as_str())
                            .collect();
                        installed["packs"]
                            .as_array_mut()
                            .context("invalid installed library")?
                            .retain(|p| p["id"].as_str().is_some_and(|id| ids.contains(id)));
                        self.store.save_catalog(&installed_key, &installed)?;
                        params["installed"] = installed;
                        params["products"] = owned["products"].clone();
                        if kind == "sticker" {
                            params["recent"] = self
                                .store
                                .catalog("recent_stickers", i64::MAX)?
                                .unwrap_or(json!([]));
                        }
                        let mut result = self.transport.application(&method, params, generation)?;
                        result["stale"] = json!(stale);
                        return Ok(result);
                    }
                    if method == "send_message" {
                        let installed = self
                            .store
                            .catalog("expression_installed_emoji", i64::MAX)?
                            .unwrap_or_default();
                        let mentions = serde_json::from_value::<Vec<crate::mentions::Mention>>(
                            params.get("mentions").cloned().unwrap_or(json!([])),
                        )?;
                        let (text, metadata) = crate::mentions::compose(
                            params["text"].as_str().unwrap_or_default(),
                            &installed,
                            &mentions,
                        );
                        params["text"] = json!(text);
                        params["contentMetadata"] = metadata;
                    }
                    if matches!(method.as_str(), "list_chats" | "list_friends") {
                        params["previous"] = json!({
                            "chats":self.store.stale_catalog("list_chats",self.policy.disk_chats)?.unwrap_or_default()["chats"],
                            "friends":self.store.stale_catalog("list_friends",self.policy.disk_chats)?.unwrap_or_default()["friends"],
                        });
                    }
                    if method == "list_stickers" {
                        params["owned"] = self
                            .store
                            .catalog("owned_stickers", self.policy.owned_stickers)?
                            .unwrap_or_default();
                        params["previous"] = self
                            .store
                            .stale_catalog("list_stickers", self.policy.disk_chats)?
                            .unwrap_or_default();
                        params["recent"] = self
                            .store
                            .catalog("recent_stickers", i64::MAX)?
                            .unwrap_or(json!([]));
                    }
                    if method == "send_sticker" {
                        let explicit = crate::message::string(&params["version"]).trim().to_owned();
                        if !explicit.is_empty() {
                            params["version"] = json!(explicit);
                        } else {
                            let chooser = self
                                .store
                                .catalog("list_stickers", self.policy.owned_stickers)?;
                            let cached = self
                                .store
                                .catalog("owned_stickers", self.policy.owned_stickers)?
                                .or_else(|| {
                                    chooser
                                        .filter(|v| !v["partial"].as_bool().unwrap_or(false))
                                        .and_then(|v| v.get("products").cloned())
                                });
                            let owned = if let Some(owned) = cached {
                                owned
                            } else {
                                self.pace(self.last_request, self.page_pause, generation)?;
                                let query = self.transport.call(
                                    "bridge_owned_stickers",
                                    json!({}),
                                    generation,
                                );
                                self.last_request = Some(Instant::now());
                                self.valid(generation)?;
                                match query {
                                    Ok(owned) => {
                                        self.store.save_catalog("owned_stickers", &owned)?;
                                        owned
                                    }
                                    Err(_) => Value::Null, // Legacy behavior uses version 1 when lookup fails.
                                }
                            };
                            params["version"] = json!(crate::stickers::package_version(
                                &owned,
                                params["packageId"].as_str().unwrap_or_default()
                            ));
                        }
                    }
                    self.pace(self.last_request, self.page_pause, generation)?;
                    let operation = self
                        .transport
                        .application(&method, params.clone(), generation);
                    self.last_request = Some(Instant::now());
                    self.valid(generation)?;
                    let mut value = match operation {
                        Err(error)
                            if method == "send_sticker"
                                && crate::stickers::not_owned(&error.to_string()) =>
                        {
                            self.store.reject_sticker(
                                params["stickerId"].as_str().unwrap_or_default(),
                                params["packageId"].as_str().unwrap_or_default(),
                            )?;
                            bail!("sticker_not_owned");
                        }
                        other => other?,
                    };
                    if cached_kind {
                        self.store.decorate_chats(&mut value)?;
                        self.store.save_catalog(&method, &value)?;
                        if method == "list_stickers"
                            && !value["partial"].as_bool().unwrap_or(false)
                            && value["products"].is_array()
                            && params["owned"].is_null()
                        {
                            self.store
                                .save_catalog("owned_stickers", &value["products"])?;
                        }
                    }
                    if method.starts_with("send_") {
                        if let Some(raw) = value.get("rawMessage") {
                            let mut message = crate::message::normalize(raw, &self.account);
                            message["from"] = json!(self.account);
                            message["to"] = params["chatMid"].clone();
                            message["mine"] = json!(true);
                            if method == "send_sticker" {
                                message["stickerId"] = params["stickerId"].clone();
                                message["stickerPackageId"] = params["packageId"].clone();
                                message["contentType"] = json!("STICKER");
                            }
                            for key in ["filePath", "imagePath", "audioPath", "fileName"] {
                                if !value[key].is_null() {
                                    message[key] = value[key].clone();
                                }
                            }
                            message["rawMessage"] = raw.clone();
                            let chat = params["chatMid"].as_str().unwrap_or_default();
                            self.store
                                .save_messages(chat, std::slice::from_ref(&message))?;
                            self.decorate_receipts(chat, std::slice::from_mut(&mut message))?;
                            self.store.invalidate(chat)?;
                            self.queue_media(chat, std::slice::from_ref(&message), generation);
                            if let Some(object) = message.as_object_mut() {
                                object.remove("rawMessage");
                            }
                            value = json!({"message":message,"sent":value["sent"],"fallback":value["fallback"]});
                        }
                        self.store.invalidate_catalogs()?;
                        if method == "send_sticker" {
                            let mut recent = self
                                .store
                                .catalog("recent_stickers", i64::MAX)?
                                .and_then(|v| v.as_array().cloned())
                                .unwrap_or_default();
                            recent.retain(|s| s["stickerId"] != params["stickerId"]);
                            recent.insert(0, json!({"stickerId":params["stickerId"],"packageId":params["packageId"],"version":params["version"]}));
                            recent.truncate(200);
                            self.store.save_catalog("recent_stickers", &json!(recent))?;
                        }
                    }
                    if method == "add_friend" {
                        self.store.remember_contact(&value)?;
                    }
                    if method == "mute_chat" {
                        let mid = params["mid"].as_str().unwrap_or_default();
                        let muted = params["muted"].as_bool().unwrap_or(false);
                        if value["mid"] != mid || value["muted"] != muted {
                            bail!("mute setting not acknowledged");
                        }
                        self.store.set_chat_muted(mid, muted)?;
                        self.transport
                            .emit(json!({"event":"chat_mute","mid":mid,"muted":muted}));
                    }
                    if method == "clear_cache" {
                        self.store.invalidate_catalogs()?;
                    }
                    if method == "unsend_message" {
                        self.store.unsend(
                            params["chatMid"].as_str().unwrap_or_default(),
                            params["messageId"].as_str().unwrap_or_default(),
                        )?;
                    }
                    if method == "clear_cache" {
                        let kind = params["kind"].as_str().unwrap_or("all");
                        if matches!(kind, "all" | "messages") {
                            self.scheduler.clear_work();
                            self.background.clear();
                            self.expression_scheduled = false;
                            self.queued_media.clear();
                            self.queued_gaps.clear();
                        }
                        self.store.clear_scope(kind)?;
                        if matches!(kind, "all" | "messages") {
                            self.clear_receipts();
                        }
                        if matches!(kind, "all" | "stickers") {
                            self.expressions_cache_cleared(generation)?;
                        }
                        self.schedule_expressions(generation);
                    }
                    Ok(value)
                })();
                // Finish only after the whole operation, including SQLite persistence.
                // All early errors take this path, even if no upload RPC was started.
                if upload && self.generation.load(Ordering::SeqCst) == generation {
                    self.transport.emit(crate::attachments::upload_progress(
                        &upload_chat,
                        if result.is_ok() { 1.0 } else { 0.0 },
                        if result.is_ok() { "Sent" } else { "" },
                        true,
                    ));
                }
                self.response(id, result, generation);
            }
            Command::Stop => return Ok(false),
            Command::VisibleMedia {
                chat,
                ids,
                generation,
            } => {
                self.valid(generation)?;
                for message in ids {
                    if self.transport.memory_media(&self.account, &chat)
                        && self.transport.media_visible(&chat, &message)
                        && let Some(row) = self.store.message(&chat, &message)?
                        && matches!(row["contentType"].as_str(), Some("IMAGE" | "VIDEO"))
                        && self.queued_media.insert((chat.clone(), message))
                    {
                        self.background.push_back(Work::Media {
                            chat: chat.clone(),
                            row,
                            generation,
                        });
                    }
                }
            }
            Command::Retention(mode) => {
                self.policy = crate::cache_policy::CachePolicy::for_mode(&mode);
                self.store.set_retention(&mode);
            }
            Command::Session {
                account,
                generation,
            } => {
                if self.generation.load(Ordering::SeqCst) != generation {
                    return Ok(true);
                }
                if self.account == account && self.session_generation == generation {
                    return Ok(true);
                }
                if let Err(error) = self.flush_receipts() {
                    eprintln!("[core receipts] {error}");
                }
                self.receipts
                    .retain(|key, _| self.dirty_receipts.contains(key));
                self.receipts_synced.clear();
                self.queued_receipts.clear();
                self.background.clear();
                self.expression_scheduled = false;
                self.expression_jobs.clear();
                self.queued_media.clear();
                self.queued_gaps.clear();
                self.scheduler.reset();
                self.account = account;
                self.session_generation = generation;
                self.transport.session(&self.account, generation);
                self.store.set_account(&self.account);
                if self.account.is_empty() {
                    return Ok(true);
                }
                self.store.import_legacy(&self.legacy)?;
                self.restore_expression_jobs(generation)?;
            }
            Command::Fetch {
                id,
                chat,
                before,
                generation,
            } => {
                if self.scheduler.enabled {
                    self.scheduler.pages.push_back(scheduler::PageJob {
                        id,
                        chat,
                        before,
                        generation,
                        gap: None,
                    });
                    return Ok(true);
                }
                let result = self
                    .fetch(&chat, before.as_ref(), generation)
                    .and_then(|mut page| {
                        self.queue_media(&chat, &page.messages, generation);
                        if before.is_none() {
                            self.queue_gap(&chat, generation);
                        }
                        for row in &mut page.messages {
                            if self.transport.memory_media(&self.account, &chat)
                                && matches!(row["contentType"].as_str(), Some("IMAGE" | "VIDEO"))
                            {
                                row["imagePath"] = Value::Null;
                                row["filePath"] = Value::Null;
                                row["needsMedia"] = json!(true);
                            }
                            if let Some(object) = row.as_object_mut() {
                                object.remove("rawMessage");
                                for key in ["imagePath", "audioPath", "filePath"] {
                                    if object
                                        .get(key)
                                        .and_then(Value::as_str)
                                        .is_some_and(|p| !std::path::Path::new(p).is_file())
                                    {
                                        object.insert(key.into(), Value::Null);
                                    }
                                }
                            }
                        }
                        Ok(serde_json::to_value(page)?)
                    });
                self.response(id, result, generation);
            }
            Command::Download {
                id,
                chat,
                message,
                generation,
            } => {
                if self.scheduler.enabled {
                    let result = self.store.message(&chat, &message)?;
                    if let Some(row) = result {
                        self.scheduler
                            .downloads
                            .push_back((id, chat, row, generation));
                    } else {
                        self.response(id, Err(anyhow::anyhow!("message unavailable")), generation);
                    }
                    return Ok(true);
                }
                let result = (|| {
                    self.valid(generation)?;
                    let row = self
                        .store
                        .message(&chat, &message)?
                        .ok_or_else(|| anyhow::anyhow!("message unavailable"))?;
                    let row = self.media(&chat, row, generation)?;
                    Ok(
                        json!({"path":row["filePath"],"fileName":row["fileName"],"contentType":row["contentType"]}),
                    )
                })();
                self.response(id, result, generation);
            }
            Command::Live {
                chat,
                mut message,
                generation,
            } => {
                self.valid(generation)?;
                message = crate::message::from_envelope(&message, &self.account);
                self.store
                    .save_messages(&chat, std::slice::from_ref(&message))?;
                self.decorate_receipts(&chat, std::slice::from_mut(&mut message))?;
                self.store.invalidate(&chat)?;
                self.store.invalidate_catalogs()?;
                let mut visible = message.clone();
                if self.transport.memory_media(&self.account, &chat)
                    && matches!(visible["contentType"].as_str(), Some("IMAGE" | "VIDEO"))
                {
                    visible["imagePath"] = Value::Null;
                    visible["filePath"] = Value::Null;
                }
                if let Some(obj) = visible.as_object_mut() {
                    obj.remove("rawMessage");
                }
                self.transport
                    .emit(json!({"event":"message","chatMid":chat,"message":visible}));
                self.queue_media(&chat, &[message], generation);
            }
            Command::Unsend {
                chat,
                message,
                generation,
            } => {
                self.valid(generation)?;
                self.store.unsend(&chat, &message)?;
            }
            Command::Invalidate { chat, generation } => {
                self.valid(generation)?;
                self.store.invalidate(&chat)?;
            }
            Command::Clear { id, generation } => {
                let result = (|| {
                    self.valid(generation)?;
                    self.scheduler.clear_work();
                    self.background.clear();
                    self.expression_scheduled = false;
                    self.queued_media.clear();
                    self.queued_gaps.clear();
                    self.store.clear()?;
                    self.clear_receipts();
                    self.schedule_expressions(generation);
                    Ok(json!({"cleared":"messages"}))
                })();
                self.response(id, result, generation);
            }
        }
        Ok(true)
    }

    fn background(&mut self, work: Work) -> Result<()> {
        match work {
            Work::Receipts { chat, generation } => {
                self.queued_receipts.remove(&chat);
                self.sync_receipts(&chat, generation)?;
            }
            Work::Expression { generation } => {
                self.expression_scheduled = false;
                self.expression_step(generation)?;
            }
            Work::Media {
                chat,
                row,
                generation,
            } => {
                self.queued_media
                    .remove(&(chat.clone(), id(&row).to_owned()));
                if self.transport.memory_media(&self.account, &chat)
                    && matches!(row["contentType"].as_str(), Some("IMAGE" | "VIDEO"))
                    && !self.transport.media_visible(&chat, id(&row))
                {
                    return Ok(());
                }
                if let Err(error) = self.media(&chat, row.clone(), generation) {
                    if self.valid(generation).is_ok() {
                        self.transport.emit(
                            json!({"event":"media_failed","chatMid":chat,"messageId":id(&row)}),
                        );
                    }
                    return Err(error);
                }
            }
            Work::Gap { chat, generation } => {
                self.queued_gaps.remove(&chat);
                if self.transport.memory_media(&self.account, &chat) {
                    return Ok(());
                }
                self.valid(generation)?;
                if let Some(gap) = self.store.gap(&chat)? {
                    let page = self.fetch(&chat, Some(&gap.before), generation)?;
                    let done = gap.before.message_id == gap.target_id
                        || page.messages.iter().any(|m| id(m) == gap.target_id)
                        || !page.has_more
                        || page.next_cursor.is_none()
                        || page.next_cursor.as_ref() == Some(&gap.before);
                    self.queue_media(&chat, &page.messages, generation);
                    self.background.push_back(Work::Advance {
                        chat,
                        gap,
                        next: if done { None } else { page.next_cursor },
                        generation,
                    });
                }
            }
            Work::Advance {
                chat,
                gap,
                next,
                generation,
            } => {
                self.valid(generation)?;
                // All page media jobs are ahead of this checkpoint in the FIFO.
                if self.transport.memory_media(&self.account, &chat) {
                    return Ok(());
                }
                self.store.advance(&chat, &gap, next.as_ref())?;
                self.last_request = Some(Instant::now());
                self.queue_gap(&chat, generation);
            }
        }
        Ok(())
    }

    pub fn run(mut self, rx: mpsc::Receiver<Command>) {
        loop {
            self.finish_async();
            if self.receipts_flush_at.elapsed() >= Duration::from_secs(10) {
                if let Err(error) = self.flush_receipts() {
                    eprintln!("[core receipts] {error}");
                }
                self.receipts_flush_at = Instant::now();
            }
            match rx.try_recv() {
                Ok(command) => match self.command(command) {
                    Ok(false) => break,
                    Err(e) => eprintln!("[core] {e}"),
                    _ => {}
                },
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {
                    if self.generation.load(Ordering::SeqCst) != self.session_generation {
                        self.background.clear();
                        self.expression_scheduled = false;
                        self.queued_media.clear();
                        self.queued_gaps.clear();
                        self.queued_receipts.clear();
                    }
                    if self.scheduler.enabled && self.start_async() {
                        continue;
                    }
                    let work = if self.scheduler.enabled {
                        self.next_background()
                    } else {
                        self.background.pop_front()
                    };
                    if let Some(work) = work {
                        if let Err(e) = self.background(work) {
                            eprintln!("[core background] {e}");
                        }
                    } else {
                        match rx.recv_timeout(if self.scheduler.enabled {
                            Duration::from_millis(10)
                        } else {
                            Duration::from_secs(1)
                        }) {
                            Ok(command) => match self.command(command) {
                                Ok(false) => break,
                                Err(e) => eprintln!("[core] {e}"),
                                _ => {}
                            },
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                }
            }
        }
        if let Err(error) = self.flush_receipts() {
            eprintln!("[core receipts] {error}");
        }
    }
}
