//! Choose the upload protocol in Rust; the adapter only executes that choice.
use anyhow::{Result, bail};
use serde_json::{Value, json};

/// Stage-based progress, not a fabricated byte-transfer percentage.
pub fn upload_progress(chat: &str, progress: f64, label: &str, done: bool) -> Value {
    json!({"event":"upload_progress","chatMid":chat,"progress":progress,"label":label,"done":done})
}

pub fn upload_method(chat: &str) -> Result<&'static str> {
    if chat.is_empty() {
        bail!("missing attachment destination");
    }
    Ok(if chat.starts_with('u') || chat.starts_with('c') {
        "bridge_send_media"
    } else {
        "bridge_upload_plain_media"
    })
}
pub fn mime(extension: &str, kind: &str) -> &'static str {
    match extension {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "jpg" | "jpeg" => "image/jpeg",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "aac" | "m4a" => "audio/mp4",
        _ => match kind {
            "image" | "gif" => "image/jpeg",
            "video" => "video/mp4",
            "audio" => "audio/mp4",
            _ => "application/octet-stream",
        },
    }
}
