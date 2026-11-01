use anyhow::{Context, Result, bail};
use async_channel::Sender;
use line_rust_core::{
    store::Store,
    sync::{Command as CoreCommand, Engine, Transport},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::Duration;

use crate::protocol::{ProtocolEvent, Response};

type Replies = Arc<Mutex<HashMap<u64, mpsc::SyncSender<Response>>>>;

#[derive(Clone)]
pub struct CoreBridge {
    pub memory: Arc<crate::memory_media::MemoryMedia>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    events: Sender<ProtocolEvent>,
    failed_generation: Arc<AtomicU64>,
    pub audio: crate::call_audio::AudioHost,
    data: PathBuf,
    pub commands: mpsc::Sender<CoreCommand>,
    replies: Replies,
    generation: Arc<AtomicU64>,
}

pub(crate) struct Ipc {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    replies: Replies,
    pub(crate) ids: Arc<AtomicU64>,
    generation: Arc<AtomicU64>,
    events: Sender<ProtocolEvent>,
    pub(crate) data: PathBuf,
    pub(crate) asset_at: Mutex<Option<std::time::Instant>>,
}

impl CoreBridge {
    pub fn start(
        data: &Path,
        stdin: Arc<Mutex<Option<ChildStdin>>>,
        ids: Arc<AtomicU64>,
        generation: Arc<AtomicU64>,
        events: Sender<ProtocolEvent>,
    ) -> Result<Self> {
        let cache = data.join("cache");
        let memory = crate::memory_media::for_data(data);
        memory.configure(crate::config::AppConfig::load(data).memory_media_chats);
        crate::config::ensure_private_dir(&cache)?;
        let store = Store::open(&cache.join("messages.sqlite3"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                cache.join("messages.sqlite3"),
                std::fs::Permissions::from_mode(0o600),
            )?;
        }
        let replies = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel();
        let ipc = Ipc {
            stdin: stdin.clone(),
            replies: replies.clone(),
            ids,
            generation: generation.clone(),
            events: events.clone(),
            data: data.to_owned(),
            asset_at: Mutex::new(None),
        };
        let engine = Engine::new(store, ipc, generation.clone(), cache.join("messages"));
        std::thread::Builder::new()
            .name("line-rust-core".into())
            .spawn(move || engine.run(rx))?;
        Ok(Self {
            memory,
            stdin,
            events,
            failed_generation: Arc::new(AtomicU64::new(0)),
            audio: Default::default(),
            data: data.to_owned(),
            commands: tx,
            replies,
            generation,
        })
    }

    pub fn route(&self, response: &Response, generation: u64) -> bool {
        if self.generation.load(Ordering::SeqCst) != generation {
            return true;
        }
        if let Some(id) = response.id
            && let Some(waiter) = self.replies.lock().ok().and_then(|mut r| r.remove(&id))
        {
            let _ = waiter.send(Response {
                id: response.id,
                ok: response.ok,
                result: response.result.clone(),
                error: response.error.clone(),
                event: response.event.clone(),
                extra: response.extra.clone(),
            });
            return true;
        }
        match response.event.as_deref() {
            Some("peer_read") => {
                let p = &response.extra;
                if let (Some(chat), Some(reader), Some(message)) = (
                    p["chatMid"].as_str(),
                    p["readerMid"].as_str(),
                    p["messageId"].as_str(),
                ) {
                    let _ = self.commands.send(CoreCommand::PeerRead {
                        chat: chat.into(),
                        reader: reader.into(),
                        message: message.into(),
                        generation,
                    });
                }
                return true;
            }
            Some("protocol_error") => {
                let code = response.extra["code"].as_str().unwrap_or_default();
                let error = response.extra["error"].as_str().unwrap_or("protocol error");
                if line_rust_core::session::authentication_failure(code)
                    || line_rust_core::session::authentication_failure(error)
                {
                    if self.failed_generation.swap(generation, Ordering::SeqCst) != generation {
                        // Invalidate queued and in-flight core work immediately,
                        // rather than waiting for Session to reach the worker.
                        let ended_generation = generation.wrapping_add(1);
                        if self
                            .generation
                            .compare_exchange(
                                generation,
                                ended_generation,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            )
                            .is_err()
                        {
                            return true;
                        }
                        self.audio.stop();
                        let _ = self.commands.send(CoreCommand::Session {
                            account: String::new(),
                            generation: ended_generation,
                        });
                        // Direct control request: do not queue behind a blocked media RPC.
                        if let Ok(mut input) = self.stdin.lock()
                            && let Some(input) = input.as_mut()
                        {
                            let _ =
                                writeln!(input, "{{\"method\":\"end_session\",\"params\":{{}}}}");
                            let _ = input.flush();
                        }
                        if let Ok(mut waiters) = self.replies.lock() {
                            for (id, waiter) in waiters.drain() {
                                let _ = waiter.send(Response {
                                    id: Some(id),
                                    ok: Some(false),
                                    result: None,
                                    error: Some("session_changed".into()),
                                    event: None,
                                    extra: Value::Null,
                                });
                            }
                        }
                        let _ = self.events.send_blocking(ProtocolEvent::SessionFailed {
                            error: "LOGGED_OUT: LINE session authorization expired".into(),
                        });
                    }
                } else {
                    // Avoid logging arbitrary decoder exception data/key material.
                    tracing::warn!(
                        source = response.extra["source"].as_str().unwrap_or("protocol"),
                        "recoverable protocol error; session retained"
                    );
                }
                return true;
            }
            Some("host_audio_start") => {
                if let Err(error) = self.audio.start(
                    &self.data.join("protocol-state/audio.sock"),
                    response.extra["input"].as_str().unwrap_or("default"),
                    response.extra["output"].as_str().unwrap_or("default"),
                ) {
                    tracing::warn!("audio start: {error}");
                }
                return true;
            }
            Some("host_audio_stop") => {
                self.audio.stop();
                return true;
            }
            Some("session_failed") => {
                let _ = self.commands.send(CoreCommand::Session {
                    account: String::new(),
                    generation,
                });
            }
            Some("session" | "core_session") => {
                self.failed_generation.store(0, Ordering::SeqCst);
                if let Some(account) = response.extra["mid"].as_str() {
                    let _ = self.commands.send(CoreCommand::Session {
                        account: account.into(),
                        generation,
                    });
                }
                if response.event.as_deref() == Some("core_session") {
                    return true;
                }
            }
            Some("message") => {
                if self.failed_generation.load(Ordering::SeqCst) == generation {
                    return true;
                }
                let row = &response.extra["message"];
                let chat = response.extra["chatMid"].as_str().unwrap_or_default();
                if !chat.is_empty() {
                    let _ = self.commands.send(CoreCommand::Live {
                        chat: chat.into(),
                        message: row.clone(),
                        generation,
                    });
                }
                return true;
            }
            Some("catalog_invalidated") => {
                let _ = self.commands.send(CoreCommand::Operation {
                    id: 0,
                    method: "invalidate_catalogs".into(),
                    params: json!({}),
                    generation,
                });
                return true;
            }
            Some("message_unsent") => {
                if let (Some(chat), Some(message)) = (
                    response.extra["chatMid"].as_str(),
                    response.extra["messageId"].as_str(),
                ) {
                    let _ = self.commands.send(CoreCommand::Unsend {
                        chat: chat.into(),
                        message: message.into(),
                        generation,
                    });
                }
            }
            _ => {}
        }
        // Successful sends carry their message in the response, even when the
        // server does not echo a SEND_MESSAGE event back to this device.
        if response.ok == Some(true)
            && let Some(row) = response.result.as_ref().and_then(|r| r.get("message"))
            && let Some(chat) = row["to"].as_str()
        {
            let _ = self.commands.send(CoreCommand::Live {
                chat: chat.into(),
                message: row.clone(),
                generation,
            });
        }
        false
    }
}

impl Ipc {
    pub(crate) fn active(&self, generation: u64) -> Result<()> {
        if self.generation.load(Ordering::SeqCst) != generation {
            bail!("session_changed");
        }
        Ok(())
    }

    fn safe_cache_file(&self, path: &Path) -> Result<PathBuf> {
        let path = path.canonicalize()?;
        if !path.starts_with(self.data.join("cache").canonicalize()?) {
            bail!("media path outside cache");
        }
        let metadata = path.metadata()?;
        if !metadata.is_file() || metadata.len() == 0 {
            bail!("cached media is not a nonempty regular file");
        }
        Ok(path)
    }

    fn cached_media_file(&self, path: &Path) -> Option<PathBuf> {
        let path = self.safe_cache_file(path).ok()?;
        path.starts_with(self.data.join("cache/media").canonicalize().ok()?)
            .then_some(path)
    }

    pub(crate) fn thumb(&self, file: &Path, dest: &Path) -> Result<()> {
        Self::thumb_with_program(Path::new("ffmpeg"), file, dest)
    }

    pub(crate) fn thumb_with_program(program: &Path, file: &Path, dest: &Path) -> Result<()> {
        let status = Command::new(program)
            .args(["-v", "error", "-y", "-i"])
            .arg(file)
            .args([
                "-vf",
                "scale=440:440:force_original_aspect_ratio=decrease",
                "-frames:v",
                "1",
            ])
            .arg(dest)
            .status()?;
        if !status.success() {
            bail!("thumbnail generation failed");
        }
        Ok(())
    }
}

impl Transport for Ipc {
    fn memory_media(&self, account: &str, chat: &str) -> bool {
        crate::memory_media::for_data(&self.data).enabled(account, chat)
    }
    fn media_visible(&self, chat: &str, id: &str) -> bool {
        crate::memory_media::for_data(&self.data).requested(chat, id)
    }
    fn session(&self, account: &str, generation: u64) {
        crate::memory_media::for_data(&self.data).session(account, generation);
    }
    fn application(&self, method: &str, params: Value, generation: u64) -> Result<Value> {
        self.application_operation(method, params, generation)
    }
    fn call(&self, method: &str, params: Value, generation: u64) -> Result<Value> {
        self.active(generation)?;
        let id = self.ids.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(1);
        self.replies
            .lock()
            .map_err(|_| anyhow::anyhow!("reply lock"))?
            .insert(id, tx);
        let result = (|| {
            let mut stdin = self
                .stdin
                .lock()
                .map_err(|_| anyhow::anyhow!("stdin lock"))?;
            let stdin = stdin.as_mut().context("protocol unavailable")?;
            serde_json::to_writer(
                &mut *stdin,
                &json!({"id":id,"method":method,"params":params}),
            )?;
            stdin.write_all(b"\n")?;
            stdin.flush()?;
            Ok(())
        })();
        if let Err(e) = result {
            self.replies.lock().ok().map(|mut r| r.remove(&id));
            return Err(e);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(300);
        let result = loop {
            if let Err(e) = self.active(generation) {
                break Err(e);
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(r) => {
                    break if r.ok == Some(true) {
                        Ok(r.result.unwrap_or(Value::Null))
                    } else {
                        Err(anyhow::anyhow!(
                            r.error.unwrap_or_else(|| "protocol error".into())
                        ))
                    };
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(anyhow::anyhow!("protocol disconnected"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() >= deadline => {
                    break Err(anyhow::anyhow!("protocol timeout"));
                }
                _ => {}
            }
        };
        self.replies.lock().ok().map(|mut r| r.remove(&id));
        result
    }

    fn materialize(&self, row: &Value, generation: u64) -> Result<Value> {
        self.materialize_with_preview(row, generation, |file, dest| self.thumb(file, dest))
    }

    fn emit(&self, event: Value) {
        self.emit_ui_event(event);
    }
}

impl Ipc {
    /// Transfer bounded chunks directly into anonymous memory, never cache/transfer.
    fn materialize_memory(&self, row: &Value, generation: u64) -> Result<Value> {
        use base64::Engine as _;
        let memory = crate::memory_media::for_data(&self.data);
        let chat = row["chatMid"].as_str().context("missing media chat")?;
        let id = row["id"].as_str().context("missing media ID")?;
        let epoch = memory.ticket(chat, id)?;
        if let Some(media) = memory.get(chat, id) {
            return Ok(media.apply(row));
        }
        let download = self.call("bridge_memory_download", json!({"message":row}), generation)?;
        let token = download["token"]
            .as_str()
            .context("missing memory transfer token")?;
        let result = (|| {
            let original = crate::memory_media::RamFile::new()?;
            let size = download["bytes"]
                .as_u64()
                .context("missing transfer size")?;
            if size == 0 {
                bail!("empty media");
            }
            let mut offset = 0;
            while offset < size {
                self.active(generation)?;
                memory.check(chat, id, epoch)?;
                let part = self.call(
                    "bridge_memory_chunk",
                    json!({"token":token,"offset":offset}),
                    generation,
                )?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(part["data"].as_str().context("missing transfer bytes")?)?;
                if bytes.is_empty()
                    || bytes.len() > 256 * 1024
                    || offset + bytes.len() as u64 > size
                {
                    bail!("invalid memory transfer chunk");
                }
                original.write(&bytes)?;
                offset += bytes.len() as u64;
            }
            let preview = crate::memory_media::preview(&original)
                .ok()
                .or_else(|| (row["contentType"] == "IMAGE").then(|| original.clone()));
            self.active(generation)?;
            let media = crate::memory_media::Media { original, preview };
            let updated = media.apply(row);
            memory.insert(chat, id, epoch, media)?;
            Ok(updated)
        })();
        let _ = self.call("bridge_memory_release", json!({"token":token}), generation);
        result
    }

    /// Stable UTC names keep lexical ordering chronological across time zones.
    /// Missing legacy timestamps use the epoch, never the time of download.
    pub(crate) fn media_cache_layout(&self, row: &Value) -> Result<(PathBuf, String)> {
        let id = row["id"].as_str().context("media ID missing")?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            bail!("invalid media ID");
        }
        let root = self.data.join("cache/media");
        if !matches!(row["contentType"].as_str(), Some("IMAGE" | "VIDEO")) {
            crate::config::ensure_private_dir(&root)?;
            return Ok((root, id.to_owned()));
        }
        let chat = row["chatMid"].as_str().context("media chat MID missing")?;
        if chat.is_empty() || chat.len() > 128 || !chat.bytes().all(|b| b.is_ascii_alphanumeric()) {
            bail!("invalid media chat MID");
        }
        crate::config::ensure_private_dir(&root)?;
        let dir = root.join(chat);
        if let Ok(metadata) = std::fs::symlink_metadata(&dir)
            && (!metadata.is_dir() || metadata.file_type().is_symlink())
        {
            bail!("invalid media chat directory");
        }
        crate::config::ensure_private_dir(&dir)?;
        let time = chrono::DateTime::from_timestamp_millis(line_rust_core::message::integer(
            &row["createdTime"],
        ))
        .filter(|time| (0..=9999).contains(&chrono::Datelike::year(time)))
        .unwrap_or(chrono::DateTime::UNIX_EPOCH);
        Ok((dir, format!("{}_{id}", time.format("%Y-%m-%d_%H%M%S_%3f"))))
    }

    /// Keep old paths readable for already-rendered messages and other accounts.
    /// Publish the migrated copy atomically, then let Engine persist the new path.
    fn adopt_cached_media(&self, source: &Path, dest: &Path) -> Result<PathBuf> {
        if let Some(existing) = self.cached_media_file(dest) {
            return Ok(existing);
        }
        static COPIES: AtomicU64 = AtomicU64::new(0);
        let temporary = dest.with_extension(format!(
            "migrating-{}-{}",
            std::process::id(),
            COPIES.fetch_add(1, Ordering::Relaxed)
        ));
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let result = (|| {
            let mut input = std::fs::File::open(source)?;
            std::io::copy(&mut input, &mut output)?;
            drop(output);
            std::fs::rename(&temporary, dest)?;
            self.cached_media_file(dest)
                .context("invalid migrated media")
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    fn materialize_with_preview(
        &self,
        row: &Value,
        generation: u64,
        make_preview: impl FnOnce(&Path, &Path) -> Result<()>,
    ) -> Result<Value> {
        self.active(generation)?;
        let id = row["id"].as_str().context("media ID missing")?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            bail!("invalid media ID");
        }
        let kind = row["contentType"].as_str().unwrap_or("FILE");
        if matches!(kind, "IMAGE" | "VIDEO")
            && crate::memory_media::for_data(&self.data)
                .current_enabled(row["chatMid"].as_str().unwrap_or_default())
        {
            return self.materialize_memory(row, generation);
        }
        if kind == "STICKER" {
            return self.sticker_media(row, generation);
        }
        let mut updated = row.clone();
        let (dir, stem) = self.media_cache_layout(row)?;
        let legacy_dir = self.data.join("cache/media");
        let mut file = row["filePath"]
            .as_str()
            .map(PathBuf::from)
            .and_then(|p| self.cached_media_file(&p));
        let attachment_ext =
            line_rust_core::attachments::file_extension(row["fileName"].as_str(), "");
        if kind == "VIDEO"
            && file.as_ref().is_some_and(|p| {
                !p.extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(line_rust_core::attachments::video_extension)
            })
        {
            file = None;
        }
        if file.is_none() && kind == "FILE" {
            file = self
                .cached_media_file(&dir.join(format!("{id}.{attachment_ext}")))
                .or_else(|| self.cached_media_file(&dir.join(format!("{id}.bin"))));
        }
        if file.is_none() && kind != "FILE" {
            // Recover binaries downloaded before the SQLite migration.
            for ext in [
                "mp4",
                "webm",
                "mov",
                "mkv",
                "m4v",
                "full.jpg",
                "full.jpeg",
                "full.png",
                "full.webp",
                "full.gif",
                "full.bmp",
                "full.heic",
                "full.bin",
                "m4a",
                "mp3",
                "ogg",
                "bin",
            ] {
                let candidate = dir.join(format!("{stem}.{ext}"));
                if (kind == "VIDEO" && !line_rust_core::attachments::video_extension(ext))
                    || (kind == "IMAGE" && !ext.starts_with("full."))
                {
                    continue;
                }
                if let Some(candidate) = self.cached_media_file(&candidate) {
                    file = Some(candidate);
                    break;
                }
            }
        }
        if file.is_none() && matches!(kind, "IMAGE" | "VIDEO") {
            // Pre-MID caches (including originals uploaded without .full.).
            let extensions: &[&str] = if kind == "IMAGE" {
                &[
                    "full.jpg",
                    "full.jpeg",
                    "full.png",
                    "full.webp",
                    "full.gif",
                    "jpg",
                    "jpeg",
                    "png",
                    "webp",
                    "gif",
                    "bmp",
                    "heic",
                ]
            } else {
                &line_rust_core::attachments::VIDEO_EXTENSIONS
            };
            file = extensions
                .iter()
                .find_map(|ext| self.cached_media_file(&legacy_dir.join(format!("{id}.{ext}"))));
        }
        if file.is_none() {
            let downloaded = self.call("bridge_download", json!({"message":row}), generation)?;
            self.active(generation)?;
            let temporary = self.safe_cache_file(Path::new(
                downloaded["path"]
                    .as_str()
                    .context("missing transfer path")?,
            ))?;
            if !temporary.starts_with(self.data.join("cache/transfer").canonicalize()?) {
                bail!("invalid transfer location");
            }
            // A right-click policy change can arrive during the blocking RPC.
            if matches!(kind, "IMAGE" | "VIDEO")
                && crate::memory_media::for_data(&self.data)
                    .current_enabled(row["chatMid"].as_str().unwrap_or_default())
            {
                let _ = std::fs::remove_file(&temporary);
                return self.materialize_memory(row, generation);
            }
            let mime = downloaded["mime"].as_str().unwrap_or_default();
            let generic_ext =
                line_rust_core::attachments::file_extension(row["fileName"].as_str(), mime);
            let ext = if kind == "FILE" {
                generic_ext.as_str()
            } else if mime.contains("png") {
                "png"
            } else if mime.contains("gif") {
                "gif"
            } else if mime.contains("webp") {
                "webp"
            } else if kind == "IMAGE" {
                "jpg"
            } else if kind == "VIDEO" {
                "mp4"
            } else if mime.contains("mpeg") {
                "mp3"
            } else if mime.contains("ogg") {
                "ogg"
            } else if kind == "AUDIO" {
                "m4a"
            } else {
                "bin"
            };
            if kind != "FILE"
                && ![
                    "jpg", "png", "gif", "webp", "mp4", "m4a", "mp3", "ogg", "bin",
                ]
                .contains(&ext)
            {
                bail!("invalid media extension");
            }
            let dest = dir.join(if kind == "IMAGE" {
                format!("{stem}.full.{ext}")
            } else {
                format!("{stem}.{ext}")
            });
            std::fs::rename(temporary, &dest)?;
            file = Some(dest);
        }
        let mut file = file.context("media unavailable")?;
        if matches!(kind, "IMAGE" | "VIDEO") {
            let ext = line_rust_core::attachments::file_extension(
                file.file_name().and_then(|name| name.to_str()),
                "",
            );
            let dest = dir.join(if kind == "IMAGE" {
                format!("{stem}.full.{ext}")
            } else {
                format!("{stem}.{ext}")
            });
            file = self.adopt_cached_media(&file, &dest)?;
        }
        // Repair older .bin downloads locally so PDF/text preview works without
        // re-fetching the original. Rename replaces symlinks, never follows them.
        if kind == "FILE"
            && attachment_ext != "bin"
            && file.extension().and_then(|s| s.to_str()) == Some("bin")
        {
            let named = dir.join(format!("{id}.{attachment_ext}"));
            std::fs::rename(&file, &named)?;
            file = self
                .cached_media_file(&named)
                .context("invalid renamed attachment")?;
        }
        updated["filePath"] = json!(file);
        updated["needsMedia"] = json!(false);
        match kind {
            "IMAGE" if file.extension().and_then(|s| s.to_str()) == Some("gif") => {
                updated["imagePath"] = json!(file);
            }
            "IMAGE" | "VIDEO" => {
                let thumb = dir.join(format!("{stem}.thumb.jpg"));
                let preview = self.cached_media_file(&thumb).map(Ok).unwrap_or_else(|| {
                    let old_thumb = row["imagePath"]
                        .as_str()
                        .filter(|p| {
                            Path::new(p)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.ends_with(".thumb.jpg"))
                        })
                        .and_then(|p| self.cached_media_file(Path::new(p)))
                        .or_else(|| {
                            self.cached_media_file(&legacy_dir.join(format!("{id}.thumb.jpg")))
                        });
                    if let Some(old_thumb) = old_thumb {
                        return self.adopt_cached_media(&old_thumb, &thumb);
                    }
                    // Do not let ffmpeg follow an invalid/escaping thumbnail link.
                    if std::fs::symlink_metadata(&thumb).is_ok() {
                        std::fs::remove_file(&thumb)?;
                    }
                    make_preview(&file, &thumb)?;
                    self.cached_media_file(&thumb)
                        .context("invalid cached thumbnail")
                });
                self.active(generation)?;
                match preview {
                    Ok(path) => updated["imagePath"] = json!(path),
                    Err(error) => {
                        tracing::warn!("downloaded original available without thumbnail: {error}");
                        if let Err(cleanup) = std::fs::remove_file(&thumb)
                            && cleanup.kind() != std::io::ErrorKind::NotFound
                        {
                            tracing::warn!("download preview cleanup: {cleanup}");
                        }
                        // Images can be displayed directly. A video without a
                        // thumbnail is still playable through its filePath.
                        updated["imagePath"] = if kind == "IMAGE" {
                            json!(file)
                        } else {
                            Value::Null
                        };
                    }
                }
            }
            "STICKER" => updated["imagePath"] = json!(file),
            "AUDIO" => updated["audioPath"] = json!(file),
            _ => {}
        }
        Ok(updated)
    }

    fn emit_ui_event(&self, event: Value) {
        let ev = if let Some(id) = event["id"].as_u64() {
            Some(ProtocolEvent::Response {
                id,
                ok: event["ok"].as_bool().unwrap_or(false),
                result: event["result"].clone(),
                error: event["error"].as_str().map(str::to_owned),
            })
        } else {
            let chat_mid = event["chatMid"].as_str().unwrap_or_default().to_owned();
            let message_id = event["messageId"].as_str().unwrap_or_default().to_owned();
            match event["event"].as_str() {
                Some("read_counts") => Some(ProtocolEvent::ReadCounts {
                    account: event["account"].as_str().unwrap_or_default().into(),
                    chat_mid,
                    counts: serde_json::from_value(event["counts"].clone()).unwrap_or_default(),
                }),
                Some("avatar_ready") => Some(ProtocolEvent::AvatarReady {
                    mid: event["mid"].as_str().unwrap_or_default().into(),
                    avatar_path: event["avatarPath"].as_str().unwrap_or_default().into(),
                }),
                Some("expression_state") => Some(ProtocolEvent::ExpressionState(event.clone())),
                Some("chat_mute") => Some(ProtocolEvent::ChatMute {
                    mid: event["mid"].as_str().unwrap_or_default().to_owned(),
                    muted: event["muted"].as_bool().unwrap_or(false),
                }),
                Some("upload_progress") => Some(ProtocolEvent::UploadProgress {
                    chat_mid,
                    progress: event["progress"].as_f64().unwrap_or(0.0),
                    label: event["label"].as_str().unwrap_or_default().to_owned(),
                    done: event["done"].as_bool().unwrap_or(false),
                }),
                Some("message") => serde_json::from_value(event["message"].clone())
                    .ok()
                    .map(|message| ProtocolEvent::Message { chat_mid, message }),
                Some("message_unsent") => Some(ProtocolEvent::MessageUnsent {
                    chat_mid,
                    message_id,
                }),
                Some("media_ready") => Some(ProtocolEvent::MediaReady {
                    chat_mid,
                    message_id,
                    image_path: event["imagePath"].as_str().unwrap_or_default().into(),
                    audio_path: event["audioPath"].as_str().map(str::to_owned),
                    file_path: event["filePath"].as_str().map(str::to_owned),
                }),
                Some("media_failed") => Some(ProtocolEvent::MediaFailed {
                    chat_mid,
                    message_id,
                }),
                _ => None,
            }
        };
        if let Some(ev) = ev {
            let _ = self.events.send_blocking(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn live_messages_keep_the_same_chat_in_storage_and_ui() {
        let root = std::env::temp_dir().join(format!("line-live-routing-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("messages.sqlite3");
        let store = Store::open(&db).unwrap();
        let (commands, rx) = mpsc::channel();
        let (events, ui) = async_channel::unbounded();
        let bridge = CoreBridge {
            memory: Arc::default(),
            audio: Default::default(),
            data: root.clone(),
            commands,
            replies: Arc::default(),
            generation: Arc::new(AtomicU64::new(1)),
            stdin: Arc::new(Mutex::new(None)),
            events: events.clone(),
            failed_generation: Arc::new(AtomicU64::new(0)),
        };
        let ipc = Ipc {
            stdin: bridge.stdin.clone(),
            replies: bridge.replies.clone(),
            ids: Arc::new(AtomicU64::new(1)),
            generation: bridge.generation.clone(),
            events,
            data: root.clone(),
            asset_at: Mutex::new(None),
        };
        let engine = Engine::new(store, ipc, bridge.generation.clone(), root.join("legacy"));
        let session =
            serde_json::from_value(json!({"event":"core_session","mid":"u-self"})).unwrap();
        assert!(bridge.route(&session, 1));
        let cases = [
            ("c-group", "u-peer", "c-group", false),
            ("r-room", "u-peer", "r-room", false),
            ("u-peer", "u-peer", "u-self", false),
            ("c-group", "u-self", "c-group", false),
            ("r-room", "u-self", "r-room", false),
            ("u-peer", "u-self", "u-peer", false),
            ("c-group", "u-peer", "c-group", true),
            ("r-room", "u-peer", "r-room", true),
        ];
        for (index, (chat, from, to, failed)) in cases.iter().enumerate() {
            let raw = json!({"id":(index + 1).to_string(),"from":from,"to":to,
                "createdTime":index + 1,"contentType":"NONE","text":"hello"});
            let response = serde_json::from_value(json!({"event":"message","chatMid":chat,
                "message":{"rawMessage":raw,"decodedMessage":if *failed { Value::Null } else { raw.clone() },
                "decodeError":if *failed { json!("message_decode_failed") } else { Value::Null }}})).unwrap();
            assert!(bridge.route(&response, 1));
        }
        bridge.commands.send(CoreCommand::Stop).unwrap();
        engine.run(rx);

        let mut store = Store::open(&db).unwrap();
        store.set_account("u-self");
        for (index, (chat, from, to, _)) in cases.iter().enumerate() {
            let ProtocolEvent::Message { chat_mid, message } = ui.try_recv().unwrap() else {
                panic!("expected live message");
            };
            assert_eq!(chat_mid, *chat);
            assert_eq!(message.id, (index + 1).to_string());
            assert_eq!(message.from, *from);
            assert_eq!(message.to, *to);
            assert_eq!(message.mine, *from == "u-self");
            assert_eq!(
                store.message(chat, &message.id).unwrap().unwrap()["text"],
                message.text
            );
            for other in ["c-group", "r-room", "u-peer", "u-self"] {
                if other != *chat {
                    assert!(store.message(other, &message.id).unwrap().is_none());
                }
            }
        }
        assert!(ui.try_recv().is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn peer_receipts_route_through_core_and_counts_reach_ui_with_account() {
        let (commands, rx) = mpsc::channel();
        let (events, ui) = async_channel::unbounded();
        let bridge = CoreBridge {
            memory: Arc::default(),
            audio: Default::default(),
            data: PathBuf::new(),
            commands,
            replies: Arc::default(),
            generation: Arc::new(AtomicU64::new(1)),
            stdin: Arc::new(Mutex::new(None)),
            events: events.clone(),
            failed_generation: Arc::new(AtomicU64::new(0)),
        };
        let response: Response = serde_json::from_value(json!({"event":"peer_read", "chatMid":"c1", "readerMid":"u1", "messageId":"9007199254740993"})).unwrap();
        assert!(bridge.route(&response, 1));
        assert!(
            matches!(rx.try_recv().unwrap(), CoreCommand::PeerRead { chat, reader, message, generation:1 } if chat=="c1" && reader=="u1" && message=="9007199254740993")
        );
        assert!(ui.try_recv().is_err());
        bridge.route(&response, 0);
        assert!(rx.try_recv().is_err());
        let ipc = Ipc {
            stdin: bridge.stdin.clone(),
            replies: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            generation: bridge.generation.clone(),
            events,
            data: PathBuf::new(),
            asset_at: Mutex::new(None),
        };
        ipc.emit_ui_event(json!({"event":"read_counts","account":"me","chatMid":"c1","counts":{"9007199254740993":3}}));
        assert!(
            matches!(ui.try_recv().unwrap(), ProtocolEvent::ReadCounts {account,chat_mid,counts} if account=="me" && chat_mid=="c1" && counts["9007199254740993"]==3)
        );
    }

    #[test]
    fn own_profile_avatar_is_loaded_from_rust_cache_and_forwarded_to_ui() {
        let root = std::env::temp_dir().join(format!("line-own-avatar-{}", std::process::id()));
        std::fs::create_dir_all(root.join("cache/avatars")).unwrap();
        let path = root.join("cache/avatars/u1.jpg");
        std::fs::write(&path, b"cached avatar").unwrap();
        let (events, rx) = async_channel::unbounded();
        let ipc = Ipc {
            stdin: Arc::new(Mutex::new(None)),
            replies: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            events,
            data: root.clone(),
            asset_at: Mutex::new(None),
        };
        ipc.application(
            "profile_avatar",
            json!({"mid":"u1","pictureStatus":"/avatar"}),
            1,
        )
        .unwrap();
        assert!(
            matches!(rx.try_recv().unwrap(),ProtocolEvent::AvatarReady{mid,avatar_path} if mid=="u1"&&Path::new(&avatar_path)==path)
        );
        assert_eq!(ipc.ids.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn recoverable_protocol_errors_keep_live_messages_but_auth_failure_stops_session_once() {
        let (commands, rx) = mpsc::channel();
        let (events, ui) = async_channel::unbounded();
        let mut child = Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let bridge = CoreBridge {
            memory: Arc::default(),
            audio: Default::default(),
            data: PathBuf::new(),
            commands,
            replies: Arc::default(),
            generation: Arc::new(AtomicU64::new(1)),
            stdin: Arc::new(Mutex::new(child.stdin.take())),
            events,
            failed_generation: Arc::new(AtomicU64::new(0)),
        };
        let report = |code: &str, error: &str| Response {
            id: None,
            ok: None,
            result: None,
            error: None,
            event: Some("protocol_error".into()),
            extra: json!({"code":code,"error":error,"source":"test"}),
        };
        assert!(bridge.route(&report("", "decode failed"), 1));
        assert!(rx.try_recv().is_err());
        assert!(ui.try_recv().is_err());
        let live = Response {
            id: None,
            ok: None,
            result: None,
            error: None,
            event: Some("message".into()),
            extra: json!({"chatMid":"u1","message":{"id":"2","text":"still receiving"}}),
        };
        assert!(bridge.route(&live, 1));
        assert!(matches!(rx.try_recv().unwrap(), CoreCommand::Live { .. }));
        let (waiter, pending) = mpsc::sync_channel(1);
        bridge.replies.lock().unwrap().insert(99, waiter);
        bridge.route(&report("NOT_AUTHORIZED_DEVICE", "request failed"), 1);
        assert_eq!(bridge.generation.load(Ordering::SeqCst), 2);
        assert_eq!(
            pending
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .error
                .as_deref(),
            Some("session_changed")
        );
        assert!(
            matches!(rx.try_recv().unwrap(),CoreCommand::Session{account,generation:2} if account.is_empty())
        );
        assert!(matches!(
            ui.try_recv().unwrap(),
            ProtocolEvent::SessionFailed { .. }
        ));
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["method"],
            "end_session"
        );
        bridge.route(&report("LOGGED_OUT", "request failed"), 1);
        bridge.route(&live, 1);
        assert!(rx.try_recv().is_err());
        assert!(ui.try_recv().is_err());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn downloaded_original_survives_thumbnail_failure_and_cached_reopen() {
        struct PreviewHost {
            ipc: Ipc,
            program: PathBuf,
        }
        impl Transport for PreviewHost {
            fn call(&self, m: &str, p: Value, g: u64) -> Result<Value> {
                self.ipc.call(m, p, g)
            }
            fn emit(&self, e: Value) {
                self.ipc.emit(e);
            }
            fn materialize(&self, row: &Value, g: u64) -> Result<Value> {
                self.ipc.materialize_with_preview(row, g, |file, dest| {
                    Ipc::thumb_with_program(&self.program, file, dest)
                })
            }
        }
        for kind in ["IMAGE", "VIDEO"] {
            for mode in ["missing", "failed", "empty", "success"] {
                let root = std::env::temp_dir().join(format!(
                    "line-download-preview-{}-{kind}-{mode}",
                    std::process::id()
                ));
                std::fs::create_dir_all(root.join("cache/transfer")).unwrap();
                let program = root.join("fake-ffmpeg");
                if mode != "missing" {
                    use std::os::unix::fs::PermissionsExt;
                    let script = match mode {
                        "failed" => {
                            "#!/bin/sh\nfor last; do :; done\nprintf partial > \"$last\"\nexit 1\n"
                        }
                        "empty" => "#!/bin/sh\nfor last; do :; done\n: > \"$last\"\n",
                        _ => "#!/bin/sh\nfor last; do :; done\nprintf thumbnail > \"$last\"\n",
                    };
                    std::fs::write(&program, script).unwrap();
                    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                let temporary = root.join("cache/transfer/original.part");
                std::fs::write(&temporary, b"original media bytes").unwrap();
                let mut child = Command::new("cat")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                let stdout = child.stdout.take().unwrap();
                let replies: Replies = Arc::default();
                let waiting = replies.clone();
                let responder = std::thread::spawn(move || {
                    let mut line = String::new();
                    BufReader::new(stdout).read_line(&mut line).unwrap();
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let id = req["id"].as_u64().unwrap();
                    waiting.lock().unwrap().remove(&id).unwrap().send(Response{id:Some(id),ok:Some(true),result:Some(json!({"path":temporary,"mime":if kind=="IMAGE"{"image/png"}else{"video/mp4"}})),error:None,event:None,extra:Value::Null}).unwrap();
                    assert_eq!(req["method"], "bridge_download");
                });
                let (events, rx) = async_channel::unbounded();
                let ids = Arc::new(AtomicU64::new(1));
                let generation = Arc::new(AtomicU64::new(1));
                let input = Arc::new(Mutex::new(child.stdin.take()));
                let ipc = Ipc {
                    stdin: input.clone(),
                    replies,
                    ids: ids.clone(),
                    generation: generation.clone(),
                    events,
                    data: root.clone(),
                    asset_at: Mutex::new(None),
                };
                let db = root.join("messages.sqlite3");
                let mut store = Store::open(&db).unwrap();
                store.set_account("me");
                store.save_messages("u1",&[json!({"id":"123","chatMid":"wrong","from":"u1","to":"me","createdTime":1234,"contentType":kind,"imagePath":"/stale/thumbnail.jpg"})]).unwrap();
                let engine = Engine::new(
                    store,
                    PreviewHost { ipc, program },
                    generation.clone(),
                    root.join("legacy"),
                );
                let (tx, commands) = mpsc::channel();
                let worker = std::thread::spawn(move || engine.run(commands));
                tx.send(CoreCommand::Session {
                    account: "me".into(),
                    generation: 1,
                })
                .unwrap();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for request_id in [42, 43] {
                        tx.send(CoreCommand::Download {
                            id: request_id,
                            chat: "u1".into(),
                            message: "123".into(),
                            generation: 1,
                        })
                        .unwrap();
                        let deadline = std::time::Instant::now() + Duration::from_secs(10);
                        let mut ready = false;
                        loop {
                            let event = loop {
                                if let Ok(event) = rx.try_recv() {
                                    break event;
                                }
                                assert!(
                                    std::time::Instant::now() < deadline,
                                    "no download response"
                                );
                                std::thread::sleep(Duration::from_millis(5));
                            };
                            match event {
                                ProtocolEvent::MediaReady {
                                    image_path,
                                    file_path,
                                    ..
                                } => {
                                    ready = true;
                                    let file = file_path.unwrap();
                                    assert!(
                                        file.contains("/cache/media/u1/1970-01-01_000001_234_123.")
                                    );
                                    assert_eq!(
                                        std::fs::read(&file).unwrap(),
                                        b"original media bytes"
                                    );
                                    if mode == "success" {
                                        assert!(image_path.ends_with("123.thumb.jpg"));
                                    } else if kind == "IMAGE" {
                                        assert_eq!(image_path, file);
                                    } else {
                                        assert!(image_path.is_empty());
                                    }
                                }
                                ProtocolEvent::MediaFailed { .. } => {
                                    panic!("thumbnail failure is not a download failure")
                                }
                                ProtocolEvent::Response {
                                    id,
                                    ok,
                                    result,
                                    error,
                                } => {
                                    assert_eq!(id, request_id);
                                    assert!(ok, "{mode}/{kind}: {error:?}");
                                    assert!(ready);
                                    assert_eq!(
                                        std::fs::read(result["path"].as_str().unwrap()).unwrap(),
                                        b"original media bytes"
                                    );
                                    break;
                                }
                                _ => {}
                            }
                        }
                        // Simulate disconnected network before the second request.
                        *input.lock().unwrap() = None;
                    }
                    responder.join().unwrap();
                    assert_eq!(ids.load(Ordering::SeqCst), 2);
                }));
                tx.send(CoreCommand::Stop).unwrap();
                let _ = child.kill();
                let _ = child.wait();
                if outcome.is_err() {
                    generation.fetch_add(1, Ordering::SeqCst);
                }
                worker.join().unwrap();
                if let Err(e) = outcome {
                    std::panic::resume_unwind(e);
                }
                assert_eq!(
                    root.join("cache/media/u1/1970-01-01_000001_234_123.thumb.jpg")
                        .exists(),
                    mode == "success"
                );
                let mut store = Store::open(&db).unwrap();
                store.set_account("me");
                let saved = store.message("u1", "123").unwrap().unwrap();
                assert!(Path::new(saved["filePath"].as_str().unwrap()).is_file());
                assert_eq!(saved["needsMedia"], false);
                if kind == "VIDEO" && mode != "success" {
                    assert!(saved["imagePath"].is_null());
                }
                drop(store);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }
    #[test]
    fn confirmed_mute_is_forwarded_to_ui_event() {
        let (events, rx) = async_channel::unbounded();
        let ipc = Ipc {
            stdin: Arc::new(Mutex::new(None)),
            replies: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            events,
            data: PathBuf::new(),
            asset_at: Mutex::new(None),
        };
        for muted in [true, false] {
            ipc.emit(json!({"event":"chat_mute","mid":"u1","muted":muted}));
            assert!(
                matches!(rx.try_recv().unwrap(),ProtocolEvent::ChatMute{mid,muted:actual} if mid=="u1" && actual==muted)
            );
        }
    }

    #[test]
    fn attachment_types_reach_rpc_preview_and_saved_message() {
        for (index, (filename, requested, kind, mime)) in [
            ("photo.BMP", "auto", "image", "image/bmp"),
            ("photo.HEIC", "auto", "image", "image/jpeg"),
            ("clip.M4V", "auto", "video", "video/mp4"),
            ("clip.WebM", "auto", "video", "video/webm"),
            ("clip.MOV", "auto", "video", "video/quicktime"),
            ("clip.MKV", "auto", "video", "video/x-matroska"),
            ("clip.MP4", "auto", "video", "video/mp4"),
            ("voice.AAC", "auto", "audio", "audio/mp4"),
            ("voice.OGA", "auto", "audio", "audio/ogg"),
            ("voice.FLAC", "auto", "audio", "audio/mp4"),
            ("animation.GIF", " IMAGE ", "gif", "image/gif"),
            ("animation.GIF", " FILE ", "file", "image/gif"),
            ("photo.bmp", "unsupported", "image", "image/bmp"),
            ("blob.bin", " ViDeO ", "video", "video/mp4"),
            ("blob.bin", " AuDiO ", "audio", "audio/mp4"),
            ("blob.bin", " GIF ", "gif", "image/jpeg"),
        ]
        .into_iter()
        .enumerate()
        {
            let root =
                std::env::temp_dir().join(format!("line-type-rpc-{}-{index}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let source = root.join(filename);
            std::fs::write(&source, b"unchanged original").unwrap();
            let mut child = Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let replies: Replies = Arc::default();
            let waiting = replies.clone();
            let preview_expected = matches!(kind, "image" | "gif" | "video");
            let responder = std::thread::spawn(move || {
                let mut line = String::new();
                BufReader::new(stdout).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let p = &request["params"];
                // Reply before assertions so a regression never leaves the caller waiting.
                let id = request["id"].as_u64().unwrap();
                waiting
                    .lock()
                    .unwrap()
                    .remove(&id)
                    .unwrap()
                    .send(Response {
                        id: Some(id),
                        ok: Some(true),
                        result: Some(json!({"rawMessage":{"id":"123","contentMetadata":{}}})),
                        error: None,
                        event: None,
                        extra: Value::Null,
                    })
                    .unwrap();
                assert_eq!(request["method"], "bridge_send_media");
                assert_eq!(p["oType"], kind);
                assert_eq!(p["mime"], mime);
                assert_eq!(p["fileName"], filename);
                assert_eq!(p["previewName"].is_string(), preview_expected);
                assert!(p["transferName"].as_str().unwrap().starts_with("upload-"));
            });
            let (events, _) = async_channel::unbounded();
            let generation = Arc::new(AtomicU64::new(1));
            let ipc = Ipc {
                stdin: Arc::new(Mutex::new(child.stdin.take())),
                replies,
                ids: Arc::new(AtomicU64::new(1)),
                generation,
                events,
                data: root.clone(),
                asset_at: Mutex::new(None),
            };
            let mut preview_called = false;
            let result=ipc.send_attachment_with_tools(&json!({"chatMid":"u1","account":"me","filePath":source,"oType":requested,"durationMs":3200}),1,|_,dest|{preview_called=true;std::fs::write(dest,b"preview")?;Ok(())},Path::new("/unused-probe"));
            let _ = child.kill();
            let _ = child.wait();
            responder.join().unwrap();
            let result = result.unwrap();
            assert_eq!(preview_called, preview_expected);
            assert_eq!(
                result["rawMessage"]["contentType"],
                if kind == "gif" {
                    "IMAGE".into()
                } else {
                    kind.to_ascii_uppercase()
                }
            );
            assert_eq!(
                std::fs::read(result["filePath"].as_str().unwrap()).unwrap(),
                b"unchanged original"
            );
            assert_eq!(std::fs::read(source).unwrap(), b"unchanged original");
            if kind == "video" && filename != "blob.bin" {
                let mut row = result["rawMessage"].clone();
                row["chatMid"] = json!("u1");
                row["filePath"] = result["filePath"].clone();
                row["imagePath"] = result["imagePath"].clone();
                // Any attempted RPC now fails immediately: cached reloads must
                // succeed without the upload process or network being available.
                *ipc.stdin.lock().unwrap() = None;
                let ids_before = ipc.ids.load(Ordering::SeqCst);
                for with_path in [true, false] {
                    if !with_path {
                        row["filePath"] = Value::Null;
                    }
                    let loaded = ipc.materialize(&row, 1).unwrap();
                    assert_eq!(
                        std::fs::read(loaded["filePath"].as_str().unwrap()).unwrap(),
                        b"unchanged original"
                    );
                    assert_eq!(
                        std::fs::read(loaded["imagePath"].as_str().unwrap()).unwrap(),
                        b"preview"
                    );
                }
                assert_eq!(
                    ipc.ids.load(Ordering::SeqCst),
                    ids_before,
                    "cache hit must not issue RPC"
                );
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn mid_cache_migrates_legacy_originals_and_previews_offline() {
        let root = std::env::temp_dir().join(format!("line-mid-cache-{}", std::process::id()));
        let legacy = root.join("cache/media");
        std::fs::create_dir_all(&legacy).unwrap();
        let (events, _) = async_channel::unbounded();
        let ipc = Ipc {
            stdin: Arc::new(Mutex::new(None)),
            replies: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            events,
            data: root.clone(),
            asset_at: Mutex::new(None),
        };
        for (id, kind, old_suffix, new_suffix, chat) in [
            ("123", "IMAGE", "full.png", "full.png", "cgroup"),
            ("124", "IMAGE", "gif", "full.gif", "upeer"),
            ("125", "VIDEO", "webm", "webm", "rroom"),
        ] {
            let old = legacy.join(format!("{id}.{old_suffix}"));
            let old_thumb = legacy.join(format!("{id}.thumb.jpg"));
            std::fs::write(&old, b"original").unwrap();
            std::fs::write(&old_thumb, b"preview").unwrap();
            let mut row = json!({"id":id,"chatMid":chat,"from":"sender","to":"me","createdTime":"1793525415123","contentType":kind});
            // Test both saved paths and filename-only recovery of old uploads.
            if kind == "VIDEO" {
                row["filePath"] = json!(old);
                row["imagePath"] = json!(old_thumb);
            }
            let updated = ipc
                .materialize_with_preview(&row, 1, |_, _| panic!("reuse legacy preview"))
                .unwrap();
            let expected = legacy
                .join(chat)
                .join(format!("2026-11-01_093015_123_{id}.{new_suffix}"));
            assert_eq!(Path::new(updated["filePath"].as_str().unwrap()), expected);
            assert_eq!(std::fs::read(&expected).unwrap(), b"original");
            assert_eq!(
                std::fs::read(&old).unwrap(),
                b"original",
                "old UI/database paths stay valid"
            );
            if new_suffix == "full.gif" {
                assert_eq!(updated["imagePath"], updated["filePath"]);
            } else {
                assert_eq!(
                    std::fs::read(updated["imagePath"].as_str().unwrap()).unwrap(),
                    b"preview"
                );
                assert_eq!(
                    Path::new(updated["imagePath"].as_str().unwrap()).parent(),
                    expected.parent()
                );
            }
            // Simulate a stale SQLite row after migration, with only the new
            // original/preview left on disk: rediscovery must not need RPC.
            std::fs::remove_file(old).unwrap();
            std::fs::remove_file(old_thumb).unwrap();
            let again = ipc
                .materialize_with_preview(&row, 1, |_, _| panic!("reuse new preview"))
                .unwrap();
            assert_eq!(again, updated);
        }
        assert_eq!(ipc.ids.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mid_cache_paths_are_stable_isolated_and_reject_unsafe_directories() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("line-mid-layout-{}", std::process::id()));
        let (events, _) = async_channel::unbounded();
        let ipc = Ipc {
            stdin: Arc::new(Mutex::new(None)),
            replies: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            events,
            data: root.clone(),
            asset_at: Mutex::new(None),
        };
        let row = json!({"id":"999","chatMid":"cgroup","createdTime":1234,"contentType":"IMAGE"});
        let (dir, stem) = ipc.media_cache_layout(&row).unwrap();
        assert_eq!(stem, "1970-01-01_000001_234_999");
        let mut later = row.clone();
        later["id"] = json!("1");
        later["createdTime"] = json!(1235);
        assert!(stem < ipc.media_cache_layout(&later).unwrap().1);
        later["chatMid"] = json!("upeer");
        assert_ne!(dir, ipc.media_cache_layout(&later).unwrap().0);
        for time in [Value::Null, json!(i64::MAX), json!("invalid")] {
            later["createdTime"] = time;
            assert_eq!(
                ipc.media_cache_layout(&later).unwrap().1,
                "1970-01-01_000000_000_1"
            );
        }
        for chat in ["", "..", "../escape", "/absolute", "u/peer", "u\\peer"] {
            later["chatMid"] = json!(chat);
            assert!(ipc.media_cache_layout(&later).is_err());
        }
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("cache/media/ulink")).unwrap();
        later["chatMid"] = json!("ulink");
        assert!(ipc.media_cache_layout(&later).is_err());
        assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_video_downloads_once_then_reuses_original_and_thumbnail() {
        let root = std::env::temp_dir().join(format!("line-missing-video-{}", std::process::id()));
        std::fs::create_dir_all(root.join("cache/media")).unwrap();
        std::fs::create_dir_all(root.join("cache/transfer")).unwrap();
        std::fs::write(root.join("cache/media/123.thumb.jpg"), b"thumbnail").unwrap();
        let transferred = root.join("cache/transfer/video.part");
        std::fs::write(&transferred, b"downloaded video").unwrap();
        let (mut child, replies) = (
            Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
            Replies::default(),
        );
        let stdout = child.stdout.take().unwrap();
        let waiting = replies.clone();
        let responder = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let id = request["id"].as_u64().unwrap();
            waiting
                .lock()
                .unwrap()
                .remove(&id)
                .unwrap()
                .send(Response {
                    id: Some(id),
                    ok: Some(true),
                    result: Some(json!({"path":transferred,"mime":"video/mp4"})),
                    error: None,
                    event: None,
                    extra: Value::Null,
                })
                .unwrap();
            assert_eq!(request["method"], "bridge_download");
        });
        let (events, _) = async_channel::unbounded();
        let ipc = Ipc {
            stdin: Arc::new(Mutex::new(child.stdin.take())),
            replies,
            ids: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            events,
            data: root.clone(),
            asset_at: Mutex::new(None),
        };
        let row = json!({"id":"123","chatMid":"u1","contentType":"VIDEO","filePath":root.join("cache/media/123.webm")});
        let loaded = ipc.materialize(&row, 1);
        let _ = child.kill();
        let _ = child.wait();
        responder.join().unwrap();
        let loaded = loaded.unwrap();
        assert_eq!(ipc.ids.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read(loaded["filePath"].as_str().unwrap()).unwrap(),
            b"downloaded video"
        );
        *ipc.stdin.lock().unwrap() = None;
        let again = ipc.materialize(&loaded, 1).unwrap();
        assert_eq!(again["filePath"], loaded["filePath"]);
        assert_eq!(ipc.ids.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read(again["imagePath"].as_str().unwrap()).unwrap(),
            b"thumbnail"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsafe_video_cache_candidates_are_not_reused() {
        use std::os::unix::fs::symlink;
        let root =
            std::env::temp_dir().join(format!("line-video-cache-paths-{}", std::process::id()));
        std::fs::create_dir_all(root.join("cache/media")).unwrap();
        std::fs::create_dir_all(root.join("cache/transfer")).unwrap();
        let outside = root.join("outside.webm");
        std::fs::write(&outside, b"private").unwrap();
        let link = root.join("cache/media/123.webm");
        symlink(&outside, &link).unwrap();
        let zero = root.join("cache/media/empty.mov");
        std::fs::write(&zero, []).unwrap();
        let folder = root.join("cache/media/directory.mkv");
        std::fs::create_dir(&folder).unwrap();
        let transfer = root.join("cache/transfer/not-final.mp4");
        std::fs::write(&transfer, b"unfinished").unwrap();
        let (events, _) = async_channel::unbounded();
        let ipc = Ipc {
            stdin: Arc::new(Mutex::new(None)),
            replies: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            events,
            data: root.clone(),
            asset_at: Mutex::new(None),
        };
        for path in [&outside, &link, &zero, &folder, &transfer] {
            assert!(ipc.cached_media_file(path).is_none());
        }
        // Even the legacy filename search must reject the symlink, not merely filePath.
        let result = ipc.materialize(&json!({"id":"123","chatMid":"u1","contentType":"VIDEO"}), 1);
        assert_eq!(result.unwrap_err().to_string(), "protocol unavailable");
        assert_eq!(std::fs::read(&outside).unwrap(), b"private");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn downloaded_documents_keep_preview_extensions_and_hit_cache_without_rpc() {
        for (index, (name, mime, ext)) in [
            ("報告.PDF", "application/octet-stream", "pdf"),
            ("notes.txt", "application/octet-stream", "txt"),
            ("../../report.pdf", "application/pdf", "pdf"),
            ("notes.txt/../../escape", "application/octet-stream", "bin"),
            ("document", "application/pdf", "pdf"),
            ("notes", "text/plain; charset=utf-8", "txt"),
        ]
        .into_iter()
        .enumerate()
        {
            let root = std::env::temp_dir().join(format!(
                "line-document-cache-{}-{index}",
                std::process::id()
            ));
            std::fs::create_dir_all(root.join("cache/transfer")).unwrap();
            let transferred = root.join("cache/transfer/document.part");
            std::fs::write(&transferred, b"original document bytes").unwrap();
            let mut child = Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let replies: Replies = Arc::default();
            let waiting = replies.clone();
            let responder = std::thread::spawn(move || {
                let mut line = String::new();
                BufReader::new(stdout).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let id = request["id"].as_u64().unwrap();
                waiting
                    .lock()
                    .unwrap()
                    .remove(&id)
                    .unwrap()
                    .send(Response {
                        id: Some(id),
                        ok: Some(true),
                        result: Some(json!({"path":transferred,"mime":mime})),
                        error: None,
                        event: None,
                        extra: Value::Null,
                    })
                    .unwrap();
                assert_eq!(request["method"], "bridge_download");
            });
            let (events, _) = async_channel::unbounded();
            let ipc = Ipc {
                stdin: Arc::new(Mutex::new(child.stdin.take())),
                replies,
                ids: Arc::new(AtomicU64::new(1)),
                generation: Arc::new(AtomicU64::new(1)),
                events,
                data: root.clone(),
                asset_at: Mutex::new(None),
            };
            let row = json!({"id":"123","contentType":"FILE","fileName":name});
            let loaded = ipc.materialize(&row, 1);
            let _ = child.kill();
            let _ = child.wait();
            responder.join().unwrap();
            let mut loaded = loaded.unwrap();
            let dest = root.join(format!("cache/media/123.{ext}"));
            assert_eq!(loaded["filePath"], json!(dest));
            assert_eq!(std::fs::read(&dest).unwrap(), b"original document bytes");
            assert_eq!(loaded["fileName"], name);
            *ipc.stdin.lock().unwrap() = None;
            assert_eq!(
                ipc.materialize(&loaded, 1).unwrap()["filePath"],
                json!(dest)
            );
            // Filename-derived paths can also be recovered without saved metadata.
            if name.contains('.') {
                loaded["filePath"] = Value::Null;
                assert_eq!(
                    ipc.materialize(&loaded, 1).unwrap()["filePath"],
                    json!(dest)
                );
            }
            assert_eq!(ipc.ids.load(Ordering::SeqCst), 2);
            assert!(!root.join("report.pdf").exists());
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn old_bin_document_is_renamed_locally_and_escaping_named_cache_is_rejected() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("line-bin-document-{}", std::process::id()));
        std::fs::create_dir_all(root.join("cache/media")).unwrap();
        let old = root.join("cache/media/123.bin");
        std::fs::write(&old, b"cached pdf").unwrap();
        let outside = root.join("private.txt");
        std::fs::write(&outside, b"private").unwrap();
        symlink(&outside, root.join("cache/media/456.txt")).unwrap();
        let (events, _) = async_channel::unbounded();
        let ipc = Ipc {
            stdin: Arc::new(Mutex::new(None)),
            replies: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            events,
            data: root.clone(),
            asset_at: Mutex::new(None),
        };
        let loaded = ipc
            .materialize(
                &json!({"id":"123","contentType":"FILE","fileName":"report.pdf","filePath":old}),
                1,
            )
            .unwrap();
        assert!(loaded["filePath"].as_str().unwrap().ends_with("123.pdf"));
        assert!(!old.exists());
        assert_eq!(ipc.ids.load(Ordering::SeqCst), 1);
        assert!(
            ipc.materialize(
                &json!({"id":"456","contentType":"FILE","fileName":"secret.txt"}),
                1
            )
            .is_err()
        );
        assert_eq!(std::fs::read(outside).unwrap(), b"private");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rust_upload_events_reach_the_existing_ui_protocol_variant() {
        let (events, rx) = async_channel::unbounded();
        let ipc = Ipc {
            stdin: Arc::new(Mutex::new(None)),
            replies: Arc::default(),
            ids: Arc::new(AtomicU64::new(1)),
            generation: Arc::new(AtomicU64::new(1)),
            events,
            data: PathBuf::new(),
            asset_at: Mutex::new(None),
        };
        for (progress, label, done) in [
            (0.35, "Uploading…", false),
            (1.0, "Sent", true),
            (0.0, "", true),
        ] {
            ipc.emit(line_rust_core::attachments::upload_progress(
                "c-group", progress, label, done,
            ));
            match rx.try_recv().unwrap() {
                ProtocolEvent::UploadProgress {
                    chat_mid,
                    progress: p,
                    label: l,
                    done: d,
                } => {
                    assert_eq!(chat_mid, "c-group");
                    assert_eq!(p, progress);
                    assert_eq!(l, label);
                    assert_eq!(d, done);
                }
                other => panic!("wrong UI event: {other:?}"),
            }
        }
    }

    #[test]
    fn real_attachment_pipeline_reports_stages_and_cleans_staging_on_rpc_failure() {
        for fail in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "line-upload-progress-{}-{fail}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let source = root.join("document.txt");
            std::fs::write(&source, b"attachment content").unwrap();
            let (events, rx) = async_channel::unbounded();
            let mut child = Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let replies: Replies = Arc::default();
            let waiting = replies.clone();
            let responder = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut responses = 0;
                loop {
                    let waiter = {
                        let mut map = waiting.lock().unwrap();
                        map.keys()
                            .next()
                            .copied()
                            .and_then(|id| map.remove(&id).map(|tx| (id, tx)))
                    };
                    if let Some((id, tx)) = waiter {
                        tx.send(Response {
                            id: Some(id),
                            ok: Some(!fail),
                            result: Some(json!({"rawMessage":{"id":"123","contentMetadata":{}}})),
                            error: fail.then(|| "upload rejected".into()),
                            event: None,
                            extra: Value::Null,
                        })
                        .unwrap();
                        responses += 1;
                        if !fail || responses == 2 {
                            break;
                        }
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "attachment never reached upload RPC"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            let ipc = Ipc {
                stdin: Arc::new(Mutex::new(child.stdin.take())),
                replies,
                ids: Arc::new(AtomicU64::new(1)),
                generation: Arc::new(AtomicU64::new(1)),
                events,
                data: root.clone(),
                asset_at: Mutex::new(None),
            };
            let result = ipc.application(
                "send_media",
                json!({"chatMid":"u1","account":"me","filePath":source,"oType":"file"}),
                1,
            );
            responder.join().unwrap();
            let _ = child.kill();
            let _ = child.wait();
            assert_eq!(result.is_err(), fail);
            let mut stages = Vec::new();
            while let Ok(event) = rx.try_recv() {
                if let ProtocolEvent::UploadProgress {
                    chat_mid,
                    progress,
                    done,
                    ..
                } = event
                {
                    assert_eq!(chat_mid, "u1");
                    assert!(!done);
                    stages.push(progress);
                } else {
                    panic!("unexpected event");
                }
            }
            assert_eq!(
                stages,
                if fail {
                    vec![0.05, 0.10, 0.35, 0.40]
                } else {
                    vec![0.05, 0.10, 0.35, 0.90]
                }
            );
            assert_eq!(
                std::fs::read_dir(root.join("cache/transfer"))
                    .unwrap()
                    .count(),
                0
            );
            if !fail {
                assert_eq!(
                    std::fs::read(root.join("cache/media/123.txt")).unwrap(),
                    b"attachment content"
                );
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn encrypted_upload_fallback_preserves_original_and_one_terminal_progress() {
        for both_fail in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "line-e2ee-fallback-{}-{both_fail}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let source = root.join("original.txt");
            std::fs::write(&source, b"encrypted attachment").unwrap();
            let mut child = Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let replies: Replies = Arc::default();
            let waiting = replies.clone();
            let transfer = root.join("cache/transfer");
            let responder = std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut original = Value::Null;
                for (attempt, method) in ["bridge_send_media", "bridge_send_media_e2ee_fallback"]
                    .into_iter()
                    .enumerate()
                {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let request: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(request["method"], method);
                    let p = request["params"].clone();
                    assert_eq!(
                        std::fs::read(transfer.join(p["transferName"].as_str().unwrap())).unwrap(),
                        b"encrypted attachment"
                    );
                    if attempt == 0 {
                        original = p;
                    } else {
                        assert_eq!(p, original);
                    }
                    let id = request["id"].as_u64().unwrap();
                    let success = attempt == 1 && !both_fail;
                    waiting
                        .lock()
                        .unwrap()
                        .remove(&id)
                        .unwrap()
                        .send(Response {
                            id: Some(id),
                            ok: Some(success),
                            result: Some(json!({"rawMessage":{"id":"123","contentMetadata":{}}})),
                            error: (!success).then(|| format!("encrypted path {attempt} failed")),
                            event: None,
                            extra: Value::Null,
                        })
                        .unwrap();
                }
            });
            let (events, rx) = async_channel::unbounded();
            let generation = Arc::new(AtomicU64::new(1));
            let db = root.join("cache.sqlite3");
            let ipc = Ipc {
                stdin: Arc::new(Mutex::new(child.stdin.take())),
                replies,
                ids: Arc::new(AtomicU64::new(1)),
                generation: generation.clone(),
                events,
                data: root.clone(),
                asset_at: Mutex::new(None),
            };
            let engine = Engine::new(
                Store::open(&db).unwrap(),
                ipc,
                generation.clone(),
                root.join("legacy"),
            );
            let (tx, commands) = mpsc::channel();
            let worker = std::thread::spawn(move || engine.run(commands));
            tx.send(CoreCommand::Session {
                account: "me".into(),
                generation: 1,
            })
            .unwrap();
            tx.send(CoreCommand::Operation {
                id: 42,
                method: "send_media".into(),
                params: json!({"chatMid":"u1","filePath":source,"oType":"file"}),
                generation: 1,
            })
            .unwrap();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut terminal = 0;
                let mut retries = 0;
                loop {
                    let event = loop {
                        if let Ok(event) = rx.try_recv() {
                            break event;
                        }
                        assert!(
                            std::time::Instant::now() < deadline,
                            "upload did not finish"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    };
                    match event {
                        ProtocolEvent::UploadProgress {
                            done: true,
                            progress,
                            ..
                        } => {
                            terminal += 1;
                            assert_eq!(progress, if both_fail { 0.0 } else { 1.0 });
                        }
                        ProtocolEvent::UploadProgress {
                            done: false,
                            progress: 0.4,
                            ..
                        } => retries += 1,
                        ProtocolEvent::Response {
                            id: 42, ok, error, ..
                        } => {
                            assert_eq!(ok, !both_fail);
                            assert_eq!(terminal, 1);
                            assert_eq!(retries, 1);
                            if both_fail {
                                assert_eq!(error.as_deref(), Some("encrypted path 1 failed"));
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                responder.join().unwrap();
            }));
            tx.send(CoreCommand::Stop).unwrap();
            let _ = child.kill();
            let _ = child.wait();
            if outcome.is_err() {
                generation.fetch_add(1, Ordering::SeqCst);
            }
            worker.join().unwrap();
            if let Err(e) = outcome {
                std::panic::resume_unwind(e);
            }
            assert_eq!(std::fs::read(&source).unwrap(), b"encrypted attachment");
            assert_eq!(
                std::fs::read_dir(root.join("cache/transfer"))
                    .unwrap()
                    .count(),
                0
            );
            let cached = root.join("cache/media/123.txt");
            assert_eq!(cached.is_file(), !both_fail);
            if !both_fail {
                assert_eq!(std::fs::read(cached).unwrap(), b"encrypted attachment");
            }
            let mut store = Store::open(&db).unwrap();
            store.set_account("me");
            assert_eq!(store.message("u1", "123").unwrap().is_some(), !both_fail);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn optional_preview_and_duration_failures_still_upload_and_finish_successfully() {
        struct PreviewHost {
            ipc: Ipc,
            program: PathBuf,
            probe: PathBuf,
        }
        impl Transport for PreviewHost {
            fn call(&self, method: &str, p: Value, generation: u64) -> Result<Value> {
                self.ipc.call(method, p, generation)
            }
            fn emit(&self, event: Value) {
                self.ipc.emit(event);
            }
            fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
                Ok(row.clone())
            }
            fn application(&self, method: &str, p: Value, generation: u64) -> Result<Value> {
                assert_eq!(method, "send_media");
                self.ipc.send_attachment_with_tools(
                    &p,
                    generation,
                    |source, dest| Ipc::thumb_with_program(&self.program, source, dest),
                    &self.probe,
                )
            }
        }
        let mut scenarios = Vec::new();
        for mode in ["missing", "failed", "empty", "success"] {
            for (kind, extension) in [("image", "png"), ("gif", "gif"), ("video", "mp4")] {
                scenarios.push((mode, kind, extension, "unused", Some(3200), 3200));
            }
        }
        for probe in [
            "missing", "failed", "invalid", "empty", "nan", "infinite", "negative", "zero",
            "overflow", "success",
        ] {
            for (kind, extension) in [("audio", "m4a"), ("video", "mp4")] {
                scenarios.push((
                    "missing",
                    kind,
                    extension,
                    probe,
                    None,
                    if probe == "success" { 2346 } else { 1919 },
                ));
            }
        }
        for (mode, kind, extension, probe_mode, provided_duration, expected_duration) in scenarios {
            let root = std::env::temp_dir().join(format!(
                "line-optional-tools-{}-{mode}-{kind}-{probe_mode}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let source = root.join(format!("original.{extension}"));
            std::fs::write(&source, b"original attachment bytes").unwrap();
            let program = root.join("fake-ffmpeg");
            let probe = root.join("fake-ffprobe");
            if !matches!(probe_mode, "missing" | "unused") {
                use std::os::unix::fs::PermissionsExt;
                let (output, status) = match probe_mode {
                    "failed" => ("3.2", 1),
                    "invalid" => ("N/A", 0),
                    "empty" => ("", 0),
                    "nan" => ("NaN", 0),
                    "infinite" => ("inf", 0),
                    "negative" => ("-2", 0),
                    "zero" => ("0", 0),
                    "overflow" => ("1e30", 0),
                    _ => ("2.3456", 0),
                };
                std::fs::write(
                    &probe,
                    format!("#!/bin/sh\nprintf '%s\\n' '{output}'\nexit {status}\n"),
                )
                .unwrap();
                std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            if mode != "missing" {
                use std::os::unix::fs::PermissionsExt;
                let script = match mode {
                    "failed" => {
                        "#!/bin/sh\nfor last; do :; done\nprintf partial > \"$last\"\nexit 1\n"
                    }
                    "empty" => "#!/bin/sh\nfor last; do :; done\n: > \"$last\"\n",
                    _ => "#!/bin/sh\nfor last; do :; done\nprintf preview > \"$last\"\n",
                };
                std::fs::write(&program, script).unwrap();
                std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let mut child = Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let replies: Replies = Arc::default();
            let waiting = replies.clone();
            let transfer = root.join("cache/transfer");
            let expected = mode == "success";
            let responder = std::thread::spawn(move || {
                let mut line = String::new();
                BufReader::new(stdout).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "bridge_send_media");
                let p = &request["params"];
                assert_eq!(p["oType"], kind);
                assert_eq!(p["durationMs"], expected_duration);
                assert_eq!(
                    std::fs::read(transfer.join(p["transferName"].as_str().unwrap())).unwrap(),
                    b"original attachment bytes"
                );
                if expected {
                    assert_eq!(
                        std::fs::read(transfer.join(p["previewName"].as_str().unwrap())).unwrap(),
                        b"preview"
                    );
                } else {
                    assert!(p["previewName"].is_null());
                    assert!(
                        !transfer.join("preview-1.jpg").exists(),
                        "partial preview must be removed before upload"
                    );
                }
                let id = request["id"].as_u64().unwrap();
                waiting
                    .lock()
                    .unwrap()
                    .remove(&id)
                    .unwrap()
                    .send(Response {
                        id: Some(id),
                        ok: Some(true),
                        result: Some(json!({"rawMessage":{"id":"123","contentMetadata":{}}})),
                        error: None,
                        event: None,
                        extra: Value::Null,
                    })
                    .unwrap();
            });
            let (events, rx) = async_channel::unbounded();
            let generation = Arc::new(AtomicU64::new(1));
            let ipc = Ipc {
                stdin: Arc::new(Mutex::new(child.stdin.take())),
                replies,
                ids: Arc::new(AtomicU64::new(1)),
                generation: generation.clone(),
                events,
                data: root.clone(),
                asset_at: Mutex::new(None),
            };
            let db = root.join("cache.sqlite3");
            let engine = Engine::new(
                Store::open(&db).unwrap(),
                PreviewHost {
                    ipc,
                    program,
                    probe,
                },
                generation.clone(),
                root.join("legacy"),
            );
            let (tx, commands) = mpsc::channel();
            let worker = std::thread::spawn(move || engine.run(commands));
            tx.send(CoreCommand::Session {
                account: "me".into(),
                generation: 1,
            })
            .unwrap();
            let mut params = json!({"chatMid":"u1","filePath":source,"oType":kind});
            if let Some(duration) = provided_duration {
                params["durationMs"] = json!(duration);
            }
            tx.send(CoreCommand::Operation {
                id: 42,
                method: "send_media".into(),
                params,
                generation: 1,
            })
            .unwrap();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut completed = 0;
                loop {
                    let event = loop {
                        if let Ok(event) = rx.try_recv() {
                            break event;
                        }
                        assert!(std::time::Instant::now() < deadline, "no upload response");
                        std::thread::sleep(Duration::from_millis(5));
                    };
                    match event {
                        ProtocolEvent::UploadProgress {
                            done: true,
                            progress,
                            ..
                        } => {
                            assert_eq!(progress, 1.0);
                            completed += 1;
                        }
                        ProtocolEvent::Response {
                            id: 42,
                            ok,
                            result,
                            error,
                        } => {
                            assert!(ok, "{mode}/{kind}: {error:?}");
                            assert_eq!(completed, 1);
                            assert_eq!(result["message"]["imagePath"].is_null(), !expected);
                            if matches!(kind, "audio" | "video") {
                                assert_eq!(result["message"]["durationMs"], expected_duration);
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                responder.join().unwrap();
            }));
            tx.send(CoreCommand::Stop).unwrap();
            // In a failed assertion, disconnect the adapter to unblock any RPC wait.
            let _ = child.kill();
            let _ = child.wait();
            if outcome.is_err() {
                generation.fetch_add(1, Ordering::SeqCst);
            }
            worker.join().unwrap();
            if let Err(error) = outcome {
                std::panic::resume_unwind(error);
            }
            let cached = if kind == "audio" {
                root.join(format!("cache/media/123.{extension}"))
            } else {
                let suffix = if kind == "video" {
                    extension.to_owned()
                } else {
                    format!("full.{extension}")
                };
                root.join(format!("cache/media/u1/1970-01-01_000000_000_123.{suffix}"))
            };
            assert_eq!(std::fs::read(cached).unwrap(), b"original attachment bytes");
            assert_eq!(std::fs::read(source).unwrap(), b"original attachment bytes");
            assert_eq!(
                std::fs::read_dir(root.join("cache/transfer"))
                    .unwrap()
                    .count(),
                0
            );
            let mut store = Store::open(&db).unwrap();
            store.set_account("me");
            let saved = store.message("u1", "123").unwrap().unwrap();
            if matches!(kind, "audio" | "video") {
                assert_eq!(saved["durationMs"], expected_duration);
            }
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
