use anyhow::{Result, bail};
use line_rust_core::{
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, atomic::AtomicU64, mpsc};
use std::time::Duration;

struct Fake {
    events: mpsc::Sender<Value>,
    calls: Arc<Mutex<Vec<String>>>,
}
impl Transport for Fake {
    fn call(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        assert_eq!(method, "bridge_owned_expressions");
        self.calls
            .lock()
            .unwrap()
            .push(format!("owned:{}", p["offset"]));
        Ok(json!({"productList":[{"id":"abc","latestVersion":"2"},{"id":"broken"}],"totalSize":2}))
    }
    fn application(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        match method {
            "expression_library" => {
                Ok(json!({"packs":p["installed"]["packs"],"products":p["products"]}))
            }
            "download_expression" => {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("download:{}", p["packageId"]));
                if p["packageId"] == "broken" {
                    bail!("HTTP 503");
                }
                assert_eq!(p["product"]["latestVersion"], "2");
                Ok(
                    json!({"id":"abc","stickers":[{"packageId":"abc","stickerId":"001","version":"2","altText":"笑"}]}),
                )
            }
            "send_message" => {
                if p["mentions"].is_array() {
                    assert_eq!(p["text"], "好😀(笑) @Johnny");
                    assert_eq!(p["relatedMessageId"], "122");
                    let mentions: Value =
                        serde_json::from_str(p["contentMetadata"]["MENTION"].as_str().unwrap())
                            .unwrap();
                    assert_eq!(
                        mentions,
                        json!({"MENTIONEES":[{"S":"7","E":"14","M":"u1"}]})
                    );
                } else {
                    assert_eq!(p["text"], "好😀(笑)");
                }
                let r = line_rust_core::expressions::resources(&p["contentMetadata"]);
                assert_eq!(r[0]["S"], 3);
                Ok(
                    json!({"rawMessage":{"id":"123","text":p["text"],"contentMetadata":p["contentMetadata"]}}),
                )
            }
            _ => unreachable!(),
        }
    }
    fn materialize(&self, _: &Value, _: u64) -> Result<Value> {
        unreachable!()
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
}

#[test]
fn library_is_account_scoped_persists_downloads_and_only_commits_success() {
    let (events, rx) = mpsc::channel();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let engine = Engine::new(
        store,
        Fake {
            events,
            calls: calls.clone(),
        },
        Arc::new(AtomicU64::new(1)),
        "/nonexistent/expressions".into(),
    );
    let (tx, commands) = mpsc::channel();
    let worker = std::thread::spawn(move || engine.run(commands));
    let session = |account: &str| {
        tx.send(Command::Session {
            account: account.into(),
            generation: 1,
        })
        .unwrap()
    };
    let request = |id, method: &str, params| {
        tx.send(Command::Operation {
            id,
            method: method.into(),
            params,
            generation: 1,
        })
        .unwrap();
        loop {
            let value = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if value["id"] == id {
                break value;
            }
        }
    };
    session("me");
    let listed = request(1, "expression_library", json!({"kind":"emoji"}));
    assert_eq!(listed["result"]["packs"], json!([]));
    assert_eq!(calls.lock().unwrap().as_slice(), ["owned:0"]);
    assert_eq!(
        request(
            2,
            "download_expression",
            json!({"kind":"emoji","packageId":"abc"})
        )["ok"],
        true
    );
    assert_eq!(
        request(
            3,
            "download_expression",
            json!({"kind":"emoji","packageId":"broken"})
        )["ok"],
        false
    );
    assert_eq!(
        request(
            4,
            "download_expression",
            json!({"kind":"emoji","packageId":"unowned"})
        )["ok"],
        false
    );
    assert_eq!(
        request(5, "expression_library", json!({"kind":"emoji"}))["result"]["packs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let sent = request(
        6,
        "send_message",
        json!({"chatMid":"u1","text":"好😀[emoji:abc:001]"}),
    );
    assert_eq!(sent["ok"], true);
    assert_eq!(sent["result"]["message"]["emojis"][0]["sticonId"], "001");
    let mentioned = request(
        9,
        "send_message",
        json!({
            "chatMid":"c1", "text":"好😀[emoji:abc:001] @Johnny", "relatedMessageId":"122",
            "mentions":[{"start":18,"end":25,"mid":"u1","label":"@Johnny"}]
        }),
    );
    assert_eq!(mentioned["ok"], true);
    session("other");
    assert_eq!(
        request(7, "expression_library", json!({"kind":"emoji"}))["result"]["packs"],
        json!([])
    );
    session("me");
    assert_eq!(
        request(8, "expression_library", json!({"kind":"emoji"}))["result"]["packs"][0]["id"],
        "abc"
    );
    assert!(!calls.lock().unwrap().iter().any(|c| c.contains("unowned")));
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
}
