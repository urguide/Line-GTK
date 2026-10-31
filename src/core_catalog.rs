//! Rust-owned contact/chat/sticker presentation, CDN files and outgoing media staging.
use crate::core_bridge::Ipc;
use anyhow::{Context, Result, bail};
use line_gtk_core::{
    message::{integer, string},
    sync::Transport,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn numeric(s: &str) -> Result<&str> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid numeric ID");
    }
    Ok(s)
}
fn picture(row: &Value) -> Option<&str> {
    row["picturePath"]
        .as_str()
        .or_else(|| row["pictureStatus"].as_str())
        .or_else(|| row["extra"]["groupExtra"]["picturePath"].as_str())
}

impl Ipc {
    fn asset(&self, url: &str, dest: &Path, generation: u64) -> Result<PathBuf> {
        self.active(generation)?;
        if dest.is_file() {
            return Ok(dest.to_owned());
        }
        {
            let mut previous = self
                .asset_at
                .lock()
                .map_err(|_| anyhow::anyhow!("asset lock"))?;
            if let Some(last) = *previous {
                while last.elapsed() < std::time::Duration::from_millis(500) {
                    self.active(generation)?;
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
            *previous = Some(std::time::Instant::now());
        }
        let parsed = reqwest::Url::parse(url)?;
        if parsed.scheme() != "https"
            || !parsed
                .host_str()
                .is_some_and(|host| host.ends_with(".line-scdn.net"))
        {
            bail!("invalid LINE CDN URL");
        }
        if let Some(parent) = dest.parent() {
            crate::config::ensure_private_dir(parent)?;
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut response = client.get(url).send()?.error_for_status()?;
        let temporary = dest.with_extension("download");
        let result = (|| {
            let mut file = std::fs::File::create(&temporary)?;
            response.copy_to(&mut file)?;
            file.sync_all()?;
            self.active(generation)?;
            std::fs::rename(&temporary, dest)?;
            Ok(dest.to_owned())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }
    fn avatar(&self, mid: &str, path: Option<&str>, generation: u64) -> Option<PathBuf> {
        if !mid.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let path = path?;
        let url = if path.starts_with("https://") {
            path.to_owned()
        } else {
            format!(
                "https://profile.line-scdn.net/{}",
                path.trim_start_matches('/')
            )
        };
        self.asset(
            &url,
            &self.data.join("cache/avatars").join(format!("{mid}.jpg")),
            generation,
        )
        .ok()
    }
    fn sticker(&self, id: &str, animated: bool, generation: u64) -> Result<PathBuf> {
        numeric(id)?;
        if animated {
            let path = self
                .data
                .join("cache/stickers")
                .join(format!("{id}.anim.png"));
            if let Ok(file)=self.asset(&format!("https://stickershop.line-scdn.net/stickershop/v1/sticker/{id}/android/sticker_animation.png"),&path,generation){return Ok(file);}
        }
        self.asset(
            &format!(
                "https://stickershop.line-scdn.net/stickershop/v1/sticker/{id}/android/sticker.png"
            ),
            &self.data.join("cache/stickers").join(format!("{id}.png")),
            generation,
        )
    }
    fn catalogs(&self, method: &str, generation: u64) -> Result<Value> {
        let mut chats = BTreeMap::<String, Value>::new();
        if method == "list_chats" {
            let boxes = self.call("bridge_boxes", json!({}), generation)?;
            for row in boxes["messageBoxes"].as_array().into_iter().flatten() {
                let mid = string(&row["id"]);
                chats.insert(mid.clone(),json!({"mid":mid,"name":mid,"kind":if mid.starts_with('c'){"group"}else{"dm"},"lastActivity":integer(&row["lastDeliveredMessageId"]["deliveredTime"]),"unread":integer(&row["unreadCount"]),"preview":""}));
            }
        }
        let ids = self.call("bridge_contact_ids", json!({}), generation)?;
        let mut mids: Vec<Value> = ids.as_array().cloned().unwrap_or_default();
        for mid in chats.keys().filter(|mid| mid.starts_with('u')) {
            if !mids.iter().any(|v| v.as_str() == Some(mid)) {
                mids.push(json!(mid));
            }
        }
        for chunk in mids.chunks(100) {
            let contacts = self.call("bridge_contacts", json!({"mids":chunk}), generation)?;
            for row in contacts.as_array().into_iter().flatten() {
                let mid = string(&row["mid"]);
                if mid.is_empty() {
                    continue;
                }
                let mut entry = chats
                    .remove(&mid)
                    .unwrap_or(json!({"mid":mid,"lastActivity":0,"unread":0,"preview":""}));
                entry["name"] = json!(row["displayName"].as_str().unwrap_or(&mid));
                entry["kind"] = json!(if string(&row["type"]).contains("BOT") {
                    "bot"
                } else {
                    "dm"
                });
                entry["muted"] = json!(row["notificationDisabled"].as_bool().unwrap_or(false));
                entry["avatarPath"] = json!(self.avatar(&mid, picture(row), generation));
                chats.insert(mid, entry);
            }
        }
        if method == "list_chats" {
            let groups = self.call("bridge_groups", json!({}), generation)?;
            for row in groups.as_array().into_iter().flatten() {
                let mid = string(&row["chatMid"]);
                if mid.is_empty() {
                    continue;
                }
                let mut entry = chats
                    .remove(&mid)
                    .unwrap_or(json!({"mid":mid,"lastActivity":0,"unread":0,"preview":""}));
                entry["name"] = json!(row["chatName"].as_str().unwrap_or(&mid));
                entry["kind"] = json!("group");
                entry["avatarPath"] = json!(self.avatar(&mid, picture(row), generation));
                chats.insert(mid, entry);
            }
        }
        let mut rows: Vec<_> = chats.into_values().collect();
        rows.sort_by(|a, b| {
            integer(&b["lastActivity"])
                .cmp(&integer(&a["lastActivity"]))
                .then_with(|| string(&a["name"]).cmp(&string(&b["name"])))
        });
        Ok(if method == "list_chats" {
            json!({"chats":rows,"cached":false})
        } else {
            json!({"friends":rows,"cached":false})
        })
    }
    fn sticker_catalog(&self, p: &Value, generation: u64) -> Result<Value> {
        let owned = self.call("bridge_owned_stickers", json!({}), generation)?;
        let products = owned["productList"]
            .as_array()
            .or_else(|| owned["1"].as_array())
            .cloned()
            .unwrap_or_default();
        let mut packs = Vec::new();
        let mut recent = Vec::new();
        for row in p["recent"].as_array().into_iter().flatten().take(24) {
            let package = string(&row["packageId"]);
            if !products.iter().any(|v| string(&v["id"]) == package) {
                continue;
            }
            let mut row = row.clone();
            row["imagePath"] = json!(
                self.sticker(&string(&row["stickerId"]), false, generation)
                    .ok()
            );
            recent.push(row);
        }
        if !recent.is_empty() {
            packs.push(json!({"id":"__recent__","name":"Recent","recent":true,"iconPath":recent[0]["imagePath"],"stickers":recent}));
        }
        for product in products.iter().take(12) {
            let package = string(&product["id"]);
            numeric(&package)?;
            let meta_path = self
                .data
                .join("cache/stickers")
                .join(format!("pack-{package}.json"));
            let path=self.asset(&format!("https://stickershop.line-scdn.net/stickershop/v1/product/{package}/android/productInfo.meta"),&meta_path,generation)?;
            let meta: Value = serde_json::from_slice(&std::fs::read(path)?)?;
            let mut stickers = Vec::new();
            for sticker in meta["stickers"].as_array().into_iter().flatten().take(48) {
                let id = string(&sticker["id"]);
                stickers.push(json!({"stickerId":id,"packageId":package,"version":string(&meta["version"]),"imagePath":self.sticker(&id,false,generation).ok()}));
            }
            packs.push(json!({"id":package,"name":meta["title"]["en"].as_str().or_else(||product["name"].as_str()).unwrap_or(&package),"version":string(&meta["version"]),"iconPath":stickers.first().map(|s|&s["imagePath"]),"recent":false,"stickers":stickers}));
        }
        Ok(json!({"packs":packs,"ownedPackages":products.len()}))
    }
    fn send_attachment(&self, p: &Value, generation: u64) -> Result<Value> {
        let source = Path::new(p["filePath"].as_str().context("missing attachment")?);
        if !source.is_file() {
            bail!("attachment is not a regular file");
        }
        let filename = source
            .file_name()
            .context("missing filename")?
            .to_string_lossy();
        let ext = source
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let kind = match p["oType"].as_str().unwrap_or("auto") {
            "auto" => match ext.as_str() {
                "gif" => "gif",
                "jpg" | "jpeg" | "png" | "webp" => "image",
                "mp4" | "webm" | "mov" | "mkv" => "video",
                "m4a" | "mp3" | "ogg" | "wav" => "audio",
                _ => "file",
            },
            other => other,
        };
        let dir = self.data.join("cache/transfer");
        crate::config::ensure_private_dir(&dir)?;
        let token = self.ids.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let name = format!("upload-{token}.{ext}");
        let staged = dir.join(&name);
        std::fs::copy(source, &staged)?;
        let preview_name = format!("preview-{token}.jpg");
        let preview = dir.join(&preview_name);
        let result = (|| {
            if matches!(kind, "image" | "gif" | "video") {
                self.thumb(&staged, &preview)?;
            }
            let mut duration = p["durationMs"].as_i64();
            if duration.is_none() && matches!(kind, "audio" | "video") {
                let output = Command::new("ffprobe")
                    .args([
                        "-v",
                        "error",
                        "-show_entries",
                        "format=duration",
                        "-of",
                        "default=nw=1:nk=1",
                    ])
                    .arg(&staged)
                    .output()?;
                duration = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .map(|s| (s * 1000.0) as i64);
            }
            let mut result=self.call("bridge_send_media",json!({"chatMid":p["chatMid"],"transferName":name,"previewName":if preview.is_file(){Some(&preview_name)}else{None},"fileName":filename,"extension":ext,"oType":kind,"durationMs":duration}),generation)?;
            result["rawMessage"]["from"] = p["account"].clone();
            result["rawMessage"]["to"] = p["chatMid"].clone();
            result["rawMessage"]["contentType"] = json!(if kind == "gif" {
                "IMAGE".to_owned()
            } else {
                kind.to_ascii_uppercase()
            });
            result["rawMessage"]["contentMetadata"]["FILE_NAME"] = json!(filename);
            let id = string(&result["rawMessage"]["id"]);
            numeric(&id)?;
            let dest = self.data.join("cache/media").join(format!("{id}.{ext}"));
            crate::config::ensure_private_dir(dest.parent().unwrap())?;
            std::fs::rename(&staged, &dest)?;
            result["filePath"] = json!(dest);
            result["fileName"] = json!(filename);
            if preview.is_file() {
                let thumb = self
                    .data
                    .join("cache/media")
                    .join(format!("{id}.thumb.jpg"));
                std::fs::rename(&preview, &thumb)?;
                result["imagePath"] = json!(thumb);
            }
            if kind == "audio" {
                result["audioPath"] = result["filePath"].clone();
            }
            Ok(result)
        })();
        let _ = std::fs::remove_file(staged);
        let _ = std::fs::remove_file(preview);
        result
    }
    pub(crate) fn application_operation(
        &self,
        method: &str,
        mut p: Value,
        generation: u64,
    ) -> Result<Value> {
        self.active(generation)?;
        match method {
            "list_chats" | "list_friends" => self.catalogs(method, generation),
            "list_stickers" => self.sticker_catalog(&p, generation),
            "send_media" | "send_audio" => {
                if method == "send_audio" {
                    p["oType"] = json!("audio");
                }
                self.send_attachment(&p, generation)
            }
            "send_message" => {
                p["e2ee"] = json!(true);
                let mut result = match self.call("bridge_send_text", p.clone(), generation) {
                    Ok(v) => Ok(v),
                    Err(_) => {
                        p["e2ee"] = json!(false);
                        self.call("bridge_send_text", p.clone(), generation)
                    }
                }?;
                result["rawMessage"]["from"] = p["account"].clone();
                result["rawMessage"]["to"] = p["chatMid"].clone();
                result["rawMessage"]["text"] = p["text"].clone();
                Ok(result)
            }
            "send_sticker" => self.call("bridge_send_sticker", p, generation),
            "mark_read" => self.call("bridge_mark_read", p, generation),
            "unsend_message" => {
                let result = self.call("bridge_unsend", p, generation)?;
                self.emit(json!({"event":"message_unsent","chatMid":result["chatMid"],"messageId":result["messageId"]}));
                Ok(result)
            }
            "mute_chat" => self.call("bridge_mute", p, generation),
            "send_postback" => self.call("bridge_postback", p, generation),
            "add_friend" => {
                let row = self.call("bridge_find_contact", p.clone(), generation)?;
                p["mid"] = row["mid"].clone();
                self.call("bridge_friend_request", p, generation)?;
                Ok(row)
            }
            "clear_cache" => {
                let kind = p["kind"].as_str().unwrap_or("all");
                for name in ["media", "stickers", "avatars"] {
                    if kind == "all" || kind == name {
                        let directory = self.data.join("cache").join(name);
                        if directory.is_dir() {
                            std::fs::remove_dir_all(directory)?;
                        }
                    }
                }
                Ok(json!({"cleared":kind}))
            }
            _ => bail!("unknown application operation: {method}"),
        }
    }
    pub(crate) fn sticker_media(&self, row: &Value, generation: u64) -> Result<Value> {
        let path = self.sticker(&string(&row["stickerId"]), true, generation)?;
        let mut updated = row.clone();
        updated["filePath"] = json!(path);
        updated["imagePath"] = updated["filePath"].clone();
        Ok(updated)
    }
}
