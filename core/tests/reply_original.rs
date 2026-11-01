use anyhow::Result;
use line_rust_core::{
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicU64, mpsc},
    time::Duration,
};

struct CacheOnly(mpsc::Sender<Value>);
impl Transport for CacheOnly {
    fn call(&self, method: &str, _: Value, _: u64) -> Result<Value> {
        panic!("reply must not request network data: {method}");
    }
    fn materialize(&self, _: &Value, _: u64) -> Result<Value> {
        panic!("reply must not create or download another media file");
    }
    fn memory_media(&self, _: &str, chat: &str) -> bool {
        chat == "private"
    }
    fn emit(&self, event: Value) {
        self.0.send(event).unwrap();
    }
}

#[test]
fn quotes_reuse_original_paths_and_respect_chat_account_and_withdrawal() {
    let mut store = Store::open(Path::new(":memory:")).unwrap();
    store.set_account("me");
    let image = json!({"id":"1","contentType":"IMAGE","imagePath":"/cache/1.jpg","filePath":"/cache/1.full.jpg"});
    let video = json!({"id":"2","contentType":"VIDEO","imagePath":"/cache/2.thumb.jpg","filePath":"/cache/2.mp4"});
    store
        .save_messages("chat", &[image.clone(), video.clone()])
        .unwrap();
    store
        .save_messages("private", std::slice::from_ref(&image))
        .unwrap();
    let (events, rx) = mpsc::channel();
    let engine = Engine::new(
        store,
        CacheOnly(events),
        Arc::new(AtomicU64::new(1)),
        "/nonexistent/legacy".into(),
    );
    let (tx, commands) = mpsc::channel();
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "me".into(),
        generation: 1,
    })
    .unwrap();
    let request = |id, chat: &str, message: &str| {
        tx.send(Command::Operation {
            id,
            method: "reply_original".into(),
            params: json!({"chatMid":chat,"messageId":message}),
            generation: 1,
        })
        .unwrap();
        loop {
            let event = rx.recv_timeout(Duration::from_secs(3)).unwrap();
            if event["id"] == id {
                assert_eq!(event["ok"], true);
                break event["result"]["message"].clone();
            }
        }
    };
    for _ in 0..2 {
        let result = request(1, "chat", "1");
        assert_eq!(result["imagePath"], image["imagePath"]);
        assert_eq!(result["filePath"], image["filePath"]);
        let result = request(2, "chat", "2");
        assert_eq!(result["imagePath"], video["imagePath"]);
        assert_eq!(result["filePath"], video["filePath"]);
    }
    assert!(request(3, "other-chat", "1").is_null());
    assert!(request(4, "chat", "missing").is_null());
    let private = request(5, "private", "1");
    assert!(private["imagePath"].is_null());
    assert!(private["filePath"].is_null());
    tx.send(Command::Unsend {
        chat: "chat".into(),
        message: "1".into(),
        generation: 1,
    })
    .unwrap();
    assert!(request(6, "chat", "1").is_null());
    tx.send(Command::Session {
        account: "other-account".into(),
        generation: 1,
    })
    .unwrap();
    assert!(request(7, "chat", "2").is_null());
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
}
