use anyhow::Result;
use line_rust_core::{
    message::from_envelope,
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex, atomic::AtomicU64, mpsc},
    time::Duration,
};

#[test]
fn rust_fallback_retains_identity_and_safe_plaintext_without_exposing_ciphertext() {
    let raw = json!({"id":"9007199254740993","createdTime":1,"deliveredTime":"2","contentType":"IMAGE","text":"ciphertext","chunks":[{"type":"Buffer","data":[1,2]}],"contentMetadata":{"FILE_NAME":"photo.png","e2eeVersion":"2"}});
    let row = from_envelope(
        &json!({"rawMessage":raw,"decodedMessage":null,"decodeError":"message_decode_failed"}),
        "me",
    );
    assert_eq!(row["id"], "9007199254740993");
    assert_eq!(row["text"], "[Unable to decrypt message]");
    assert_eq!(row["fileName"], "photo.png");
    assert_eq!(row["rawMessage"], raw);
    assert_eq!(row["needsMedia"], false);
    assert!(row["flex"].is_null());
    let plain = from_envelope(
        &json!({"rawMessage":{"id":"1","text":"usable raw text"},"decodeError":"message_decode_failed"}),
        "me",
    );
    assert_eq!(plain["text"], "usable raw text");
    let recovered = from_envelope(
        &json!({"rawMessage":raw,"decodedMessage":{"id":"9007199254740993","text":"recovered"},"decodeError":null}),
        "me",
    );
    assert_eq!(recovered["text"], "recovered");
    assert!(recovered["decodeError"].is_null());
}

struct Mixed {
    events: mpsc::Sender<Value>,
    calls: Arc<Mutex<Vec<Value>>>,
}
impl Transport for Mixed {
    fn call(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        assert_eq!(method, "bridge_fetch_page");
        self.calls.lock().unwrap().push(p.clone());
        let raw = json!({"id":"9007199254740993","from":"peer","to":"me","createdTime":1,"deliveredTime":"1","contentType":"IMAGE","chunks":["encrypted"]});
        Ok(if p["before"].is_null() {
            json!({"messages":[
            {"rawMessage":{"id":"9007199254740994"},"decodedMessage":{"id":"9007199254740994","text":"readable","createdTime":2}},
            {"rawMessage":raw,"decodedMessage":null,"decodeError":"message_decode_failed"}
        ],"nextCursor":{"messageId":"9007199254740993","deliveredTime":"1"},"hasMore":true})
        } else {
            assert_eq!(p["before"]["messageId"], "9007199254740993");
            json!({"messages":[{"rawMessage":{"id":"9007199254740992"},"decodedMessage":{"id":"9007199254740992","text":"older","createdTime":0}}],"nextCursor":{"messageId":"9007199254740992","deliveredTime":"0"},"hasMore":false})
        })
    }
    fn materialize(&self, _: &Value, _: u64) -> Result<Value> {
        panic!("undecryptable media must not be auto-downloaded")
    }
    fn emit(&self, e: Value) {
        self.events.send(e).unwrap();
    }
}

#[test]
fn mixed_page_is_displayed_persisted_and_can_continue_after_failed_oldest_message() {
    let root = std::env::temp_dir().join(format!("line-decode-page-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("cache.sqlite");
    let store = Store::open(&db).unwrap();
    let (events, rx) = mpsc::channel();
    let (tx, commands) = mpsc::channel();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::new(
        store,
        Mixed {
            events,
            calls: calls.clone(),
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
    tx.send(Command::Fetch {
        id: 1,
        chat: "peer".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first["ok"], true);
    let rows = first["result"]["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["text"], "[Unable to decrypt message]");
    assert_eq!(rows[1]["text"], "readable");
    assert!(rows[0].get("rawMessage").is_none());
    tx.send(Command::Fetch {
        id: 2,
        chat: "peer".into(),
        before: Some(serde_json::from_value(first["result"]["nextCursor"].clone()).unwrap()),
        generation: 1,
    })
    .unwrap();
    let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(second["ok"], true);
    assert_eq!(second["result"]["messages"][0]["text"], "older");
    assert_eq!(second["result"]["hasMore"], false);
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    assert_eq!(calls.lock().unwrap().len(), 2);
    let mut store = Store::open(&db).unwrap();
    store.set_account("me");
    let failed = store.message("peer", "9007199254740993").unwrap().unwrap();
    assert_eq!(failed["decodeError"], "message_decode_failed");
    assert_eq!(failed["rawMessage"]["chunks"], json!(["encrypted"]));
    for id in ["9007199254740992", "9007199254740994"] {
        assert!(store.message("peer", id).unwrap().is_some());
    }
    let recovery = from_envelope(
        &json!({"rawMessage":failed["rawMessage"],"decodedMessage":{"id":"9007199254740993","text":"recovered","createdTime":1},"decodeError":null}),
        "me",
    );
    store.save_messages("peer", &[recovery]).unwrap();
    assert!(store.message("peer", "9007199254740993").unwrap().unwrap()["decodeError"].is_null());
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
