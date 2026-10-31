//! Persistent negative cache only for confirmed absent animation assets.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// None means never expire, matching the existing Forever setting.
pub fn animation_miss_ttl(mode: &str) -> Option<u64> {
    crate::cache_policy::CachePolicy::for_mode(mode).animation_miss
}

pub fn confirms_absence(status: u16) -> bool {
    matches!(status, 404 | 410)
}

pub enum Fetch {
    Saved,
    Missing,
}

/// Preserve the original CDN order and filenames, including iPhone @2x assets.
pub fn candidate_urls(id: &str, animated: bool) -> Result<Vec<String>> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid sticker ID");
    }
    let paths = if animated {
        [
            "android/sticker_animation.png",
            "ANDROID/sticker_animation.png",
            "iPhone/sticker_animation@2x.png",
        ]
    } else {
        [
            "android/sticker.png",
            "iPhone/sticker@2x.png",
            "ANDROID/sticker.png",
        ]
    };
    Ok(paths
        .iter()
        .map(|path| format!("https://stickershop.line-scdn.net/stickershop/v1/sticker/{id}/{path}"))
        .collect())
}

/// Only aggregate Missing when every URL confirmed absence. Retain an error
/// even if later paths return 404, and stop immediately when a path succeeds.
pub fn fetch_candidates(
    id: &str,
    animated: bool,
    mut fetch: impl FnMut(&str) -> Result<Fetch>,
) -> Result<Fetch> {
    let mut transient = None;
    for url in candidate_urls(id, animated)? {
        match fetch(&url) {
            Ok(Fetch::Saved) => return Ok(Fetch::Saved),
            Ok(Fetch::Missing) => {}
            Err(error) => transient = Some(error),
        }
    }
    if let Some(error) = transient {
        return Err(error);
    }
    Ok(Fetch::Missing)
}

#[derive(Serialize, Deserialize)]
struct Miss {
    version: u8,
    confirmed_missing_at: u64,
}

fn present(path: &Path) -> bool {
    path.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// All fetching remains sequential. A transient animation error falls back to
/// static without poisoning the persistent miss cache. Clock is supplied by the
/// host, permitting expiry tests without sleeping or altering filesystem times.
pub fn load(
    directory: &Path,
    id: &str,
    animated: bool,
    ttl: Option<u64>,
    now_ms: u64,
    mut fetch: impl FnMut(bool, &Path) -> Result<Fetch>,
) -> Result<PathBuf> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid sticker ID");
    }
    let animation = directory.join(format!("{id}.anim.png"));
    let marker = directory.join(format!("{id}.anim.miss"));
    if animated {
        if present(&animation) {
            let _ = std::fs::remove_file(&marker);
            return Ok(animation);
        }
        let fresh = std::fs::read(&marker)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Miss>(&bytes).ok())
            .is_some_and(|miss| {
                miss.version == 2
                    && now_ms >= miss.confirmed_missing_at
                    && ttl.is_none_or(|ttl| now_ms - miss.confirmed_missing_at < ttl)
            });
        if !fresh {
            // Untyped markers mixed network failures with absence; version 1
            // checked Android only. Both need revalidation across all paths.
            let _ = std::fs::remove_file(&marker);
            match fetch(true, &animation) {
                Ok(Fetch::Saved) if present(&animation) => return Ok(animation),
                Ok(Fetch::Missing) => {
                    let write = || -> Result<()> {
                        std::fs::create_dir_all(directory)?;
                        let temporary = marker.with_extension("miss.tmp");
                        std::fs::write(
                            &temporary,
                            serde_json::to_vec(&Miss {
                                version: 2,
                                confirmed_missing_at: now_ms,
                            })?,
                        )?;
                        std::fs::rename(temporary, &marker)?;
                        Ok(())
                    };
                    // A cache write failure must not hide an available sticker.
                    if write().is_err() {
                        let _ = std::fs::remove_file(marker.with_extension("miss.tmp"));
                    }
                }
                _ => {} // timeout, 403/429/5xx, invalid image or local I/O failure
            }
        }
    }
    let image = directory.join(format!("{id}.png"));
    if present(&image) {
        return Ok(image);
    }
    match fetch(false, &image)? {
        Fetch::Saved if present(&image) => Ok(image),
        _ => bail!("sticker image unavailable"),
    }
}
