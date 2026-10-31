use crate::store::{Cursor, Gap, Page, Store, id};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

/// Implemented by the host's IPC transport; no GTK or JavaScript scheduling here.
pub trait Transport: Send + 'static {
    fn call(&self, method: &str, params: Value, generation: u64) -> Result<Value>;
    fn materialize(&self, message: &Value, generation: u64) -> Result<Value>;
    fn emit(&self, event: Value);
    fn application(&self, method: &str, params: Value, generation: u64) -> Result<Value> {
        self.call(method, params, generation)
    }
}

pub enum Command {
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
    Stop,
}

enum Work {
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
    transport: T,
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
            transport,
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
        if let Some(page) = self.store.page(chat, before, self.policy.messages)? {
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
            if let Some(raw) = row.get("decodedMessage") {
                let mut normalized = crate::message::normalize(raw, &self.account);
                normalized["rawMessage"] = row["rawMessage"].clone();
                *row = normalized;
            }
        }
        if page.messages.len() > 20 {
            bail!("protocol returned oversized history page");
        }
        if let Some(cursor) = &page.next_cursor {
            cursor.validate()?;
        }
        self.store.save_page(chat, before, &page)
    }

    fn queue_media(&mut self, chat: &str, rows: &[Value], generation: u64) {
        for row in rows.iter().rev() {
            if matches!(
                row["contentType"].as_str(),
                Some("IMAGE" | "VIDEO" | "AUDIO" | "STICKER")
            ) && self
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
        let Some(current) = self.store.message(chat, id(&row))? else {
            bail!("message unavailable");
        };
        let result = self.transport.materialize(&current, generation);
        self.last_media = Some(Instant::now());
        self.valid(generation)?;
        let updated = result?;
        self.store
            .save_messages(chat, std::slice::from_ref(&updated))?;
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
            Command::Operation {
                id,
                method,
                mut params,
                generation,
            } => {
                if method == "mark_read" {
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
                    if matches!(method.as_str(), "mute_chat" | "clear_cache") {
                        self.store.invalidate_catalogs()?;
                    }
                    if method == "unsend_message" {
                        self.store.unsend(
                            params["chatMid"].as_str().unwrap_or_default(),
                            params["messageId"].as_str().unwrap_or_default(),
                        )?;
                    }
                    if method == "clear_cache"
                        && matches!(params["kind"].as_str(), Some("all" | "messages"))
                    {
                        self.background.clear();
                        self.queued_media.clear();
                        self.queued_gaps.clear();
                        self.store.clear()?;
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
                self.background.clear();
                self.queued_media.clear();
                self.queued_gaps.clear();
                self.account = account;
                self.session_generation = generation;
                self.store.set_account(&self.account);
                if self.account.is_empty() {
                    return Ok(true);
                }
                self.store.import_legacy(&self.legacy)?;
            }
            Command::Fetch {
                id,
                chat,
                before,
                generation,
            } => {
                let result = self
                    .fetch(&chat, before.as_ref(), generation)
                    .and_then(|mut page| {
                        self.queue_media(&chat, &page.messages, generation);
                        if before.is_none() {
                            self.queue_gap(&chat, generation);
                        }
                        for row in &mut page.messages {
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
                if let Some(raw) = message.get("decodedMessage") {
                    let mut normalized = crate::message::normalize(raw, &self.account);
                    normalized["rawMessage"] = message["rawMessage"].clone();
                    message = normalized;
                }
                self.store
                    .save_messages(&chat, std::slice::from_ref(&message))?;
                self.store.invalidate(&chat)?;
                self.store.invalidate_catalogs()?;
                let mut visible = message.clone();
                if let Some(obj) = visible.as_object_mut() {
                    obj.remove("rawMessage");
                }
                self.transport
                    .emit(json!({"event":"message","message":visible}));
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
                    self.background.clear();
                    self.queued_media.clear();
                    self.queued_gaps.clear();
                    self.store.clear()?;
                    Ok(json!({"cleared":"messages"}))
                })();
                self.response(id, result, generation);
            }
        }
        Ok(true)
    }

    fn background(&mut self, work: Work) -> Result<()> {
        match work {
            Work::Media {
                chat,
                row,
                generation,
            } => {
                self.queued_media
                    .remove(&(chat.clone(), id(&row).to_owned()));
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
                self.store.advance(&chat, &gap, next.as_ref())?;
                self.last_request = Some(Instant::now());
                self.queue_gap(&chat, generation);
            }
        }
        Ok(())
    }

    pub fn run(mut self, rx: mpsc::Receiver<Command>) {
        loop {
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
                        self.queued_media.clear();
                        self.queued_gaps.clear();
                    }
                    if let Some(work) = self.background.pop_front() {
                        if let Err(e) = self.background(work) {
                            eprintln!("[core background] {e}");
                        }
                    } else {
                        match rx.recv() {
                            Ok(command) => match self.command(command) {
                                Ok(false) => break,
                                Err(e) => eprintln!("[core] {e}"),
                                _ => {}
                            },
                            Err(_) => break,
                        }
                    }
                }
            }
        }
    }
}
