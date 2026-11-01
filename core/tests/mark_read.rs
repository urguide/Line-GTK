use anyhow::{Result, bail};
use line_rust_core::{
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

struct Fake {
    events: mpsc::Sender<Value>,
    calls: Arc<Mutex<Vec<Value>>>,
    outcome: &'static str,
}
impl Transport for Fake {
    fn call(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        assert_eq!(method, "bridge_mark_read");
        self.calls.lock().unwrap().push(p);
        match self.outcome {
            "error" => bail!("LINE rejected receipt"),
            "negative" => Ok(json!({"marked":false})),
            _ => Ok(json!({"marked":true})),
        }
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        Ok(row.clone())
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
}

#[test]
fn mark_read_soft_failures_never_emit_error_envelopes_or_clear_unread() {
    for (logged_in, p, outcome, expected_calls, marked) in [
        (
            false,
            json!({"chatMid":"u1","lastMessageId":"9007199254740993"}),
            "success",
            0,
            false,
        ),
        (
            true,
            json!({"chatMid":"","lastMessageId":"1"}),
            "success",
            0,
            false,
        ),
        (true, json!({"chatMid":"u1"}), "success", 0, false),
        (
            true,
            json!({"chatMid":"u1","lastMessageId":"not-an-id"}),
            "success",
            0,
            false,
        ),
        (
            true,
            json!({"chatMid":"u1","lastMessageId":123}),
            "success",
            0,
            false,
        ),
        (
            true,
            json!({"chatMid":"u1","lastMessageId":"9007199254740993"}),
            "error",
            1,
            false,
        ),
        (
            true,
            json!({"chatMid":"u1","lastMessageId":"9007199254740993"}),
            "negative",
            1,
            false,
        ),
        (
            true,
            json!({"chatMid":"u1","lastMessageId":"9007199254740993"}),
            "success",
            1,
            true,
        ),
    ] {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "line-read-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("cache.sqlite");
        let mut store = Store::open(&path).unwrap();
        for account in ["me", "other"] {
            store.set_account(account);
            store
                .save_catalog(
                    "list_chats",
                    &json!({"chats":[{"mid":"u1","unread":8},{"mid":"u2","unread":4}]}),
                )
                .unwrap();
        }
        store.set_account("me");
        let (events, rx) = mpsc::channel();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (tx, commands) = mpsc::channel();
        let engine = Engine::new(
            store,
            Fake {
                events,
                calls: calls.clone(),
                outcome,
            },
            Arc::new(AtomicU64::new(1)),
            root.join("legacy"),
        );
        let worker = std::thread::spawn(move || engine.run(commands));
        if logged_in {
            tx.send(Command::Session {
                account: "me".into(),
                generation: 1,
            })
            .unwrap();
        }
        tx.send(Command::Operation {
            id: 1,
            method: "mark_read".into(),
            params: p.clone(),
            generation: 1,
        })
        .unwrap();
        let response = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // UI only toasts an operation's error when the envelope's ok is false.
        assert_eq!(response["id"], 1);
        assert_eq!(response["ok"], true);
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["marked"], marked);
        if logged_in {
            tx.send(Command::Operation {
                id: 2,
                method: "list_chats".into(),
                params: json!({}),
                generation: 1,
            })
            .unwrap();
            let list = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(list["ok"], true);
            assert_eq!(
                list["result"]["chats"][0]["unread"],
                if marked { 0 } else { 8 }
            );
            assert_eq!(list["result"]["chats"][1]["unread"], 4);
        }
        tx.send(Command::Stop).unwrap();
        worker.join().unwrap();
        assert!(rx.try_recv().is_err());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), expected_calls);
        if expected_calls == 1 {
            assert_eq!(calls[0], p);
        }
        let mut reopened = Store::open(&path).unwrap();
        reopened.set_account("me");
        assert_eq!(
            reopened.catalog("list_chats", i64::MAX).unwrap().unwrap()["chats"][0]["unread"],
            if marked { 0 } else { 8 }
        );
        reopened.set_account("other");
        assert_eq!(
            reopened.catalog("list_chats", i64::MAX).unwrap().unwrap()["chats"][0]["unread"],
            8
        );
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn read_cache_update_does_not_refresh_expiry_or_reset_invalidation() {
    let time = Arc::new(AtomicI64::new(1_000_000));
    let clock = time.clone();
    let mut store = Store::open_with_clock(
        std::path::Path::new(":memory:"),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
    .unwrap();
    store.set_account("me");
    store
        .save_catalog("list_chats", &json!({"chats":[{"mid":"u1","unread":5}]}))
        .unwrap();
    time.store(1_000_099, Ordering::SeqCst);
    store.mark_chat_read("u1").unwrap();
    assert_eq!(
        store.catalog("list_chats", 100).unwrap().unwrap()["chats"][0]["unread"],
        0
    );
    time.store(1_000_100, Ordering::SeqCst);
    assert!(store.catalog("list_chats", 100).unwrap().is_none());
    store.invalidate_catalogs().unwrap();
    store.mark_chat_read("u1").unwrap();
    assert!(store.catalog("list_chats", 1000).unwrap().is_none());
    assert_eq!(
        store.stale_catalog("list_chats", 1000).unwrap().unwrap()["chats"][0]["unread"],
        0
    );
    store.mark_chat_read("unknown").unwrap();
}
