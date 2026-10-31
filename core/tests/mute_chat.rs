use anyhow::{Result, bail};
use line_gtk_core::{
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
    calls: Arc<Mutex<Vec<String>>>,
    fail: bool,
}
impl Transport for Fake {
    fn call(&self, _: &str, _: Value, _: u64) -> Result<Value> {
        unreachable!()
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        Ok(row.clone())
    }
    fn application(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        self.calls.lock().unwrap().push(method.into());
        match method {
            "mute_chat" => {
                if self.fail {
                    bail!("LINE rejected mute");
                }
                Ok(json!({"mid":p["mid"],"muted":p["muted"]}))
            }
            "list_chats" | "list_friends" => {
                line_gtk_core::catalog::load(method, &p["previous"], |_, _| bail!("offline"))
            }
            _ => panic!("unexpected operation"),
        }
    }
}

#[test]
fn mute_and_unmute_commit_before_ui_event_survive_offline_refresh_and_restart() {
    for (muted, fail) in [(true, false), (false, false), (true, true), (false, true)] {
        let root =
            std::env::temp_dir().join(format!("line-mute-{}-{muted}-{fail}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("cache.sqlite3");
        let mut store = Store::open(&db).unwrap();
        for account in ["me", "other"] {
            store.set_account(account);
            for (kind, key) in [("list_chats", "chats"), ("list_friends", "friends")] {
                store.save_catalog(kind,&json!({key:[{"mid":"u1","name":"Friend","muted":!muted,"unread":3,"avatarPath":"/cached"},{"mid":"u2","muted":true}]})).unwrap();
            }
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
                fail,
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
            id: 1,
            method: "mute_chat".into(),
            params: json!({"mid":"u1","muted":muted}),
            generation: 1,
        })
        .unwrap();
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let response = if fail {
            assert!(first["event"].is_null());
            first
        } else {
            assert_eq!(first, json!({"event":"chat_mute","mid":"u1","muted":muted}));
            let mut reader = Store::open(&db).unwrap();
            reader.set_account("me");
            assert_eq!(
                reader.catalog("list_chats", i64::MAX).unwrap().unwrap()["chats"][0]["muted"],
                muted
            );
            rx.recv_timeout(Duration::from_secs(5)).unwrap()
        };
        assert_eq!(response["ok"], !fail);
        assert_eq!(response["id"], 1);
        let expected = if fail { !muted } else { muted };
        for (id, method, key) in [(2, "list_chats", "chats"), (3, "list_friends", "friends")] {
            tx.send(Command::Operation {
                id,
                method: method.into(),
                params: json!({"force":true}),
                generation: 1,
            })
            .unwrap();
            let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(result["id"], id);
            assert_eq!(result["ok"], true);
            assert_eq!(
                result["result"][key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["mid"] == "u1")
                    .unwrap()["muted"],
                expected
            );
        }
        tx.send(Command::Stop).unwrap();
        worker.join().unwrap();
        assert!(rx.try_recv().is_err());
        let mut reopened = Store::open(&db).unwrap();
        for account in ["me", "other"] {
            reopened.set_account(account);
            for (kind, key) in [("list_chats", "chats"), ("list_friends", "friends")] {
                let list = reopened.stale_catalog(kind, i64::MAX).unwrap().unwrap();
                let rows = list[key].as_array().unwrap();
                let row = rows.iter().find(|row| row["mid"] == "u1").unwrap();
                assert_eq!(
                    row["muted"],
                    if account == "me" { expected } else { !muted }
                );
                assert_eq!(row["name"], "Friend");
                assert_eq!(row["unread"], 3);
                assert_eq!(
                    rows.iter().find(|row| row["mid"] == "u2").unwrap()["muted"],
                    true
                );
            }
        }
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn missing_catalogs_gain_partial_mute_rows_and_existing_expiry_is_preserved() {
    let time = Arc::new(AtomicI64::new(1000));
    let clock = time.clone();
    let mut store = Store::open_with_clock(
        std::path::Path::new(":memory:"),
        Arc::new(move || clock.load(Ordering::SeqCst)),
    )
    .unwrap();
    store.set_account("me");
    store
        .save_catalog("list_chats", &json!({"chats":[{"mid":"u1","muted":false}]}))
        .unwrap();
    time.store(1099, Ordering::SeqCst);
    store.set_chat_muted("u1", true).unwrap();
    assert!(store.catalog("list_chats", 100).unwrap().is_some());
    time.store(1100, Ordering::SeqCst);
    assert!(store.catalog("list_chats", 100).unwrap().is_none());
    assert_eq!(
        store.stale_catalog("list_friends", 1000).unwrap().unwrap()["friends"][0]["muted"],
        true
    );
    assert!(store.catalog("list_friends", 1000).unwrap().is_none());
    store.invalidate_catalogs().unwrap();
    store.set_chat_muted("u1", false).unwrap();
    assert!(store.catalog("list_chats", 1000).unwrap().is_none());
}
