use anyhow::bail;
use line_rust_core::{
    sticker_cache::{self, Fetch},
    stickers,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "line-pack-icon-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage bytes";

#[test]
fn preferred_and_fallback_icons_use_legacy_order_then_reuse_disk() {
    for success in 0..3 {
        let root = Temp::new();
        let mut calls = Vec::new();
        let path = sticker_cache::pack_icon(&root.0, "123", |url, dest| {
            let i = calls.len();
            calls.push(url.to_owned());
            if i == success {
                std::fs::write(dest, PNG)?;
                Ok(Fetch::Saved)
            } else if i == 0 {
                Ok(Fetch::Missing)
            } else {
                bail!("transient error")
            }
        })
        .unwrap();
        let expected =
            ["android/main.png", "ANDROID/main.png", "android/tab_on.png"].map(|suffix| {
                format!("https://stickershop.line-scdn.net/stickershop/v1/product/123/{suffix}")
            });
        assert_eq!(calls, expected[..=success]);
        assert_eq!(path, root.0.join("pack-123.png"));
        // Fresh invocation has no in-memory state; represents reopening/restarting.
        assert_eq!(
            sticker_cache::pack_icon(&root.0, "123", |_, _| panic!("must use disk icon")).unwrap(),
            path
        );
    }
}

#[test]
fn all_icon_candidates_fail_but_packages_and_recents_keep_first_sticker() {
    let root = Temp::new();
    let mut catalog = json!({"packs":[
        {"id":"__recent__","iconPath":"/recent.png","stickers":[]},
        {"id":"1","iconPath":"/obsolete.png","stickers":[{"imagePath":"/first.png"}]},
        {"id":"2","stickers":[{"imagePath":"/second.png"}]},
        {"id":"3","stickers":[]}
    ]});
    let mut calls = Vec::new();
    stickers::apply_pack_icons(&mut catalog, |package| {
        let path = sticker_cache::pack_icon(&root.0, package, |url, dest| {
            calls.push(url.to_owned());
            std::fs::write(dest, b"partial")?;
            bail!("CDN failed")
        })?;
        Ok(path.to_string_lossy().into_owned())
    });
    assert_eq!(calls.len(), 9);
    assert_eq!(catalog["packs"][0]["iconPath"], "/recent.png");
    assert_eq!(catalog["packs"][1]["iconPath"], "/first.png");
    assert_eq!(catalog["packs"][2]["iconPath"], "/second.png");
    assert!(catalog["packs"][3]["iconPath"].is_null());
    assert_eq!(catalog["packs"].as_array().unwrap().len(), 4);
    assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 0);
}

#[test]
fn invalid_png_cache_and_invalid_candidate_are_retried_without_poisoning_disk() {
    let root = Temp::new();
    std::fs::write(root.0.join("pack-1.png"), b"HTML error").unwrap();
    let mut calls = 0;
    let path = sticker_cache::pack_icon(&root.0, "1", |_, dest| {
        calls += 1;
        std::fs::write(
            dest,
            if calls == 1 {
                b"not PNG".as_slice()
            } else {
                PNG
            },
        )?;
        Ok(Fetch::Saved)
    })
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(std::fs::read(path).unwrap(), PNG);
    assert!(
        sticker_cache::pack_icon(&root.0, "../1", |_, _| panic!("invalid ID must not fetch"))
            .is_err()
    );
}

#[test]
fn real_catalog_prefers_pack_icon_over_first_sticker() {
    let root = Temp::new();
    let mut catalog = stickers::load(
        Ok(json!({"productList":[{"id":"1"}]})),
        &Value::Null,
        &json!([]),
        |_| Ok(br#"{"stickers":[{"id":10}],"version":1}"#.to_vec()),
        |_| Ok("/first.png".into()),
    )
    .unwrap();
    stickers::apply_pack_icons(&mut catalog, |package| {
        Ok(sticker_cache::pack_icon(&root.0, package, |_, dest| {
            std::fs::write(dest, PNG)?;
            Ok(Fetch::Saved)
        })?
        .to_string_lossy()
        .into_owned())
    });
    assert_eq!(
        catalog["packs"][0]["iconPath"],
        json!(root.0.join("pack-1.png"))
    );
    assert_eq!(
        catalog["packs"][0]["stickers"][0]["imagePath"],
        "/first.png"
    );
}
