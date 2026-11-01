//! Rust-owned contact/chat/sticker presentation, CDN files and outgoing media staging.
use crate::core_bridge::Ipc;
use anyhow::{Context, Result, bail};
use line_gtk_core::{message::string, sync::Transport};
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
        use line_gtk_core::sticker_cache::{self, Fetch};
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
        let mut result = line_gtk_core::catalog::load(method, previous, |rpc, args| {
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
                line_gtk_core::catalog::picture(row),
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
        let mut result = line_gtk_core::stickers::load(
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
                        .and_then(|b| line_gtk_core::stickers::metadata(&b).ok())
                        .is_none()
                {
                    std::fs::remove_file(&path)?;
                }
                let downloaded=self.asset(&format!("https://stickershop.line-scdn.net/stickershop/v1/product/{package}/android/productInfo.meta"),&path,generation)?;
                let bytes = std::fs::read(downloaded)?;
                if let Err(error) = line_gtk_core::stickers::metadata(&bytes) {
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
        line_gtk_core::stickers::apply_pack_icons(&mut result, |package| {
            let path = line_gtk_core::sticker_cache::pack_icon(
                &self.data.join("cache/stickers"),
                package,
                |url, dest| {
                    self.active(generation)?;
                    self.asset(url, dest, generation)?;
                    Ok(line_gtk_core::sticker_cache::Fetch::Saved)
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
            self.emit(line_gtk_core::attachments::upload_progress(
                chat, fraction, label, false,
            ));
            Ok(())
        };
        let method =
            line_gtk_core::attachments::upload_method(p["chatMid"].as_str().unwrap_or_default())?;
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
        let kind = line_gtk_core::attachments::normalize_media_type(
            p["oType"].as_str().unwrap_or("auto"),
            &filename,
        );
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
                Some(line_gtk_core::attachments::duration_ms(
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
            let upload_params = json!({"chatMid":p["chatMid"],"transferName":name,"previewName":if preview_ready{Some(&preview_name)}else{None},"fileName":filename,"extension":ext,"mime":line_gtk_core::attachments::mime(&ext,kind),"oType":kind,"durationMs":duration});
            let mut result = line_gtk_core::attachments::upload(
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
            let dest = self.data.join("cache/media").join(format!("{id}.{ext}"));
            crate::config::ensure_private_dir(dest.parent().unwrap())?;
            std::fs::rename(&staged, &dest)?;
            result["filePath"] = json!(dest);
            result["fileName"] = json!(filename);
            if preview_ready {
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
            "profile_avatar" => {
                let mid = p["mid"].as_str().context("profile ID missing")?;
                let picture = line_gtk_core::catalog::picture(&p)
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
            "send_postback" => line_gtk_core::postback::send(&p, |method, args| {
                self.call(method, args, generation)
            }),
            "add_friend" => line_gtk_core::friends::add(
                p["userid"].as_str().unwrap_or_default(),
                |method, args| self.call(method, args, generation),
            ),
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
