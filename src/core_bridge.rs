use anyhow::{Context, Result, bail};
use async_channel::Sender;
use line_gtk_core::{
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
            stdin,
            replies: replies.clone(),
            ids,
            generation: generation.clone(),
            events,
            data: data.to_owned(),
            asset_at: Mutex::new(None),
        };
        let engine = Engine::new(store, ipc, generation.clone(), cache.join("messages"));
        std::thread::Builder::new()
            .name("line-rust-core".into())
            .spawn(move || engine.run(rx))?;
        Ok(Self {
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
        Ok(path)
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
        self.active(generation)?;
        let id = row["id"].as_str().context("media ID missing")?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            bail!("invalid media ID");
        }
        let kind = row["contentType"].as_str().unwrap_or("FILE");
        if kind == "STICKER" {
            return self.sticker_media(row, generation);
        }
        let mut updated = row.clone();
        let dir = self.data.join("cache/media");
        crate::config::ensure_private_dir(&dir)?;
        let mut file = row["filePath"]
            .as_str()
            .map(PathBuf::from)
            .and_then(|p| self.safe_cache_file(&p).ok());
        if kind == "VIDEO"
            && file
                .as_ref()
                .is_some_and(|p| p.extension().and_then(|s| s.to_str()) != Some("mp4"))
        {
            file = None;
        }
        if file.is_none() {
            // Recover binaries downloaded before the SQLite migration.
            for ext in [
                "mp4",
                "full.jpg",
                "full.png",
                "full.webp",
                "full.gif",
                "m4a",
                "mp3",
                "ogg",
                "bin",
            ] {
                let candidate = dir.join(format!("{id}.{ext}"));
                if (kind == "VIDEO" && ext != "mp4")
                    || (kind == "IMAGE" && !ext.starts_with("full."))
                {
                    continue;
                }
                if candidate.is_file() {
                    file = Some(candidate);
                    break;
                }
            }
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
            let mime = downloaded["mime"].as_str().unwrap_or_default();
            let ext = if mime.contains("png") {
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
            if ![
                "jpg", "png", "gif", "webp", "mp4", "m4a", "mp3", "ogg", "bin",
            ]
            .contains(&ext)
            {
                bail!("invalid media extension");
            }
            let dest = dir.join(if kind == "IMAGE" {
                format!("{id}.full.{ext}")
            } else {
                format!("{id}.{ext}")
            });
            std::fs::rename(temporary, &dest)?;
            file = Some(dest);
        }
        let file = file.context("media unavailable")?;
        updated["filePath"] = json!(file);
        updated["needsMedia"] = json!(false);
        match kind {
            "IMAGE" if file.extension().and_then(|s| s.to_str()) == Some("gif") => {
                updated["imagePath"] = json!(file);
            }
            "IMAGE" | "VIDEO" => {
                let thumb = dir.join(format!("{id}.thumb.jpg"));
                if !thumb.is_file() {
                    self.thumb(&file, &thumb)?;
                }
                updated["imagePath"] = json!(thumb);
            }
            "STICKER" => updated["imagePath"] = json!(file),
            "AUDIO" => updated["audioPath"] = json!(file),
            _ => {}
        }
        Ok(updated)
    }

    fn emit(&self, event: Value) {
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
                Some("upload_progress") => Some(ProtocolEvent::UploadProgress {
                    chat_mid,
                    progress: event["progress"].as_f64().unwrap_or(0.0),
                    label: event["label"].as_str().unwrap_or_default().to_owned(),
                    done: event["done"].as_bool().unwrap_or(false),
                }),
                Some("message") => serde_json::from_value(event["message"].clone())
                    .ok()
                    .map(ProtocolEvent::Message),
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
    fn attachment_types_reach_rpc_preview_and_saved_message() {
        for (index, (filename, requested, kind, mime)) in [
            ("photo.BMP", "auto", "image", "image/bmp"),
            ("photo.HEIC", "auto", "image", "image/jpeg"),
            ("clip.M4V", "auto", "video", "video/mp4"),
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
            std::fs::remove_dir_all(root).unwrap();
        }
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
            ipc.emit(line_gtk_core::attachments::upload_progress(
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
            assert_eq!(
                std::fs::read(root.join(format!("cache/media/123.{extension}"))).unwrap(),
                b"original attachment bytes"
            );
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
