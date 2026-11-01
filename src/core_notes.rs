use crate::core_bridge::Ipc;
use anyhow::{Context, Result, bail};
use line_rust_core::sync::Transport;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::Ordering;

impl Ipc {
    pub(crate) fn notes_operation(&self, mut p: Value, generation: u64) -> Result<Value> {
        let action = p["action"]
            .as_str()
            .context("missing notes action")?
            .to_owned();
        let transfer = self.data.join("cache/transfer");
        crate::config::ensure_private_dir(&transfer)?;
        let mut staged = Vec::new();
        let result = (|| {
            if action == "create" {
                let paths = p["filePaths"].as_array().cloned().unwrap_or_default();
                if paths.len() > 20 {
                    bail!("notes_media_limit");
                }
                let mut files = Vec::new();
                for path in paths {
                    self.active(generation)?;
                    let path = Path::new(path.as_str().context("invalid note attachment")?);
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let kind = match ext.as_str() {
                        "jpg" | "jpeg" | "png" | "gif" | "webp" => "image",
                        "mp4" | "mov" | "m4v" | "webm" => "video",
                        _ => bail!("notes_invalid_media"),
                    };
                    if !path.is_file() {
                        bail!("note attachment unavailable");
                    }
                    let name = format!("note-{}.{ext}", self.ids.fetch_add(1, Ordering::SeqCst));
                    let dest = transfer.join(&name);
                    staged.push(dest.clone());
                    std::fs::copy(path, dest)?;
                    files.push(json!({"name":name,"type":kind}));
                }
                p["files"] = json!(files);
                p.as_object_mut().unwrap().remove("filePaths");
            }
            let raw = self.call("bridge_notes", p.clone(), generation)?;
            self.active(generation)?;
            if action == "media" {
                let source = Path::new(raw["path"].as_str().context("missing note media")?)
                    .canonicalize()?;
                if !source.starts_with(transfer.canonicalize()?)
                    || !source.is_file()
                    || source.metadata()?.len() == 0
                {
                    bail!("invalid note media transfer");
                }
                let dir = self.data.join("cache/notes");
                crate::config::ensure_private_dir(&dir)?;
                let kind = if raw["type"] == "VIDEO" {
                    "VIDEO"
                } else {
                    "IMAGE"
                };
                let ext = if kind == "VIDEO" {
                    "mp4"
                } else {
                    match raw["mime"].as_str().unwrap_or_default() {
                        "image/png" => "png",
                        "image/gif" => "gif",
                        "image/webp" => "webp",
                        _ => "jpg",
                    }
                };
                let stem = source
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .context("invalid note media name")?;
                let dest = dir.join(format!("{stem}.{ext}"));
                std::fs::rename(source, &dest)?;
                return Ok(json!({"path":dest,"contentType":kind}));
            }
            line_rust_core::notes::normalize(
                &action,
                &raw,
                p["account"].as_str().unwrap_or_default(),
            )
        })();
        for file in staged {
            let _ = std::fs::remove_file(file);
        }
        result
    }
}
