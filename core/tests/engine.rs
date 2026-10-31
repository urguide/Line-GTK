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
