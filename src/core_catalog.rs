//! Rust-owned contact/chat/sticker presentation, CDN files and outgoing media staging.
use crate::core_bridge::Ipc;
use anyhow::{Context, Result, bail};
use line_rust_core::{message::string, sync::Transport};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

fn numeric(s: &str) -> Result<&str> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid numeric ID");
    }
    Ok(s)
}

impl Ipc {
    fn expression_library(&self, p: &Value) -> Result<Value> {
        let mut packs = p["installed"]["packs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        // Recheck files on every open; clearing the cache must never leave broken buttons.
        for pack in &mut packs {
            let complete = pack["stickers"].as_array().is_some_and(|items| {
                !items.is_empty()
                    && items.iter().all(|s| {
                        s["imagePath"]
                            .as_str()
                            .is_some_and(|path| Path::new(path).is_file())
                    })
            });
            pack["downloaded"] = json!(complete);
        }
        let mut products = p["products"].as_array().cloned().unwrap_or_default();
        for product in &mut products {
            let package = string(&product["id"]);
            if line_rust_core::expressions::valid_id(&package) {
                let icon = if p["kind"] == "emoji" {
                    self.data
                        .join("cache/stickers/emoji")
                        .join(&package)
                        .join("main.png")
                } else {
                    self.data
                        .join("cache/stickers")
                        .join(format!("pack-{package}.png"))
                };
                product["iconPath"] = if icon.is_file() {
                    json!(icon)
                } else {
                    packs
                        .iter()
                        .find(|pack| pack["id"] == product["id"])
                        .map(|pack| pack["iconPath"].clone())
                        .unwrap_or(Value::Null)
                };
            }
            product["downloaded"] = json!(
                packs
                    .iter()
                    .any(|p| p["id"] == product["id"] && p["downloaded"] == true)
            );
        }
        packs.retain(|p| p["downloaded"] == true);
        let mut recents = Vec::new();
        for recent in p["recent"].as_array().into_iter().flatten() {
            if let Some(item) = packs
                .iter()
                .find(|p| p["id"] == recent["packageId"])
                .and_then(|p| p["stickers"].as_array())
                .and_then(|items| items.iter().find(|s| s["stickerId"] == recent["stickerId"]))
            {
                recents.push(item.clone());
                if recents.len() == 24 {
                    break;
                }
            }
        }
        if !recents.is_empty() {
            packs.insert(
                0,
                json!({"id":"__recent__","recent":true,"name":"Recent","stickers":recents}),
            );
        }
        Ok(json!({"packs":packs,"products":products,"kind":p["kind"]}))
    }

    fn download_expression(&self, p: &Value, generation: u64) -> Result<Value> {
        use line_rust_core::expressions;
        let product = &p["product"];
        let package = string(&product["id"]);
        if !expressions::valid_id(&package) {
            bail!("invalid package ID");
        }
        if p["kind"] == "emoji" {
            let root = self.data.join("cache/stickers/emoji").join(&package);
            let meta_path = root.join("meta.json");
            if meta_path.is_file()
                && std::fs::read(&meta_path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|v| expressions::emoji_pack(&v, product).ok())
                    .is_none()
            {
                std::fs::remove_file(&meta_path)?;
            }
            self.asset(&format!("https://stickershop.line-scdn.net/sticonshop/v1/{package}/sticon/iphone/meta.json"), &meta_path, generation)?;
            let meta = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
            let mut pack = expressions::emoji_pack(&meta, product)?;
            for item in pack["stickers"].as_array_mut().unwrap() {
                let id = string(&item["stickerId"]);
                let path = self.asset(&format!("https://stickershop.line-scdn.net/sticonshop/v1/sticon/{package}/android/{id}.png"), &root.join(format!("{id}.png")), generation)?;
                item["imagePath"] = json!(path);
            }
            pack["iconPath"] = pack["stickers"][0]["imagePath"].clone();
            pack["downloaded"] = json!(true);
            return Ok(pack);
        }
        let result = self.sticker_catalog(
            &json!({"owned":[product],"previous":{},"recent":[]}),
            generation,
        )?;
        if result["partial"] == true {
            bail!("package download incomplete; retry to resume");
        }
        let mut pack = result["packs"]
            .as_array()
            .and_then(|packs| packs.first())
            .cloned()
            .context("empty sticker package")?;
        pack["downloaded"] = json!(true);
        Ok(pack)
    }

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
        use line_rust_core::sticker_cache::{self, Fetch};
        self.active(generation)?;
        let cfg = crate::config::AppConfig::load(&self.data);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        let result = sticker_cache::load(
            &self.data.join("cache/stickers"),
            id,
            animated,
            sticker_cache::animation_miss_ttl(&cfg.cache_retention),
            now,
            |animation, dest| {
                sticker_cache::fetch_candidates(id, animation, |url| {
                    match self.asset(url, dest, generation) {
                        Ok(_) => Ok(Fetch::Saved),
                        Err(error)
                            if error
                                .downcast_ref::<reqwest::Error>()
                                .and_then(reqwest::Error::status)
                                .is_some_and(|status| {
                                    sticker_cache::confirms_absence(status.as_u16())
                                }) =>
                        {
                            Ok(Fetch::Missing)
                        }
                        Err(error) => Err(error),
                    }
                })
            },
        );
        self.active(generation)?;
        result
    }
    fn catalogs(&self, method: &str, previous: &Value, generation: u64) -> Result<Value> {
        let mut result = line_rust_core::catalog::load(method, previous, |rpc, args| {
            self.call(rpc, args, generation)
        })?;
        self.active(generation)?;
        let key = if method == "list_friends" {
            "friends"
        } else {
            "chats"
        };
        for row in result[key].as_array_mut().into_iter().flatten() {
            if let Some(path) = self.avatar(
                &string(&row["mid"]),
                line_rust_core::catalog::picture(row),
                generation,
            ) {
                row["avatarPath"] = json!(path);
            }
        }
        self.active(generation)?;
        Ok(result)
    }
    fn sticker_catalog(&self, p: &Value, generation: u64) -> Result<Value> {
        let owned = if p["owned"].is_null() {
            self.call("bridge_owned_stickers", json!({}), generation)
        } else if p["owned"].is_array() {
            Ok(json!({"productList":p["owned"]}))
        } else {
            Ok(p["owned"].clone())
        };
        self.active(generation)?;
        let mut result = line_rust_core::stickers::load(
            owned,
            &p["previous"],
            &p["recent"],
            |package| {
                self.active(generation)?;
                let path = self
                    .data
                    .join("cache/stickers")
                    .join(format!("pack-{package}.json"));
                // A damaged local file must not permanently poison future refreshes.
                if path.is_file()
                    && std::fs::read(&path)
                        .ok()
                        .and_then(|b| line_rust_core::stickers::metadata(&b).ok())
                        .is_none()
                {
                    std::fs::remove_file(&path)?;
                }
                let downloaded=self.asset(&format!("https://stickershop.line-scdn.net/stickershop/v1/product/{package}/android/productInfo.meta"),&path,generation)?;
                let bytes = std::fs::read(downloaded)?;
                if let Err(error) = line_rust_core::stickers::metadata(&bytes) {
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
                Ok(bytes)
            },
            |id| {
                self.active(generation)?;
                Ok(self
                    .sticker(id, false, generation)?
                    .to_string_lossy()
                    .into_owned())
            },
        )?;
        line_rust_core::stickers::apply_pack_icons(&mut result, |package| {
            let path = line_rust_core::sticker_cache::pack_icon(
                &self.data.join("cache/stickers"),
                package,
                |url, dest| {
                    self.active(generation)?;
                    self.asset(url, dest, generation)?;
                    Ok(line_rust_core::sticker_cache::Fetch::Saved)
                },
            )?;
            Ok(path.to_string_lossy().into_owned())
        });
        self.active(generation)?;
        Ok(result)
    }
    fn send_attachment(&self, p: &Value, generation: u64) -> Result<Value> {
        self.send_attachment_with_preview(p, generation, |source, destination| {
            self.thumb(source, destination)
        })
    }

    fn send_memory_attachment(&self, p: &Value, kind: &str, generation: u64) -> Result<Value> {
        use base64::Engine as _;
        use std::io::Read;
        let source = p["filePath"].as_str().context("missing attachment")?;
        let _clipboard = crate::memory_media::take_upload(source);
        let original = crate::memory_media::RamFile::new()?;
        std::io::copy(&mut std::fs::File::open(source)?, &mut &*original.0)?;
        let preview = crate::memory_media::preview(&original).ok();
        let started = self.call("bridge_memory_upload_start", json!({}), generation)?;
        let token = started["token"]
            .as_str()
            .context("missing memory upload token")?;
        let result = (|| {
            let mut input = std::fs::File::open(original.path())?;
            let mut buffer = vec![0; 256 * 1024];
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                self.call(
                    "bridge_memory_upload_chunk",
                    json!({"token":token,
                    "data":base64::engine::general_purpose::STANDARD.encode(&buffer[..n])}),
                    generation,
                )?;
            }
            let filename = if crate::memory_media::is_ram_path(source) {
                "image.png"
            } else {
                Path::new(source)
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("media.bin")
            };
            let ext = line_rust_core::attachments::file_extension(Some(filename), "");
            let preview_data = preview
                .as_ref()
                .map(|file| std::fs::read(file.path()))
                .transpose()?
                .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));
            let params = json!({"chatMid":p["chatMid"],"memoryToken":token,"previewData":preview_data,
                "fileName":filename,"extension":ext,"mime":line_rust_core::attachments::mime(&ext,kind),
                "oType":kind,"durationMs":p["durationMs"]});
            let mut result = line_rust_core::attachments::upload(
                &params,
                |rpc, args| self.call(rpc, args, generation),
                || self.active(generation),
            )?;
            if let Some(id) = result["objId"].as_str() {
                result = json!({"rawMessage":{"id":id,"createdTime":std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64}});
            }
            result["rawMessage"]["from"] = p["account"].clone();
            result["rawMessage"]["to"] = p["chatMid"].clone();
            result["rawMessage"]["contentType"] =
                json!(if kind == "video" { "VIDEO" } else { "IMAGE" });
            if !result["rawMessage"]["contentMetadata"].is_object() {
                result["rawMessage"]["contentMetadata"] = json!({});
            }
            result["rawMessage"]["contentMetadata"]["FILE_NAME"] = json!(filename);
            result["fileName"] = json!(filename);
            // The sent message is persisted without any process-local paths.
            // Its visible row uses the same lazy memory download path as incoming media.
            Ok(result)
        })();
        let _ = self.call(
            "bridge_memory_upload_release",
            json!({"token":token}),
            generation,
        );
        result
    }

    pub(crate) fn send_attachment_with_preview(
        &self,
        p: &Value,
        generation: u64,
        make_preview: impl FnOnce(&Path, &Path) -> Result<()>,
    ) -> Result<Value> {
        self.send_attachment_with_tools(p, generation, make_preview, Path::new("ffprobe"))
    }

    pub(crate) fn send_attachment_with_tools(
        &self,
        p: &Value,
        generation: u64,
        make_preview: impl FnOnce(&Path, &Path) -> Result<()>,
        probe_program: &Path,
    ) -> Result<Value> {
        let chat = p["chatMid"].as_str().unwrap_or_default();
        let progress = |fraction, label| -> Result<()> {
            self.active(generation)?;
            self.emit(line_rust_core::attachments::upload_progress(
                chat, fraction, label, false,
            ));
            Ok(())
        };
        let method =
            line_rust_core::attachments::upload_method(p["chatMid"].as_str().unwrap_or_default())?;
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
        let kind = line_rust_core::attachments::normalize_media_type(
            p["oType"].as_str().unwrap_or("auto"),
            &filename,
        );
        if matches!(kind, "image" | "gif" | "video")
            && crate::memory_media::for_data(&self.data).current_enabled(chat)
        {
            return self.send_memory_attachment(p, kind, generation);
        }
        // Validate the destination before uploading anything to the server.
        let content_type = if kind == "gif" {
            "IMAGE".to_owned()
        } else {
            kind.to_ascii_uppercase()
        };
        self.media_cache_layout(&json!({"id":"0","chatMid":chat,"contentType":content_type}))?;
        let dir = self.data.join("cache/transfer");
        crate::config::ensure_private_dir(&dir)?;
        let token = self.ids.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let name = format!("upload-{token}.{ext}");
        let staged = dir.join(&name);
        progress(0.05, "Reading file…")?;
        std::fs::copy(source, &staged)?;
        let preview_name = format!("preview-{token}.jpg");
        let preview = dir.join(&preview_name);
        let result = (|| {
            progress(0.10, "Preparing preview…")?;
            let preview_ready = if matches!(kind, "image" | "gif" | "video") {
                match make_preview(&staged, &preview).and_then(|()| {
                    if !preview.metadata()?.is_file() || preview.metadata()?.len() == 0 {
                        bail!("empty or invalid upload preview");
                    }
                    Ok(())
                }) {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(
                            "upload preview unavailable; continuing with original: {error}"
                        );
                        // Never pass a partial output to linejs, even if cleanup fails.
                        if let Err(cleanup) = std::fs::remove_file(&preview)
                            && cleanup.kind() != std::io::ErrorKind::NotFound
                        {
                            tracing::warn!("upload preview cleanup: {cleanup}");
                        }
                        false
                    }
                }
            } else {
                false
            };
            let duration = if matches!(kind, "audio" | "video") {
                Some(line_rust_core::attachments::duration_ms(
                    p["durationMs"].as_f64(),
                    || {
                        let output = Command::new(probe_program)
                            .args([
                                "-v",
                                "error",
                                "-show_entries",
                                "format=duration",
                                "-of",
                                "default=nw=1:nk=1",
                            ])
                            .arg(&staged)
                            .output();
                        match output {
                            Ok(output) if output.status.success() => {
                                std::str::from_utf8(&output.stdout)
                                    .ok()?
                                    .trim()
                                    .parse::<f64>()
                                    .ok()
                            }
                            Ok(_) => {
                                tracing::warn!("ffprobe failed; using fallback duration");
                                None
                            }
                            Err(error) => {
                                tracing::warn!(
                                    "ffprobe unavailable; using fallback duration: {error}"
                                );
                                None
                            }
                        }
                    },
                ))
            } else {
                p["durationMs"].as_i64()
            };
            progress(0.35, "Uploading…")?;
            let upload_params = json!({"chatMid":p["chatMid"],"transferName":name,"previewName":if preview_ready{Some(&preview_name)}else{None},"fileName":filename,"extension":ext,"mime":line_rust_core::attachments::mime(&ext,kind),"oType":kind,"durationMs":duration});
            let mut result = line_rust_core::attachments::upload(
                &upload_params,
                |rpc, params| self.call(rpc, params, generation),
                || {
                    progress(0.40, "Retrying encrypted upload…")?;
                    // Keep retry pacing and cancellation in the Rust host.
                    for _ in 0..10 {
                        self.active(generation)?;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    self.active(generation)
                },
            )?;
            progress(0.90, "Saving attachment…")?;
            if method == "bridge_upload_plain_media" {
                let object_id = result["objId"]
                    .as_str()
                    .context("upload response missing object ID")?;
                numeric(object_id)?;
                result = json!({"rawMessage":{"id":object_id,"createdTime":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64,"contentMetadata":{"DURATION":duration.map(|n|n.to_string())}}});
            }
            result["rawMessage"]["from"] = p["account"].clone();
            result["rawMessage"]["to"] = p["chatMid"].clone();
            result["rawMessage"]["contentType"] = json!(if kind == "gif" {
                "IMAGE".to_owned()
            } else {
                kind.to_ascii_uppercase()
            });
            result["rawMessage"]["contentMetadata"]["FILE_NAME"] = json!(filename);
            if matches!(kind, "audio" | "video") {
                result["rawMessage"]["contentMetadata"]["DURATION"] =
                    json!(duration.map(|n| n.to_string()));
            }
            let id = string(&result["rawMessage"]["id"]);
            numeric(&id)?;
            let mut cache_row = result["rawMessage"].clone();
            cache_row["id"] = json!(id);
            cache_row["chatMid"] = json!(chat);
            let (media_dir, stem) = self.media_cache_layout(&cache_row)?;
            let dest = media_dir.join(if matches!(kind, "image" | "gif") {
                format!("{stem}.full.{ext}")
            } else {
                format!("{stem}.{ext}")
            });
            std::fs::rename(&staged, &dest)?;
            result["filePath"] = json!(dest);
            result["fileName"] = json!(filename);
            if preview_ready {
                let thumb = media_dir.join(format!("{stem}.thumb.jpg"));
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
            "expression_library" => self.expression_library(&p),
            "download_expression" => self.download_expression(&p, generation),
            "expression_icon" => {
                use line_rust_core::sticker_cache::{self, Fetch};
                let package = p["packageId"].as_str().context("missing package ID")?;
                let fetch = |url: &str, dest: &Path| {
                    self.active(generation)?;
                    self.asset(url, dest, generation)?;
                    Ok(Fetch::Saved)
                };
                let path = if p["kind"] == "emoji" {
                    sticker_cache::emoji_pack_icon(
                        &self.data.join("cache/stickers/emoji"),
                        package,
                        fetch,
                    )
                } else {
                    sticker_cache::pack_icon(&self.data.join("cache/stickers"), package, fetch)
                }?;
                self.active(generation)?;
                Ok(json!({"iconPath":path}))
            }
            "expression_images" => {
                let mut images = Vec::new();
                for resource in p["resources"].as_array().into_iter().flatten().take(100) {
                    let package = string(&resource["productId"]);
                    let id = string(&resource["sticonId"]);
                    if !line_rust_core::expressions::valid_id(&package)
                        || !line_rust_core::expressions::valid_id(&id)
                    {
                        continue;
                    }
                    let path = self
                        .data
                        .join("cache/stickers/emoji")
                        .join(&package)
                        .join(format!("{id}.png"));
                    if let Ok(path) = self.asset(&format!("https://stickershop.line-scdn.net/sticonshop/v1/sticon/{package}/android/{id}.png"), &path, generation) {
                        let mut resource = resource.clone();
                        resource["imagePath"] = json!(path);
                        images.push(resource);
                    }
                    self.active(generation)?;
                }
                Ok(json!({"resources":images}))
            }
            "notes" => self.notes_operation(p, generation),
            "profile_avatar" => {
                let mid = p["mid"].as_str().context("profile ID missing")?;
                let picture = line_rust_core::catalog::picture(&p)
                    .or_else(|| p["pictureUrl"].as_str().filter(|url| !url.is_empty()));
                if let Some(path) = self.avatar(mid, picture, generation) {
                    self.active(generation)?;
                    self.emit(json!({"event":"avatar_ready","mid":mid,"avatarPath":path}));
                }
                Ok(json!({}))
            }
            "list_chats" | "list_friends" => self.catalogs(method, &p["previous"], generation),
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
            "unsend_message" => {
                let result = self.call("bridge_unsend", p, generation)?;
                self.emit(json!({"event":"message_unsent","chatMid":result["chatMid"],"messageId":result["messageId"]}));
                Ok(result)
            }
            "mute_chat" => self.call("bridge_mute", p, generation),
            "send_postback" => line_rust_core::postback::send(&p, |method, args| {
                self.call(method, args, generation)
            }),
            "add_friend" => line_rust_core::friends::add(
                p["userid"].as_str().unwrap_or_default(),
                |method, args| self.call(method, args, generation),
            ),
            "clear_cache" => {
                let kind = p["kind"].as_str().unwrap_or("all");
                for name in ["media", "notes", "stickers", "avatars"] {
                    if kind == "all" || kind == name || (kind == "media" && name == "notes") {
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
