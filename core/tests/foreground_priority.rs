use anyhow::{Result, bail};
use line_rust_core::{
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

#[test]
fn incoming_chats_are_fifo_deduplicated_and_share_one_worker_per_chat() {
    struct Ordered {
        events: mpsc::Sender<Value>,
        jobs: mpsc::Sender<(String, std::thread::ThreadId)>,
    }
    impl Transport for Ordered {
        fn call(&self, _: &str, params: Value, _: u64) -> Result<Value> {
            let chat = params["chatMid"].as_str().unwrap();
            self.jobs
                .send((format!("page:{chat}"), std::thread::current().id()))?;
            let id = match chat {
                "b" => "2",
                "c" => "3",
                _ => "4",
            };
            Ok(
                json!({"messages":[{"id":id,"createdTime":1,"contentType":if chat == "foreground" {"NONE"} else {"IMAGE"}}],"hasMore":false,"nextCursor":null}),
            )
        }
        fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
            self.jobs.send((
                format!("media:{}", row["chatMid"].as_str().unwrap()),
                std::thread::current().id(),
            ))?;
            Ok(row.clone())
        }
        fn emit(&self, value: Value) {
            let _ = self.events.send(value);
        }
    }
    let (events, rx) = mpsc::channel();
    let (jobs, work) = mpsc::channel();
    let engine = Engine::new(
        Store::open(Path::new(":memory:")).unwrap(),
        Ordered { events, jobs },
        Arc::new(AtomicU64::new(1)),
        "/nonexistent".into(),
    );
    let (tx, commands) = mpsc::channel();
    tx.send(Command::Session {
        account: "self".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::FocusChat {
        chat: "foreground".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Live {
        chat: "old".into(),
        message: json!({"id":"1","contentType":"IMAGE"}),
        generation: 1,
    })
    .unwrap();
    for (id, chat) in [(1, "b"), (2, "c"), (3, "b"), (4, "b")] {
        tx.send(Command::Prefetch {
            id,
            chat: chat.into(),
            generation: 1,
        })
        .unwrap();
    }
    tx.send(Command::Fetch {
        id: 5,
        chat: "foreground".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    let worker = std::thread::spawn(move || engine.run(commands));
    let mut results = std::collections::HashSet::new();
    while results.len() < 5 {
        let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        if let Some(id) = event["id"].as_u64() {
            assert_eq!(event["ok"], true);
            results.insert(id);
        }
    }
    let jobs: Vec<_> = (0..6)
        .map(|_| work.recv_timeout(Duration::from_secs(2)).unwrap())
        .collect();
    assert_eq!(
        jobs.iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        [
            "page:foreground",
            "page:b",
            "media:b",
            "page:c",
            "media:c",
            "media:old"
        ]
    );
    assert_eq!(jobs[1].1, jobs[2].1, "page and media use the same b worker");
    assert_eq!(jobs[3].1, jobs[4].1, "page and media use the same c worker");
    tx.send(Command::Prefetch {
        id: 6,
        chat: "b".into(),
        generation: 1,
    })
    .unwrap();
    wait(&rx, 6);
    assert!(
        work.recv_timeout(Duration::from_millis(100)).is_err(),
        "cached page and hydrated media are not processed again"
    );
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
}

struct Host {
    events: mpsc::Sender<Value>,
    started: mpsc::Sender<String>,
    release: Arc<(Mutex<bool>, Condvar)>,
}
impl Transport for Host {
    fn call(&self, _: &str, args: Value, _: u64) -> Result<Value> {
        let chat = args["chatMid"].as_str().unwrap();
        self.started.send(format!("page:{chat}"))?;
        Ok(
            json!({"messages":[{"id":if chat == "a" {"1"} else {"2"},"createdTime":1,"contentType":if chat == "a" {"IMAGE"} else {"NONE"}}],"hasMore":false,"nextCursor":null}),
        )
    }
    fn call_cancellable(
        &self,
        method: &str,
        args: Value,
        generation: u64,
        cancel: &AtomicBool,
    ) -> Result<Value> {
        if args["chatMid"] == "slow" {
            self.started.send("slow-started".into())?;
            while !cancel.load(Ordering::SeqCst) {
                if *self.release.0.lock().unwrap() {
                    return self.call(method, args, generation);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            self.started.send("slow-deferred".into())?;
            bail!("chat_deferred");
        }
        self.call(method, args, generation)
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        self.started
            .send(format!("media:{}", row["chatMid"].as_str().unwrap()))?;
        let mut release = self.release.0.lock().unwrap();
        while !*release {
            release = self.release.1.wait(release).unwrap();
        }
        Ok(row.clone())
    }
    fn emit(&self, event: Value) {
        let _ = self.events.send(event);
    }
}

fn wait(rx: &mpsc::Receiver<Value>, id: u64) -> Value {
    loop {
        let value = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("foreground blocked by background work");
        if value["id"] == id {
            return value;
        }
    }
}

#[test]
fn stalled_image_does_not_block_another_chat_or_live_messages() {
    let (events, rx) = mpsc::channel();
    let (started, jobs) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let engine = Engine::new(
        Store::open(Path::new(":memory:")).unwrap(),
        Host {
            events,
            started,
            release: release.clone(),
        },
        Arc::new(AtomicU64::new(1)),
        "/nonexistent".into(),
    );
    let (tx, commands) = mpsc::channel();
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "self".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::FocusChat {
        chat: "a".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Fetch {
        id: 1,
        chat: "a".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    assert_eq!(wait(&rx, 1)["ok"], true);
    assert_eq!(jobs.recv_timeout(Duration::from_secs(2)).unwrap(), "page:a");
    assert_eq!(
        jobs.recv_timeout(Duration::from_secs(2)).unwrap(),
        "media:a"
    );
    tx.send(Command::FocusChat {
        chat: "b".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Fetch {
        id: 2,
        chat: "b".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    assert_eq!(wait(&rx, 2)["ok"], true);
    assert!(
        !*release.0.lock().unwrap(),
        "image remains stalled while b is already readable"
    );
    tx.send(Command::Live {
        chat: "b".into(),
        message: json!({"id":"3","text":"live"}),
        generation: 1,
    })
    .unwrap();
    loop {
        if rx.recv_timeout(Duration::from_secs(2)).unwrap()["event"] == "message" {
            break;
        }
    }
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
}

#[test]
fn switching_defers_inflight_page_and_prioritizes_the_new_chat() {
    let (events, rx) = mpsc::channel();
    let (started, jobs) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let engine = Engine::new(
        Store::open(Path::new(":memory:")).unwrap(),
        Host {
            events,
            started,
            release: release.clone(),
        },
        Arc::new(AtomicU64::new(1)),
        "/nonexistent".into(),
    );
    let (tx, commands) = mpsc::channel();
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "self".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::FocusChat {
        chat: "slow".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Fetch {
        id: 1,
        chat: "slow".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    assert_eq!(
        jobs.recv_timeout(Duration::from_secs(2)).unwrap(),
        "slow-started"
    );
    tx.send(Command::FocusChat {
        chat: "b".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Fetch {
        id: 2,
        chat: "b".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    assert_eq!(wait(&rx, 2)["ok"], true);
    let mut deferred = false;
    while let Ok(job) = jobs.recv_timeout(Duration::from_millis(100)) {
        deferred |= job == "slow-deferred";
    }
    assert!(deferred);
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    assert_eq!(wait(&rx, 1)["ok"], true, "deferred page eventually resumes");
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
}

#[test]
fn late_media_after_account_switch_is_discarded() {
    let (events, rx) = mpsc::channel();
    let (started, jobs) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let generation = Arc::new(AtomicU64::new(1));
    let engine = Engine::new(
        Store::open(Path::new(":memory:")).unwrap(),
        Host {
            events,
            started,
            release: release.clone(),
        },
        generation.clone(),
        "/nonexistent".into(),
    );
    let (tx, commands) = mpsc::channel();
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "old".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::FocusChat {
        chat: "a".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Fetch {
        id: 1,
        chat: "a".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    wait(&rx, 1);
    assert_eq!(jobs.recv_timeout(Duration::from_secs(2)).unwrap(), "page:a");
    assert_eq!(
        jobs.recv_timeout(Duration::from_secs(2)).unwrap(),
        "media:a"
    );
    generation.store(2, Ordering::SeqCst);
    tx.send(Command::Session {
        account: "new".into(),
        generation: 2,
    })
    .unwrap();
    tx.send(Command::FocusChat {
        chat: "b".into(),
        generation: 2,
    })
    .unwrap();
    tx.send(Command::Fetch {
        id: 2,
        chat: "b".into(),
        before: None,
        generation: 2,
    })
    .unwrap();
    wait(&rx, 2);
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "old media must not reach the new session"
    );
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
}
