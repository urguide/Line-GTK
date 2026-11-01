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

struct NotesTransport {
    events: mpsc::Sender<Value>,
}
impl Transport for NotesTransport {
    fn call(&self, _: &str, _: Value, _: u64) -> Result<Value> {
        unreachable!()
    }
    fn materialize(&self, _: &Value, _: u64) -> Result<Value> {
        unreachable!()
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
    fn application(&self, method: &str, params: Value, _: u64) -> Result<Value> {
        assert_eq!(method, "notes");
        assert_eq!(params["account"], "me");
        assert_eq!(params["chatMid"], "cgroup");
        if params["action"] == "create" {
            anyhow::bail!("notes_api_403");
        }
        Ok(json!({"posts":[],"cursor":null}))
    }
}

#[test]
fn notes_use_authenticated_worker_and_surface_failures_without_success_or_retry() {
    let store = Store::open(Path::new(":memory:")).unwrap();
    let (events, rx) = mpsc::channel();
    let engine = Engine::new(
        store,
        NotesTransport { events },
        Arc::new(AtomicU64::new(1)),
        "/nonexistent/notes-legacy".into(),
    );
    let (tx, commands) = mpsc::channel();
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "me".into(),
        generation: 1,
    })
    .unwrap();
    for (id, action) in [(1, "list"), (2, "create")] {
        tx.send(Command::Operation {
            id,
            method: "notes".into(),
            params: json!({"chatMid":"cgroup","action":action,"text":"test"}),
            generation: 1,
        })
        .unwrap();
        let response = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(response["id"], id);
        assert_eq!(response["ok"], action == "list");
        if action == "create" {
            assert_eq!(response["error"], "notes_api_403");
        }
    }
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    assert!(rx.try_recv().is_err());
}
