#[cfg(unix)]
#[test]
fn rename_preserves_auth_history_and_absolute_cache_paths() {
    use line_rust_core::data_dir::resolve;
    use std::fs;
    let root = std::env::temp_dir().join(format!("line-rust-migration-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let base = root.join("native");
    assert_eq!(resolve(&base, None).unwrap(), base.join("line-rust"));
    let old = base.join("line-gtk");
    fs::create_dir_all(old.join("protocol-state")).unwrap();
    fs::create_dir_all(old.join("cache")).unwrap();
    fs::write(old.join("protocol-state/auth-token.txt"), b"test-token").unwrap();
    fs::write(old.join("cache/media.jpg"), b"test-media").unwrap();
    let media = old.join("cache/media.jpg").canonicalize().unwrap();
    fs::write(old.join("settings.json"), media.to_str().unwrap()).unwrap();
    let selected = resolve(&base, None).unwrap();
    assert_eq!(selected, old.canonicalize().unwrap());
    assert_eq!(resolve(&base, None).unwrap(), selected);
    assert_eq!(
        fs::read(base.join("line-rust/protocol-state/auth-token.txt")).unwrap(),
        b"test-token"
    );
    assert_eq!(fs::read(media).unwrap(), b"test-media");
    let flatpak_base = root.join("flatpak/data");
    assert_eq!(resolve(&flatpak_base, Some(&old)).unwrap(), selected);
    let existing = root.join("existing");
    fs::create_dir_all(existing.join("line-rust")).unwrap();
    fs::write(existing.join("line-rust/settings.json"), b"new-profile").unwrap();
    assert_eq!(
        resolve(&existing, Some(&old)).unwrap(),
        existing.join("line-rust").canonicalize().unwrap()
    );
    assert_eq!(
        fs::read(existing.join("line-rust/settings.json")).unwrap(),
        b"new-profile"
    );
    assert!(!existing.join("line-rust/protocol-state").exists());
    let broken = root.join("broken");
    fs::create_dir_all(&broken).unwrap();
    std::os::unix::fs::symlink(root.join("missing"), broken.join("line-rust")).unwrap();
    assert!(resolve(&broken, Some(&old)).is_err());
    fs::remove_dir_all(root).unwrap();
}
