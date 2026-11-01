use anyhow::Result;
use line_rust_core::{
    cache_policy::{CachePolicy, DAY, MINUTE, MONTH, WEEK},
    store::{Cursor, Page, Store},
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

const START: i64 = 2_000_000_000_000;
fn clock_store() -> (Store, Arc<AtomicI64>) {
    let time = Arc::new(AtomicI64::new(START));
    let clock = time.clone();
    let mut store = Store::open_with_clock(
        Path::new(":memory:"),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
    .unwrap();
    store.set_account("me");
    (store, time)
}
fn page() -> Page {
    Page {
        messages: vec![json!({"id":"1","createdTime":1})],
        next_cursor: None,
        has_more: false,
        cached: false,
    }
}

#[test]
fn every_mode_uses_expected_freshness_and_retention_values() {
    for (mode, messages, chats, contacts, disk, owned, miss) in [
        ("smart", 20, 30, 30, 360 * DAY, DAY, Some(WEEK as u64)),
        ("day", 15, 30, 30, DAY, DAY, Some(DAY as u64)),
        ("week", 30, 60, 120, WEEK, WEEK, Some(WEEK as u64)),
        ("month", 60, 120, 360, MONTH, MONTH, Some(MONTH as u64)),
        ("forever", 120, 360, 360, i64::MAX, WEEK, None),
    ] {
        let p = CachePolicy::for_mode(mode);
        assert_eq!(p.messages, messages * MINUTE);
        assert_eq!(p.chats, chats * MINUTE);
        assert_eq!(p.contacts, contacts * MINUTE);
        assert_eq!(
            p.disk_messages,
            if mode == "smart" { i64::MAX } else { disk }
        );
        assert_eq!(p.disk_chats, disk);
        assert_eq!(p.owned_stickers, owned);
        assert_eq!(p.animation_miss, miss);
    }
    assert_eq!(CachePolicy::for_mode("unknown"), CachePolicy::default());
    assert_eq!(
        CachePolicy::for_mode(" WEEK "),
        CachePolicy::for_mode("week")
    );
}

#[test]
fn sqlite_freshness_expires_at_exact_boundary_and_invalidation_keeps_disk_age() {
    for mode in ["smart", "day", "week", "month", "forever"] {
        let (mut store, time) = clock_store();
        store.set_retention(mode);
        let p = CachePolicy::for_mode(mode);
        store.save_page("chat", None, &page()).unwrap();
        for method in [
            "list_chats",
            "list_friends",
            "list_stickers",
            "owned_stickers",
        ] {
            time.store(START, Ordering::SeqCst);
            store.save_catalog(method, &json!({"value":1})).unwrap();
            let ttl = p.catalog_ttl(method);
            time.store(START + ttl - 1, Ordering::SeqCst);
            assert!(store.catalog(method, ttl).unwrap().is_some());
            time.store(START + ttl, Ordering::SeqCst);
            assert!(
                store.catalog(method, ttl).unwrap().is_none(),
                "{mode}/{method}"
            );
            time.store(START + ttl + 1, Ordering::SeqCst);
            assert!(store.catalog(method, ttl).unwrap().is_none());
        }
        time.store(START + p.messages - 1, Ordering::SeqCst);
        assert!(store.page("chat", None, p.messages).unwrap().is_some());
        time.store(START + p.messages, Ordering::SeqCst);
        assert!(store.page("chat", None, p.messages).unwrap().is_none());
        time.store(START + 1, Ordering::SeqCst);
        store.invalidate_catalogs().unwrap();
        assert!(store.catalog("list_chats", p.chats).unwrap().is_none());
        assert!(
            store
                .stale_catalog("list_chats", p.disk_chats)
                .unwrap()
                .is_some()
        );
        if p.disk_chats != i64::MAX {
            time.store(START + p.disk_chats, Ordering::SeqCst);
            assert!(
                store
                    .stale_catalog("list_chats", p.disk_chats)
                    .unwrap()
                    .is_none()
            );
        }
        if p.disk_messages != i64::MAX {
            time.store(START + p.disk_messages, Ordering::SeqCst);
            // Disk messages use age > TTL (inclusive boundary).
            assert!(store.message("chat", "1").unwrap().is_some());
            time.store(START + p.disk_messages + 1, Ordering::SeqCst);
            assert!(store.message("chat", "1").unwrap().is_none());
        } else {
            time.store(i64::MAX - 1, Ordering::SeqCst);
            assert!(store.message("chat", "1").unwrap().is_some());
        }
    }
}

#[test]
fn permanent_history_survives_new_writes_and_restart_but_allows_explicit_removal() {
    for mode in ["smart", "forever", "day", "week", "month"] {
        let path = std::env::temp_dir().join(format!(
            "line-permanent-history-{}-{mode}.sqlite",
            std::process::id()
        ));
        let time = Arc::new(AtomicI64::new(START));
        let clock = time.clone();
        let mut store =
            Store::open_with_clock(&path, Arc::new(move || clock.load(Ordering::SeqCst))).unwrap();
        store.set_account("me");
        store.set_retention(mode);
        let before = Cursor {
            message_id: "2".into(),
            delivered_time: "2".into(),
        };
        store.save_page("chat", Some(&before), &page()).unwrap();
        time.store(START + 10 * 365 * DAY, Ordering::SeqCst);
        store
            .save_messages("other-chat", &[json!({"id":"3","createdTime":3})])
            .unwrap();
        drop(store);

        let mut store =
            Store::open_with_clock(&path, Arc::new(move || time.load(Ordering::SeqCst))).unwrap();
        store.set_account("me");
        // Read with unlimited retention to verify whether cleanup actually
        // deleted the old rows, rather than merely filtering them by age.
        store.set_retention("forever");
        let saved = store.page("chat", Some(&before), 0).unwrap();
        if matches!(mode, "smart" | "forever") {
            assert_eq!(saved.unwrap().messages[0]["id"], "1", "{mode}");
            store.unsend("chat", "1").unwrap();
            assert!(store.message("chat", "1").unwrap().is_none());
            store.save_messages("chat", &page().messages).unwrap();
            assert!(store.message("chat", "1").unwrap().is_none());
        } else {
            assert!(saved.is_none(), "{mode}");
            assert!(store.message("chat", "1").unwrap().is_none());
        }
        assert!(store.message("other-chat", "3").unwrap().is_some());
        store.clear().unwrap();
        assert!(store.message("other-chat", "3").unwrap().is_none());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}

struct Fake {
    events: mpsc::Sender<Value>,
    calls: Arc<Mutex<Vec<String>>>,
}
impl Transport for Fake {
    fn call(&self, method: &str, _: Value, _: u64) -> Result<Value> {
        self.calls.lock().unwrap().push(method.into());
        Ok(if method == "bridge_fetch_page" {
            serde_json::to_value(page())?
        } else {
            json!({"productList":[{"id":"20","latestVersion":"9"}]})
        })
    }
    fn application(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        self.calls.lock().unwrap().push(method.into());
        Ok(match method {
            "send_sticker" => {
                assert_eq!(p["version"], "9");
                json!({"rawMessage":{"id":"2","contentType":"NONE"}})
            }
            "list_stickers" => json!({"packs":[],"products":[]}),
            "list_chats" => json!({"chats":[]}),
            _ => json!({"friends":[]}),
        })
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        Ok(row.clone())
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
}
#[test]
fn engine_applies_each_modes_ttl_to_pages_lists_and_ownership() {
    for mode in ["smart", "day", "week", "month", "forever"] {
        for method in [
            "fetch_messages",
            "list_chats",
            "list_friends",
            "list_stickers",
            "send_sticker",
        ] {
            for delta in [-1, 0] {
                let (mut store, time) = clock_store();
                let p = CachePolicy::for_mode(mode);
                store.save_page("chat", None, &page()).unwrap();
                store
                    .save_catalog(method, &json!({"cached_test":true}))
                    .unwrap();
                store
                    .save_catalog(
                        "owned_stickers",
                        &json!({"productList":[{"id":"20","latestVersion":"9"}]}),
                    )
                    .unwrap();
                let ttl = match method {
                    "fetch_messages" => p.messages,
                    "send_sticker" => p.owned_stickers,
                    _ => p.catalog_ttl(method),
                };
                // Query at exact expiry. If the engine still uses hardcoded larger
                // TTLs (or inclusive comparisons), this assertion catches the hit.
                time.store(START + ttl + delta, Ordering::SeqCst);
                let (events, rx) = mpsc::channel();
                let calls = Arc::new(Mutex::new(Vec::new()));
                let (tx, commands) = mpsc::channel();
                let engine = Engine::new(
                    store,
                    Fake {
                        events,
                        calls: calls.clone(),
                    },
                    Arc::new(AtomicU64::new(1)),
                    PathBuf::from("/nonexistent/legacy"),
                );
                let worker = std::thread::spawn(move || engine.run(commands));
                tx.send(Command::Retention(mode.into())).unwrap();
                tx.send(Command::Session {
                    account: "me".into(),
                    generation: 1,
                })
                .unwrap();
                tx.send(if method == "fetch_messages" {
                    Command::Fetch {
                        id: 42,
                        chat: "chat".into(),
                        before: None,
                        generation: 1,
                    }
                } else {
                    Command::Operation {
                        id: 42,
                        method: method.into(),
                        params: json!({"chatMid":"chat","stickerId":"10","packageId":"20"}),
                        generation: 1,
                    }
                })
                .unwrap();
                loop {
                    let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    if event["id"] == 42 {
                        assert_eq!(event["ok"], true);
                        break;
                    }
                }
                tx.send(Command::Stop).unwrap();
                worker.join().unwrap();
                let expected = if delta < 0 {
                    if method == "send_sticker" {
                        vec!["send_sticker"]
                    } else {
                        vec![]
                    }
                } else {
                    match method {
                        "fetch_messages" => vec!["bridge_fetch_page"],
                        "send_sticker" => vec!["bridge_owned_stickers", "send_sticker"],
                        _ => vec![method],
                    }
                };
                assert_eq!(*calls.lock().unwrap(), expected, "{mode}/{method}");
            }
        }
    }
}

#[test]
fn retention_change_rechecks_existing_data_without_rewriting_timestamps() {
    let (mut store, time) = clock_store();
    store.save_page("chat", None, &page()).unwrap();
    let before = Cursor {
        message_id: "2".into(),
        delivered_time: "2".into(),
    };
    store.save_page("chat", Some(&before), &page()).unwrap();
    time.store(START + 2 * DAY, Ordering::SeqCst);
    store.set_retention("week");
    assert!(store.page("chat", Some(&before), 0).unwrap().is_some());
    store.set_retention("day");
    assert!(store.page("chat", Some(&before), 0).unwrap().is_none());
    store.set_retention("month");
    assert!(store.page("chat", Some(&before), 0).unwrap().is_some());
    time.store(START + 16 * MINUTE, Ordering::SeqCst);
    let (events, rx) = mpsc::channel();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (tx, commands) = mpsc::channel();
    let engine = Engine::new(
        store,
        Fake {
            events,
            calls: calls.clone(),
        },
        Arc::new(AtomicU64::new(1)),
        PathBuf::from("/nonexistent/legacy"),
    );
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "me".into(),
        generation: 1,
    })
    .unwrap();
    for (id, mode, cached) in [(1, "week", true), (2, "day", false)] {
        tx.send(Command::Retention(mode.into())).unwrap();
        tx.send(Command::Fetch {
            id,
            chat: "chat".into(),
            before: None,
            generation: 1,
        })
        .unwrap();
        let response = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(response["ok"], true);
        if cached {
            assert_eq!(response["result"]["cached"], true);
            assert!(calls.lock().unwrap().is_empty());
        }
    }
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    assert_eq!(*calls.lock().unwrap(), vec!["bridge_fetch_page"]);
}

#[test]
fn old_catalog_schema_migrates_without_losing_rows_or_breaking_writes() {
    let path =
        std::env::temp_dir().join(format!("line-policy-schema-{}.sqlite", std::process::id()));
    let old = rusqlite::Connection::open(&path).unwrap();
    old.execute_batch("CREATE TABLE catalogs(account TEXT NOT NULL,kind TEXT NOT NULL,updated INTEGER NOT NULL,body TEXT NOT NULL,PRIMARY KEY(account,kind));").unwrap();
    old.execute(
        "INSERT INTO catalogs VALUES('me','list_chats',?1,'{\"chats\":[]}')",
        [START],
    )
    .unwrap();
    drop(old);
    for attempt in 0..2 {
        let mut store = Store::open_with_clock(&path, Arc::new(|| START + 1)).unwrap();
        store.set_account("me");
        // The alias upgrade invalidates old public-name snapshots once, but
        // keeps them available offline and doesn't invalidate the next refresh.
        assert_eq!(
            store.catalog("list_chats", DAY).unwrap().is_some(),
            attempt > 0
        );
        assert!(store.stale_catalog("list_chats", DAY).unwrap().is_some());
        store.invalidate_catalogs().unwrap();
        assert!(store.catalog("list_chats", DAY).unwrap().is_none());
        assert!(store.stale_catalog("list_chats", DAY).unwrap().is_some());
        store
            .save_catalog("list_chats", &json!({"chats":[]}))
            .unwrap();
    }
    std::fs::remove_file(path).unwrap();
}
