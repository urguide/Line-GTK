use anyhow::{Result, bail};
use line_rust_core::{
    friends,
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

#[test]
fn normalizes_id_and_preserves_lookup_for_existing_friend_or_failed_request() {
    for input in ["alice", "@alice", "  @alice  "] {
        for rejection in [None, Some("already friends"), Some("request unavailable")] {
            let mut calls = Vec::new();
            let result = friends::add(input, |method, p| {
                calls.push(method.to_owned());
                assert_eq!(p["userid"], "alice");
                if method == "bridge_find_contact" {
                    return Ok(json!({"mid":"u1","displayName":"Alice","pictureStatus":"/avatar"}));
                }
                assert_eq!(p["mid"], "u1");
                if let Some(error) = rejection {
                    bail!("{error}");
                }
                Ok(json!({}))
            })
            .unwrap();
            assert_eq!(calls, vec!["bridge_find_contact", "bridge_friend_request"]);
            assert_eq!(result["mid"], "u1");
            assert_eq!(result["displayName"], "Alice");
            assert_eq!(result["picturePath"], "/avatar");
            assert_eq!(result["requestSent"], rejection.is_none());
        }
    }
}

#[test]
fn missing_user_or_failed_lookup_never_sends_friend_request() {
    for lookup in [
        Value::Null,
        json!({}),
        json!({"mid":""}),
        json!({"mid":" "}),
    ] {
        let mut calls = 0;
        let error = friends::add("@missing", |method, _| {
            calls += 1;
            assert_eq!(method, "bridge_find_contact");
            Ok(lookup.clone())
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "user_not_found");
        assert_eq!(calls, 1);
    }
    assert_eq!(
        friends::add("@alice", |_, _| bail!("lookup failed"))
            .unwrap_err()
            .to_string(),
        "lookup failed"
    );
    for input in ["", " ", "@"] {
        assert!(friends::add(input, |_, _| panic!("empty ID must not call API")).is_err());
    }
    let result = friends::add("@alice", |method, _| {
        Ok(if method == "bridge_find_contact" {
            json!({"mid":"u1"})
        } else {
            json!({})
        })
    })
    .unwrap();
    assert_eq!(result["displayName"], "alice");
}

struct Fake {
    events: mpsc::Sender<Value>,
    found: bool,
}
impl Transport for Fake {
    fn call(&self, _: &str, _: Value, _: u64) -> Result<Value> {
        unreachable!()
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        Ok(row.clone())
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
    fn application(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        match method {
            "add_friend" => friends::add(p["userid"].as_str().unwrap(), |rpc, args| {
                assert_eq!(args["userid"], "alice");
                if rpc == "bridge_friend_request" {
                    bail!("already friends");
                }
                Ok(if self.found {
                    json!({"mid":"u1","displayName":"New name","picturePath":"/new"})
                } else {
                    Value::Null
                })
            }),
            "list_friends" => {
                line_rust_core::catalog::load(method, &p["previous"], |_, _| bail!("offline"))
            }
            _ => panic!("unexpected {method}"),
        }
    }
}

#[test]
fn worker_updates_cache_only_after_lookup_and_returns_ui_result_after_persistence() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for found in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "line-add-friend-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("cache.sqlite");
        let mut store = Store::open(&db).unwrap();
        for account in ["me", "other"] {
            store.set_account(account);
            store.save_catalog("list_chats",&json!({"chats":[{"mid":"u1","name":"Old name","unread":3,"muted":true,"avatarPath":"/cached"}]})).unwrap();
            store
                .save_catalog(
                    "list_friends",
                    &json!({"friends":[{"mid":"u1","name":"Old name"}]}),
                )
                .unwrap();
        }
        store.set_account("me");
        let (events, rx) = mpsc::channel();
        let (tx, commands) = mpsc::channel();
        let engine = Engine::new(
            store,
            Fake { events, found },
            Arc::new(AtomicU64::new(1)),
            PathBuf::from("/nonexistent/legacy"),
        );
        let worker = std::thread::spawn(move || engine.run(commands));
        tx.send(Command::Session {
            account: "me".into(),
            generation: 1,
        })
        .unwrap();
        tx.send(Command::Operation {
            id: 1,
            method: "add_friend".into(),
            params: json!({"userid":"@alice"}),
            generation: 1,
        })
        .unwrap();
        let response = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["ok"], found);
        if found {
            assert_eq!(response["result"]["displayName"], "New name");
            assert_eq!(response["result"]["requestSent"], false);
        } else {
            assert_eq!(response["error"], "user_not_found");
        }
        tx.send(Command::Operation {
            id: 2,
            method: "list_friends".into(),
            params: json!({}),
            generation: 1,
        })
        .unwrap();
        let list = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(list["ok"], true);
        assert_eq!(
            list["result"]["friends"][0]["name"],
            if found { "New name" } else { "Old name" }
        );
        tx.send(Command::Stop).unwrap();
        worker.join().unwrap();
        let mut store = Store::open(&db).unwrap();
        store.set_account("me");
        let row = store
            .stale_catalog("list_chats", i64::MAX)
            .unwrap()
            .unwrap()["chats"][0]
            .clone();
        assert_eq!(row["name"], if found { "New name" } else { "Old name" });
        assert_eq!(row["unread"], 3);
        assert_eq!(row["muted"], true);
        assert_eq!(row["avatarPath"], "/cached");
        store.set_account("other");
        assert_eq!(
            store.catalog("list_friends", i64::MAX).unwrap().unwrap()["friends"][0]["name"],
            "Old name"
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
