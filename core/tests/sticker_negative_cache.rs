use anyhow::bail;
use line_gtk_core::sticker_cache::{self, Fetch};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "line-sticker-miss-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn confirmed_miss_reuses_static_across_repeated_loads_and_restart() {
    let root = Temp::new();
    let mut requests = Vec::new();
    let image = sticker_cache::load(&root.0, "123", true, Some(100), 1000, |animation, path| {
        requests.push(animation);
        if animation {
            Ok(Fetch::Missing)
        } else {
            std::fs::write(path, b"static")?;
            Ok(Fetch::Saved)
        }
    })
    .unwrap();
    assert_eq!(requests, vec![true, false]);
    assert_eq!(std::fs::read(&image).unwrap(), b"static");
    assert!(root.0.join("123.anim.miss").is_file());
    // No retained in-memory state: a new directory handle has exactly the same result.
    for time in [1001, 1050, 1099] {
        let restarted = PathBuf::from(root.0.to_string_lossy().to_string());
        let cached = sticker_cache::load(&restarted, "123", true, Some(100), time, |_, _| {
            panic!("unexpected repeated network request")
        })
        .unwrap();
        assert_eq!(cached, image);
    }
}

#[test]
fn expired_miss_retries_animation_and_success_removes_marker() {
    let root = Temp::new();
    std::fs::write(root.0.join("1.png"), b"static").unwrap();
    sticker_cache::load(&root.0, "1", true, Some(100), 1000, |animation, _| {
        assert!(animation);
        Ok(Fetch::Missing)
    })
    .unwrap();
    let mut attempts = 0;
    let animated = sticker_cache::load(&root.0, "1", true, Some(100), 1100, |animation, path| {
        assert!(animation);
        attempts += 1;
        std::fs::write(path, b"animation")?;
        Ok(Fetch::Saved)
    })
    .unwrap();
    assert_eq!(attempts, 1);
    assert!(animated.ends_with("1.anim.png"));
    assert!(!root.0.join("1.anim.miss").exists());
    assert_eq!(
        sticker_cache::load(&root.0, "1", true, Some(100), 5000, |_, _| panic!(
            "already downloaded"
        ))
        .unwrap(),
        animated
    );
}

#[test]
fn transient_failures_do_not_poison_cache_and_static_is_still_shown() {
    let root = Temp::new();
    std::fs::write(root.0.join("1.png"), b"static").unwrap();
    for message in [
        "timeout",
        "HTTP 403",
        "HTTP 429",
        "HTTP 500",
        "connection reset",
    ] {
        let mut attempted = false;
        let image = sticker_cache::load(&root.0, "1", true, Some(100), 1000, |animation, _| {
            assert!(animation);
            attempted = true;
            bail!("{message}")
        })
        .unwrap();
        assert!(attempted);
        assert!(image.ends_with("1.png"));
        assert!(!root.0.join("1.anim.miss").exists());
    }
    sticker_cache::load(&root.0, "1", true, Some(100), 1000, |_, _| {
        Ok(Fetch::Missing)
    })
    .unwrap();
    sticker_cache::load(&root.0, "1", true, Some(100), 1100, |_, _| {
        bail!("temporary after expiry")
    })
    .unwrap();
    assert!(!root.0.join("1.anim.miss").exists());
    let mut retried = false;
    sticker_cache::load(&root.0, "1", true, Some(100), 1101, |_, _| {
        retried = true;
        Ok(Fetch::Missing)
    })
    .unwrap();
    assert!(retried);
    for status in [404, 410] {
        assert!(sticker_cache::confirms_absence(status));
    }
    for status in [200, 301, 403, 408, 429, 500, 503] {
        assert!(!sticker_cache::confirms_absence(status));
    }
}

#[test]
fn invalid_legacy_markers_revalidate_and_static_requests_never_probe_animation() {
    let root = Temp::new();
    std::fs::write(root.0.join("1.png"), b"static").unwrap();
    for bytes in [
        b"".as_slice(),
        b"not-json",
        br#"{"version":1,"confirmed_missing_at":1000}"#,
        br#"{"version":1,"confirmed_missing_at":9999}"#,
    ] {
        std::fs::write(root.0.join("1.anim.miss"), bytes).unwrap();
        let mut requests = 0;
        sticker_cache::load(&root.0, "1", true, None, 1000, |animation, _| {
            assert!(animation);
            requests += 1;
            Ok(Fetch::Missing)
        })
        .unwrap();
        assert_eq!(requests, 1);
    }
    sticker_cache::load(&root.0, "1", true, None, u64::MAX, |_, _| {
        panic!("forever marker")
    })
    .unwrap();
    sticker_cache::load(&root.0, "1", false, Some(1), 10000, |_, _| {
        panic!("static cache is sufficient")
    })
    .unwrap();
    assert!(
        sticker_cache::load(&root.0, "../bad", true, None, 1000, |_, _| panic!(
            "invalid ID"
        ))
        .is_err()
    );
    assert_eq!(sticker_cache::animation_miss_ttl("day"), Some(86_400_000));
    assert_eq!(
        sticker_cache::animation_miss_ttl("week"),
        Some(7 * 86_400_000)
    );
    assert_eq!(
        sticker_cache::animation_miss_ttl("smart"),
        Some(7 * 86_400_000)
    );
    assert_eq!(
        sticker_cache::animation_miss_ttl("month"),
        Some(30 * 86_400_000)
    );
    assert_eq!(sticker_cache::animation_miss_ttl("forever"), None);
}

#[test]
fn alternate_static_and_animated_paths_succeed_after_preferred_path_is_missing() {
    for animated in [true, false] {
        for success_at in [1, 2] {
            let root = Temp::new();
            let mut requested = Vec::new();
            let candidates = sticker_cache::candidate_urls("123", animated).unwrap();
            let image = sticker_cache::load(
                &root.0,
                "123",
                animated,
                Some(100),
                1000,
                |animation, path| {
                    assert_eq!(animation, animated);
                    sticker_cache::fetch_candidates("123", animation, |url| {
                        let index = requested.len();
                        requested.push(url.to_owned());
                        if index == success_at {
                            std::fs::write(path, b"fallback image")?;
                            Ok(Fetch::Saved)
                        } else {
                            Ok(Fetch::Missing)
                        }
                    })
                },
            )
            .unwrap();
            assert_eq!(requested, candidates[..=success_at]);
            assert_eq!(std::fs::read(&image).unwrap(), b"fallback image");
            assert!(!root.0.join("123.anim.miss").exists());
            sticker_cache::load(&root.0, "123", animated, Some(100), 1001, |_, _| {
                panic!("cached success must not refetch")
            })
            .unwrap();
        }
    }
    let static_urls = sticker_cache::candidate_urls("123", false).unwrap();
    assert!(static_urls[0].ends_with("/android/sticker.png"));
    assert!(static_urls[1].ends_with("/iPhone/sticker@2x.png"));
    assert!(static_urls[2].ends_with("/ANDROID/sticker.png"));
    let animated_urls = sticker_cache::candidate_urls("123", true).unwrap();
    assert!(animated_urls[0].ends_with("/android/sticker_animation.png"));
    assert!(animated_urls[1].ends_with("/ANDROID/sticker_animation.png"));
    assert!(animated_urls[2].ends_with("/iPhone/sticker_animation@2x.png"));
}

#[test]
fn only_all_missing_candidates_create_negative_cache() {
    for transient_at in [None, Some(0), Some(1), Some(2)] {
        let root = Temp::new();
        std::fs::write(root.0.join("123.png"), b"static").unwrap();
        let mut requests = 0;
        let image = sticker_cache::load(&root.0, "123", true, Some(100), 1000, |animated, _| {
            assert!(animated);
            sticker_cache::fetch_candidates("123", animated, |_| {
                let index = requests;
                requests += 1;
                if Some(index) == transient_at {
                    bail!("HTTP 503");
                }
                Ok(Fetch::Missing)
            })
        })
        .unwrap();
        assert_eq!(requests, 3);
        assert!(image.ends_with("123.png"));
        assert_eq!(
            root.0.join("123.anim.miss").exists(),
            transient_at.is_none()
        );
        if transient_at.is_none() {
            sticker_cache::load(&root.0, "123", true, Some(100), 1050, |_, _| {
                panic!("all paths already confirmed missing")
            })
            .unwrap();
        } else {
            let mut retried = 0;
            sticker_cache::load(&root.0, "123", true, Some(100), 1050, |animated, path| {
                sticker_cache::fetch_candidates("123", animated, |_| {
                    retried += 1;
                    std::fs::write(path, b"animation recovered")?;
                    Ok(Fetch::Saved)
                })
            })
            .unwrap();
            assert_eq!(retried, 1);
            assert!(!root.0.join("123.anim.miss").exists());
        }
    }
}

#[test]
fn transient_preferred_path_can_still_use_later_success_and_legacy_android_miss_is_rechecked() {
    let root = Temp::new();
    std::fs::write(
        root.0.join("123.anim.miss"),
        br#"{"version":1,"confirmed_missing_at":1000}"#,
    )
    .unwrap();
    let mut requests = 0;
    let result = sticker_cache::load(&root.0, "123", true, None, 1001, |animation, path| {
        sticker_cache::fetch_candidates("123", animation, |_| {
            requests += 1;
            if requests == 1 {
                bail!("timeout");
            }
            std::fs::write(path, b"animation")?;
            Ok(Fetch::Saved)
        })
    })
    .unwrap();
    assert_eq!(requests, 2);
    assert!(result.ends_with("123.anim.png"));
    assert!(!root.0.join("123.anim.miss").exists());
    assert!(sticker_cache::fetch_candidates("../123", true, |_| panic!("invalid ID")).is_err());
}

#[test]
fn miss_marker_does_not_suppress_missing_static_download() {
    let root = Temp::new();
    let first = sticker_cache::load(&root.0, "1", true, Some(100), 1000, |animation, _| {
        if animation {
            Ok(Fetch::Missing)
        } else {
            bail!("static unavailable")
        }
    });
    assert!(first.is_err());
    assert!(root.0.join("1.anim.miss").exists());
    let image = sticker_cache::load(&root.0, "1", true, Some(100), 1050, |animation, path| {
        assert!(!animation);
        std::fs::write(path, b"static")?;
        Ok(Fetch::Saved)
    })
    .unwrap();
    assert!(image.ends_with("1.png"));
}
