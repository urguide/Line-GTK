//! Persistent negative cache only for confirmed absent animation assets.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const DAY_MS: u64 = 86_400_000;

/// None means never expire, matching the existing Forever setting.
pub fn animation_miss_ttl(mode: &str) -> Option<u64> {
    match mode {
        "day" => Some(DAY_MS),
        "month" => Some(30 * DAY_MS),
        "forever" => None,
        _ => Some(7 * DAY_MS),
    }
}

pub fn confirms_absence(status: u16) -> bool {
    matches!(status, 404 | 410)
}

pub enum Fetch {
    Saved,
    Missing,
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
                miss.version == 1
                    && now_ms >= miss.confirmed_missing_at
                    && ttl.is_none_or(|ttl| now_ms - miss.confirmed_missing_at < ttl)
            });
        if !fresh {
            // Legacy markers did not distinguish HTTP absence from network
            // failure. Revalidate them rather than perpetuate a false miss.
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
                                version: 1,
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
