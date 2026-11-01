use crate::core_bridge::Ipc;
use anyhow::{Context, Result, bail};
use line_rust_core::sync::Transport;
use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::Ordering;

impl Ipc {
    pub(crate) fn profile_operation(&self, p: Value, generation: u64) -> Result<Value> {
        let mid = p["mid"].as_str().context("missing profile ID")?;
        if mid.is_empty() || !mid.bytes().all(|b| b.is_ascii_alphanumeric()) {
            bail!("invalid profile ID");
        }
        let action = p["action"].as_str().context("missing profile action")?;
        if action == "get" {
            let raw = self.call("bridge_profile", p.clone(), generation)?;
            let profile = &raw["profile"];
            // Profile pictures change without changing the MID. Key this cache by
            // picture status so opening a profile never reuses an obsolete image.
            let avatar = line_rust_core::catalog::picture(profile).and_then(|picture| {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                picture.hash(&mut hash);
                let url = if picture.starts_with("https://") {
                    picture.to_owned()
                } else {
                    format!(
                        "https://profile.line-scdn.net/{}",
                        picture.trim_start_matches('/')
                    )
                };
                let dest = self
                    .data
                    .join("cache/avatars")
                    .join(format!("{mid}-{:x}.jpg", hash.finish()));
                self.asset(&url, &dest, generation).ok()
            });
            self.active(generation)?;
            return Ok(json!({"mid":mid,
                "name":line_rust_core::catalog::name(profile).unwrap_or(mid),
                "displayNameOverridden":line_rust_core::catalog::alias(profile),
                "statusMessage":profile["statusMessage"], "avatarPath":avatar,
                "picturePath":line_rust_core::catalog::picture(profile),
                "isFriend":raw["isFriend"]}));
        }
        if action == "add" {
            return self.call("bridge_profile", p, generation);
        }
        let transfer = self.data.join("cache/transfer");
        crate::config::ensure_private_dir(&transfer)?;
        if action == "cover" {
            let raw = self.call("bridge_profile", p.clone(), generation)?;
            let Some(path) = raw["path"].as_str() else {
                return Ok(json!({"mid":mid,"coverPath":null}));
            };
            let source = Path::new(path).canonicalize()?;
            if source.parent() != Some(transfer.canonicalize()?.as_path()) || !source.is_file() {
                bail!("invalid profile transfer");
            }
            let result = (|| {
                image::ImageReader::open(&source)?
                    .with_guessed_format()?
                    .into_dimensions()?;
                self.active(generation)?;
                let dir = self.data.join("cache/avatars");
                crate::config::ensure_private_dir(&dir)?;
                let dest = dir.join(format!("{mid}-cover.jpg"));
                std::fs::rename(&source, &dest)?;
                Ok(json!({"mid":mid,"coverPath":dest}))
            })();
            let _ = std::fs::remove_file(source);
            return result;
        }
        if !matches!(action, "avatar" | "background") {
            bail!("invalid profile action");
        }
        if p["account"].as_str() != Some(mid) {
            bail!("cannot edit another profile");
        }
        let source = Path::new(p["filePath"].as_str().context("missing profile image")?);
        if !source.is_file() || source.metadata()?.len() > 20 * 1024 * 1024 {
            bail!("profile image must be a file smaller than 20 MiB");
        }
        // Bound decoding and send an actual JPEG, independent of the selected extension.
        let mut reader = image::ImageReader::open(source)?.with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        limits.max_alloc = Some(256 * 1024 * 1024);
        reader.limits(limits);
        let image = reader.decode()?.thumbnail(2048, 2048).to_rgb8();
        let name = format!("profile-{}", self.ids.fetch_add(1, Ordering::SeqCst));
        let staged = transfer.join(&name);
        let result = (|| {
            image.save_with_format(&staged, image::ImageFormat::Jpeg)?;
            let result = self.call(
                "bridge_profile",
                json!({"mid":mid,"action":action,"transferName":name}),
                generation,
            )?;
            if result["updated"] != true {
                bail!("profile update not acknowledged");
            }
            self.active(generation)?;
            let dir = self.data.join("cache/avatars");
            crate::config::ensure_private_dir(&dir)?;
            let dest = dir.join(if action == "avatar" {
                format!("{mid}.jpg")
            } else {
                format!("{mid}-cover.jpg")
            });
            std::fs::rename(&staged, &dest)?;
            if action == "avatar" {
                self.emit(json!({"event":"avatar_ready","mid":mid,"avatarPath":dest}));
            }
            Ok(json!({"mid":mid,"updated":true,"path":dest}))
        })();
        let _ = std::fs::remove_file(staged);
        result
    }
}
