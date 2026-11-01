use anyhow::Result;
use line_rust_core::{
    store::{Cursor, Page, Store},
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, atomic::AtomicU64, mpsc};
use std::time::Duration;

struct Fake {
    visible: Arc<Mutex<Vec<String>>>,
    events: mpsc::Sender<Value>,
    downloads: mpsc::Sender<String>,
}
impl Transport for Fake {
    fn call(&self, method: &str, _: Value, _: u64) -> Result<Value> {
        panic!("unexpected network/history request: {method}");
    }
    fn memory_media(&self, account: &str, chat: &str) -> bool {
        account == "me" && chat == "private"
    }
    fn media_visible(&self, _: &str, id: &str) -> bool {
        self.visible.lock().unwrap().iter().any(|v| v == id)
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        self.downloads.send(row["id"].as_str().unwrap().into())?;
        let mut row = row.clone();
        row["filePath"] = json!("/proc/123/fd/9");
        row["imagePath"] = json!("/proc/123/fd/10");
        Ok(row)
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
}

#[test]
fn private_media_requires_visible_request_and_never_persists_ram_paths() {
    let path =
        std::env::temp_dir().join(format!("line-memory-policy-{}.sqlite", std::process::id()));
    let mut store = Store::open(&path).unwrap();
    store.set_account("me");
    store
        .save_messages(
            "private",
            &[json!({"id":"1","createdTime":1,"contentType":"IMAGE"})],
        )
        .unwrap();
    // This disconnected latest page creates a persisted gap. Private chats
    // must not repair it or hydrate its media in the background.
    store.save_page("private", None, &Page {
        messages: vec![json!({"id":"3","createdTime":3,"contentType":"IMAGE","filePath":"/old/disk.jpg"})],
        next_cursor: Some(Cursor { message_id:"3".into(), delivered_time:"3".into() }),
        has_more: true, cached: false,
    }).unwrap();
    assert!(store.gap("private").unwrap().is_some());
    let visible = Arc::new(Mutex::new(vec![]));
    let (events, rx) = mpsc::channel();
    let (downloads, downloaded) = mpsc::channel();
    let engine = Engine::new(
        store,
        Fake {
            visible: visible.clone(),
            events,
            downloads,
        },
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
    tx.send(Command::Fetch {
        id: 10,
        chat: "private".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Live {
        chat: "private".into(),
        message: json!({"id":"2","contentType":"VIDEO","createdTime":2}),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Download {
        id: 1,
        chat: "private".into(),
        message: "1".into(),
        generation: 1,
    })
    .unwrap();
    loop {
        let event = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        if event["id"] == 10 {
            assert_eq!(event["ok"], true);
            assert!(event["result"]["messages"][0]["filePath"].is_null());
        }
        if event["id"] == 1 {
            assert_eq!(event["ok"], false);
            break;
        }
    }
    assert!(downloaded.try_recv().is_err());
    visible.lock().unwrap().push("1".into());
    tx.send(Command::VisibleMedia {
        chat: "private".into(),
        ids: vec!["1".into(), "2".into()],
        generation: 1,
    })
    .unwrap();
    assert_eq!(
        downloaded.recv_timeout(Duration::from_secs(3)).unwrap(),
        "1"
    );
    loop {
        let event = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        if event["event"] == "media_ready" {
            break;
        }
    }
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    assert!(downloaded.try_recv().is_err());
    let mut store = Store::open(&path).unwrap();
    store.set_account("me");
    let row = store.message("private", "1").unwrap().unwrap();
    assert!(row["filePath"].is_null());
    assert!(row["imagePath"].is_null());
    drop(store);
    std::fs::remove_file(path).unwrap();
}
