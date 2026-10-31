//! Choose the upload protocol in Rust; the adapter only executes that choice.
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub const FALLBACK_DURATION_MS: i64 = 1919;

/// Keep LINE's legacy fallback when media inspection is unavailable. Reject
/// NaN/infinity/overflow before converting to an integer sent through JSON.
pub fn duration_ms(explicit: Option<f64>, probe_seconds: impl FnOnce() -> Option<f64>) -> i64 {
    fn valid(ms: f64) -> Option<i64> {
        (ms.is_finite() && ms > 0.0 && ms <= 9_007_199_254_740_991.0)
            .then(|| ms.round().max(1.0) as i64)
    }
    explicit
        .and_then(valid)
        .or_else(|| probe_seconds().and_then(|seconds| valid(seconds * 1000.0)))
        .unwrap_or(FALLBACK_DURATION_MS)
}

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

/// Retry only through the library's E2EE implementation. A transport timeout
/// has an unknown server outcome, so do not risk resending in that case.
pub fn upload(
    params: &Value,
    mut call: impl FnMut(&str, Value) -> Result<Value>,
    mut before_retry: impl FnMut() -> Result<()>,
) -> Result<Value> {
    let method = upload_method(params["chatMid"].as_str().unwrap_or_default())?;
    match call(method, params.clone()) {
        Ok(result) => Ok(result),
        Err(error) => {
            let reason = error.to_string();
            if method != "bridge_send_media"
                || matches!(
                    reason.as_str(),
                    "session_changed"
                        | "protocol timeout"
                        | "protocol disconnected"
                        | "protocol unavailable"
                        | "bridge_busy"
                        | "not_logged_in"
                )
            {
                return Err(error);
            }
            before_retry()?;
            call("bridge_send_media_e2ee_fallback", params.clone())
        }
    }
}
/// Match the original client's precedence: recognized explicit types win,
/// except image + .gif; unknown/empty types use the filename just like auto.
pub fn normalize_media_type(requested: &str, filename: &str) -> &'static str {
    let filename = filename.to_ascii_lowercase();
    match requested.trim().to_ascii_lowercase().as_str() {
        "image" if filename.ends_with(".gif") => return "gif",
        "image" => return "image",
        "gif" => return "gif",
        "video" => return "video",
        "audio" => return "audio",
        "file" => return "file",
        _ => {}
    }
    let extension = filename.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match extension {
        "gif" => "gif",
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "heic" => "image",
        "mp4" | "webm" | "mov" | "mkv" | "m4v" => "video",
        "m4a" | "mp3" | "wav" | "ogg" | "oga" | "aac" | "flac" => "audio",
        _ => "file",
    }
}

/// Preserve the legacy LINE upload MIME table and its type-based defaults.
/// This selects protocol metadata; it does not transcode file contents.
pub fn mime(extension: &str, kind: &str) -> &'static str {
    match extension.to_ascii_lowercase().as_str() {
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
