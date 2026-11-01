use anyhow::{Result, bail};
use line_rust_core::{
    stickers,
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

#[test]
fn version_lookup_handles_both_line_shapes_and_unknown_package() {
    assert_eq!(
        stickers::package_version(
            &json!({"productList":[{"id":"20","latestVersion":8}]}),
            "20"
        ),
        "8"
    );
    assert_eq!(
        stickers::package_version(&json!({"1":[{"1":"20","21":"9"}]}), "20"),
        "9"
    );
    assert_eq!(
        stickers::package_version(&json!([{"id":"20","version":" 7 "}]), "20"),
        "7"
    );
    assert_eq!(stickers::package_version(&Value::Null, "20"), "1");
    assert_eq!(
        stickers::package_version(&json!([{"id":"21","latestVersion":"9"}]), "20"),
        "1"
    );
}

struct Fake {
    events: mpsc::Sender<Value>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    error: Option<&'static str>,
    lookup_fails: bool,
}
impl Transport for Fake {
    fn call(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        self.calls.lock().unwrap().push((method.into(), p));
        assert_eq!(method, "bridge_owned_stickers");
        if self.lookup_fails {
            bail!("network unavailable");
        }
        Ok(json!({"productList":[{"id":"20","latestVersion":"8"}]}))
    }
    fn application(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        self.calls.lock().unwrap().push((method.into(), p));
        assert_eq!(method, "send_sticker");
        if let Some(error) = self.error {
            bail!("{error}");
        }
        Ok(json!({"rawMessage":{"id":"123","contentType":"STICKER","createdTime":1}}))
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        Ok(row.clone())
    }
}

#[test]
fn send_version_selection_and_rejection_cleanup_are_persisted_without_false_success() {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    for (explicit, cached, error, lookup_fails, expected) in [
        (Some(" 12 "), None, None, false, "12"),
        (None, Some("6"), None, false, "6"),
        (Some(" "), None, None, false, "8"),
        (None, None, None, true, "1"),
        (None, Some("6"), Some("USER_NOT_STICKER_OWNER"), false, "6"),
        (Some("4"), Some("6"), Some("product not owned"), false, "4"),
        (None, Some("6"), Some("network unavailable"), false, "6"),
    ] {
        let root = std::env::temp_dir().join(format!(
            "line-sticker-send-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("messages.sqlite3");
        let mut store = Store::open(&db).unwrap();
        store.set_account("me");
        let recent = json!([{"stickerId":"10","packageId":"20"},{"stickerId":"11","packageId":"20"},{"stickerId":"10","packageId":"30"}]);
        store.save_catalog("recent_stickers", &recent).unwrap();
        store
            .save_catalog("list_stickers", &json!({"packs":[]}))
            .unwrap();
        if let Some(version) = cached {
            store
                .save_catalog(
                    "owned_stickers",
                    &json!({"productList":[{"id":"20","latestVersion":version}]}),
                )
                .unwrap();
        }
        store.set_account("other");
        store.save_catalog("recent_stickers", &recent).unwrap();
        store
            .save_catalog("owned_stickers", &json!({"productList":[]}))
            .unwrap();
        store.set_account("me");
        let (events, rx) = mpsc::channel();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (tx, commands) = mpsc::channel();
        let engine = Engine::new(
            store,
            Fake {
                events,
                calls: calls.clone(),
                error,
                lookup_fails,
            },
            Arc::new(AtomicU64::new(1)),
            root.join("legacy"),
        );
        let worker = std::thread::spawn(move || engine.run(commands));
        tx.send(Command::Session {
            account: "me".into(),
            generation: 1,
        })
        .unwrap();
        tx.send(Command::Operation {
            id: 42,
            method: "send_sticker".into(),
            params: json!({"chatMid":"u1","stickerId":"10","packageId":"20","version":explicit}),
            generation: 1,
        })
        .unwrap();
        let response = loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if event["id"] == 42 {
                break event;
            }
        };
        tx.send(Command::Stop).unwrap();
        worker.join().unwrap();
        assert_eq!(response["ok"], error.is_none());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.last().unwrap().1["version"], expected);
        let lookup = explicit.is_none_or(|s| s.trim().is_empty()) && cached.is_none();
        assert_eq!(
            calls
                .iter()
                .filter(|(m, _)| m == "bridge_owned_stickers")
                .count(),
            usize::from(lookup)
        );
        let mut reopened = Store::open(&db).unwrap();
        reopened.set_account("me");
        assert_eq!(
            reopened.message("u1", "123").unwrap().is_some(),
            error.is_none()
        );
        let saved = reopened
            .catalog("recent_stickers", i64::MAX)
            .unwrap()
            .unwrap();
        if error.is_some_and(stickers::not_owned) {
            assert_eq!(response["error"], "sticker_not_owned");
            assert_eq!(
                saved,
                json!([{"stickerId":"11","packageId":"20"},{"stickerId":"10","packageId":"30"}])
            );
            assert!(
                reopened
                    .catalog("owned_stickers", i64::MAX)
                    .unwrap()
                    .is_none()
            );
            assert!(
                reopened
                    .catalog("list_stickers", i64::MAX)
                    .unwrap()
                    .is_none()
            );
        } else if error.is_some() {
            assert_eq!(saved, recent);
            assert!(
                reopened
                    .catalog("owned_stickers", i64::MAX)
                    .unwrap()
                    .is_some()
            );
        } else {
            assert_eq!(saved[0]["version"], expected);
        }
        reopened.set_account("other");
        assert_eq!(
            reopened
                .catalog("recent_stickers", i64::MAX)
                .unwrap()
                .unwrap(),
            recent
        );
        assert!(
            reopened
                .catalog("owned_stickers", i64::MAX)
                .unwrap()
                .is_some()
        );
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
}
