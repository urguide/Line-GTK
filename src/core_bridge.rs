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
        let status = Command::new("ffmpeg")
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
                        break;
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
                    vec![0.05, 0.10, 0.35]
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
}
