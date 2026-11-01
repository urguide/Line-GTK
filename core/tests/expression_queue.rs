use anyhow::{Result, bail};
use line_rust_core::{
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

const JOBS: &str = "expression_download_jobs";
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "expression-jobs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fake {
    events: mpsc::Sender<Value>,
    commands: mpsc::Sender<Command>,
    calls: Arc<Mutex<Vec<String>>>,
    interrupt: bool,
    stop_queue: bool,
}
impl Transport for Fake {
    fn call(&self, _: &str, _: Value, _: u64) -> Result<Value> {
        panic!("local library/download queue must not query shop");
    }
    fn materialize(&self, _: &Value, _: u64) -> Result<Value> {
        unreachable!()
    }
    fn application(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        match method {
            "expression_library" => {
                Ok(json!({"packs":p["installed"]["packs"],"products":p["products"]}))
            }
            "expression_prepare" => {
                let package = p["product"]["id"].as_str().unwrap();
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("prepare:{package}"));
                Ok(
                    json!({"id":package,"name":package,"stickers":[{"stickerId":"001","packageId":package},{"stickerId":"002","packageId":package}]}),
                )
            }
            "expression_item" => {
                let package = p["packageId"].as_str().unwrap();
                let id = p["stickerId"].as_str().unwrap();
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("item:{package}:{id}"));
                if package == "broken" {
                    bail!("CDN unavailable");
                }
                Ok(json!({"imagePath":format!("/cache/{package}/{id}.png")}))
            }
            _ => panic!("unexpected {method}"),
        }
    }
    fn emit(&self, event: Value) {
        if event["event"] == "expression_state"
            && event["jobs"][0]["status"] == "downloading"
            && event["jobs"][0]["done"] == 1
        {
            // The UI never submits the next item: the engine does. Inject a
            // foreground operation to prove it runs before the next item.
            self.commands
                .send(operation(90, "expression_local", json!({"kind":"emoji"})))
                .unwrap();
            if self.interrupt {
                self.commands.send(Command::Stop).unwrap();
            }
            if self.stop_queue {
                self.commands
                    .send(operation(91, "stop_expressions", json!({"kind":"emoji"})))
                    .unwrap();
            }
        }
        self.events.send(event).unwrap();
    }
}

fn operation(id: u64, method: &str, params: Value) -> Command {
    Command::Operation {
        id,
        method: method.into(),
        params,
        generation: 1,
    }
}
fn seed(path: &Path) {
    let mut store = Store::open(path).unwrap();
    store.set_account("me");
    store
        .save_catalog(
            "expression_owned_emoji",
            &json!({"products":[{"id":"abc"},{"id":"broken"},{"id":"last"}]}),
        )
        .unwrap();
}
fn response(rx: &mpsc::Receiver<Value>, id: u64) -> Value {
    loop {
        let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if event["id"] == id {
            return event;
        }
    }
}

#[test]
fn durable_queue_resumes_after_restart_without_ui_and_yields_between_images() {
    let root = Temp::new();
    let db = root.0.join("db.sqlite");
    seed(&db);
    let calls = Arc::new(Mutex::new(Vec::new()));
    for interrupted in [true, false] {
        let (tx, commands) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        // Queue foreground commands before starting the worker, eliminating timing assumptions.
        tx.send(Command::Session {
            account: "me".into(),
            generation: 1,
        })
        .unwrap();
        tx.send(operation(1, "expression_local", json!({"kind":"emoji"})))
            .unwrap();
        if interrupted {
            tx.send(operation(
                2,
                "enqueue_expressions",
                json!({"kind":"emoji","packages":["abc","abc","broken","last"]}),
            ))
            .unwrap();
        }
        let engine = Engine::new(
            Store::open(&db).unwrap(),
            Fake {
                events,
                commands: tx.clone(),
                calls: calls.clone(),
                interrupt: interrupted,
                stop_queue: false,
            },
            Arc::new(AtomicU64::new(1)),
            root.0.join("legacy"),
        );
        let worker = std::thread::spawn(move || engine.run(commands));
        assert_eq!(response(&rx, 1)["result"]["packs"], json!([]));
        if interrupted {
            let accepted = response(&rx, 2);
            assert_eq!(accepted["result"]["jobs"].as_array().unwrap().len(), 3);
            assert_eq!(accepted["result"]["jobs"][0]["status"], "queued");
            assert_eq!(response(&rx, 90)["result"]["packs"], json!([]));
            worker.join().unwrap();
            let mut store = Store::open(&db).unwrap();
            store.set_account("me");
            let jobs = store.catalog(JOBS, i64::MAX).unwrap().unwrap();
            assert_eq!(jobs[0]["next"], 1);
            assert_eq!(jobs[0]["status"], "downloading");
            assert!(
                store
                    .catalog("expression_installed_emoji", i64::MAX)
                    .unwrap()
                    .is_none()
            );
        } else {
            loop {
                let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                if event["event"] == "expression_state" && event["jobs"][2]["status"] == "done" {
                    assert_eq!(event["jobs"][0]["status"], "done");
                    assert_eq!(event["jobs"][1]["status"], "failed");
                    assert_eq!(event["catalog"]["packs"].as_array().unwrap().len(), 2);
                    break;
                }
            }
            tx.send(Command::Stop).unwrap();
            worker.join().unwrap();
        }
    }
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "prepare:abc",
            "item:abc:001",
            "item:abc:002",
            "prepare:broken",
            "item:broken:001",
            "prepare:last",
            "item:last:001",
            "item:last:002"
        ]
    );
    let mut store = Store::open(&db).unwrap();
    store.set_account("other");
    assert!(store.catalog(JOBS, i64::MAX).unwrap().is_none());
    assert!(
        store
            .catalog("expression_installed_emoji", i64::MAX)
            .unwrap()
            .is_none()
    );
}

#[test]
fn stopping_finishes_active_pack_and_persists_cancellation_of_remaining_jobs() {
    let root = Temp::new();
    let db = root.0.join("db.sqlite");
    seed(&db);
    let (tx, commands) = mpsc::channel();
    let (events, rx) = mpsc::channel();
    tx.send(Command::Session {
        account: "me".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(operation(
        1,
        "enqueue_expressions",
        json!({"kind":"emoji","packages":["abc","last"]}),
    ))
    .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::new(
        Store::open(&db).unwrap(),
        Fake {
            events,
            commands: tx.clone(),
            calls: calls.clone(),
            interrupt: false,
            stop_queue: true,
        },
        Arc::new(AtomicU64::new(1)),
        root.0.join("legacy"),
    );
    let worker = std::thread::spawn(move || engine.run(commands));
    assert_eq!(response(&rx, 1)["ok"], true);
    let stopped = response(&rx, 91);
    assert_eq!(stopped["result"]["jobs"][1]["status"], "cancelled");
    loop {
        let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if event["event"] == "expression_state" && event["jobs"][0]["status"] == "done" {
            break;
        }
    }
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        ["prepare:abc", "item:abc:001", "item:abc:002"]
    );
    let mut store = Store::open(&db).unwrap();
    store.set_account("me");
    assert_eq!(
        store.catalog(JOBS, i64::MAX).unwrap().unwrap()[1]["status"],
        "cancelled"
    );
}
