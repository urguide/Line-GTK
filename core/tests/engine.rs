use anyhow::Result;
use line_gtk_core::{
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, atomic::AtomicU64, mpsc};
use std::time::Duration;

struct Fake {
    calls: Arc<Mutex<Vec<u64>>>,
    events: mpsc::Sender<Value>,
}
impl Transport for Fake {
    fn call(&self, method: &str, args: Value, _: u64) -> Result<Value> {
        assert_eq!(method, "bridge_fetch_page");
        let before = args["before"]["messageId"]
            .as_str()
            .map(|s| s.parse::<u64>().unwrap());
        let start = before.map(|n| n - 1).unwrap_or(150);
        self.calls.lock().unwrap().push(start);
        let messages = (start - 19..=start)
            .map(|i| json!({"id":i.to_string(),"createdTime":i,"contentType":"NONE"}))
            .collect::<Vec<_>>();
        Ok(
            json!({"messages":messages,"nextCursor":{"messageId":(start-19).to_string(),"deliveredTime":(start-19).to_string()},"hasMore":true}),
        )
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        Ok(row.clone())
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
}

#[test]
fn worker_returns_first_page_then_repairs_gap_and_reuses_saved_pages() {
    let mut store = Store::open(Path::new(":memory:")).unwrap();
    store.set_account("account");
    store
        .save_messages("chat", &[json!({"id":"100","createdTime":100})])
        .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (events, rx) = mpsc::channel();
    let engine = Engine::new(
        store,
        Fake {
            calls: calls.clone(),
            events,
        },
        Arc::new(AtomicU64::new(1)),
        PathBuf::from("/nonexistent/legacy"),
    );
    let (tx, commands) = mpsc::channel();
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "account".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Fetch {
        id: 1,
        chat: "chat".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first["result"]["messages"].as_array().unwrap().len(), 20);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while calls.lock().unwrap().len() < 3 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    tx.send(Command::Fetch {
        id: 2,
        chat: "chat".into(),
        before: Some(serde_json::from_value(first["result"]["nextCursor"].clone()).unwrap()),
        generation: 1,
    })
    .unwrap();
    let next = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(next["result"]["cached"], true);
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    assert_eq!(*calls.lock().unwrap(), vec![150, 130, 110]);
}

#[test]
fn forced_catalog_refresh_receives_stale_cache_and_postback_fallback_is_persisted() {
    struct Recovery {
        events: mpsc::Sender<Value>,
    }
    impl Transport for Recovery {
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
                "list_chats" => {
                    assert_eq!(p["previous"]["chats"][0]["name"], "Saved");
                    line_gtk_core::catalog::load(method, &p["previous"], |_, _| {
                        anyhow::bail!("offline")
                    })
                }
                "send_postback" => line_gtk_core::postback::send(&p, |rpc, _| {
                    if rpc == "bridge_postback" {
                        anyhow::bail!("unsupported");
                    }
                    Ok(
                        json!({"rawMessage":{"id":"9007199254740993","text":"click","createdTime":10,"contentType":"NONE"}}),
                    )
                }),
                _ => panic!("unexpected {method}"),
            }
        }
    }
    let path =
        std::env::temp_dir().join(format!("line-engine-catalog-{}.sqlite", std::process::id()));
    let mut store = Store::open(&path).unwrap();
    store.set_account("account");
    store
        .save_catalog(
            "list_chats",
            &json!({"chats":[{"mid":"u1","name":"Saved"}]}),
        )
        .unwrap();
    store.invalidate_catalogs().unwrap();
    let (events, rx) = mpsc::channel();
    let engine = Engine::new(
        store,
        Recovery { events },
        Arc::new(AtomicU64::new(1)),
        PathBuf::from("/nonexistent/legacy"),
    );
    let (tx, commands) = mpsc::channel();
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "account".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Operation {
        id: 1,
        method: "list_chats".into(),
        params: json!({"force":true}),
        generation: 1,
    })
    .unwrap();
    let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["result"]["chats"][0]["name"], "Saved");
    tx.send(Command::Operation {
        id: 2,
        method: "send_postback".into(),
        params: json!({"chatMid":"u1","data":"click"}),
        generation: 1,
    })
    .unwrap();
    let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(result["result"]["fallback"], "message");
    assert_eq!(result["result"]["sent"], true);
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    let mut store = Store::open(&path).unwrap();
    store.set_account("account");
    assert_eq!(
        store.message("u1", "9007199254740993").unwrap().unwrap()["text"],
        "click"
    );
    drop(store);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn sticker_refresh_keeps_cached_pack_and_partial_result_retries_without_force() {
    struct Stickers {
        events: mpsc::Sender<Value>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Transport for Stickers {
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
            assert_eq!(method, "list_stickers");
            assert_eq!(
                p["previous"]["packs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["id"] == "1")
                    .unwrap()["name"],
                "Saved"
            );
            let attempt = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            line_gtk_core::stickers::load(
                Ok(json!({"productList":[{"id":"1"},{"id":"2"}]})),
                &p["previous"],
                &p["recent"],
                |id| {
                    if attempt == 0 && id == "1" {
                        anyhow::bail!("metadata download failed");
                    }
                    Ok(json!({"title":"Recovered","version":1,"stickers":[{"id":format!("{id}0")}]}).to_string().into_bytes())
                },
                |id| Ok(format!("/cache/{id}.png")),
            )
        }
    }
    let mut store = Store::open(Path::new(":memory:")).unwrap();
    store.set_account("account");
    store.save_catalog("list_stickers",&json!({"packs":[{"id":"1","name":"Saved","version":"1","stickers":[{"packageId":"1","stickerId":"10"}]}]})).unwrap();
    store
        .save_catalog(
            "recent_stickers",
            &json!([{"packageId":"1","stickerId":"10"}]),
        )
        .unwrap();
    store.invalidate_catalogs().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (events, rx) = mpsc::channel();
    let (tx, commands) = mpsc::channel();
    let engine = Engine::new(
        store,
        Stickers {
            events,
            calls: calls.clone(),
        },
        Arc::new(AtomicU64::new(1)),
        PathBuf::from("/nonexistent/legacy"),
    );
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "account".into(),
        generation: 1,
    })
    .unwrap();
    for id in 1..=3 {
        tx.send(Command::Operation {
            id,
            method: "list_stickers".into(),
            params: json!({}),
            generation: 1,
        })
        .unwrap();
        let response = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["packs"].as_array().unwrap().len(), 3);
        assert_eq!(response["result"]["partial"], id == 1);
    }
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}
