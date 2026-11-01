use anyhow::{Result, bail};
use line_rust_core::{
    receipts::Receipts,
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

#[test]
fn ranges_deduplicate_readers_preserve_gaps_and_do_not_add_snapshot_counts() {
    let mut receipts = Receipts::default();
    receipts.observe(&[
        json!({"id":"10","mine":true,"readCount":5}),
        json!({"id":"20","mine":true}),
        json!({"id":"30","mine":true}),
        json!({"id":"40","mine":true}),
        json!({"id":"50","mine":false,"readCount":99}),
    ]);
    let ranges = json!({
        "me":[{"startMessageId":"0","endMessageId":"100"}],
        "a":[{"startMessageId":"10","endMessageId":"20"},{"startMessageId":"40","endMessageId":"40"}],
        "b":[{"startMessageId":"20","endMessageId":"40"}]
    });
    assert!(receipts.merge_ranges(&ranges, "me"));
    assert!(!receipts.merge_ranges(&ranges, "me"));
    assert_eq!(
        receipts.counts,
        [
            ("10".into(), 5),
            ("20".into(), 2),
            ("30".into(), 1),
            ("40".into(), 2)
        ]
        .into()
    );
    assert!(!receipts.peer_read("a", "40", "me", false));
    assert!(!receipts.peer_read("a", "20", "me", false));
    assert!(!receipts.peer_read("me", "100", "me", false));
    assert!(receipts.peer_read("a", "9007199254740993", "me", false));
    assert!(!receipts.peer_read("a", "bad", "me", false));
    receipts.observe(&[json!({"id":"9007199254740993","mine":true})]);
    assert_eq!(receipts.counts["9007199254740993"], 1);
    assert_eq!(receipts.counts["30"], 1); // gap does not disappear
    assert!(receipts.peer_read("new", "40", "me", false));
    assert_eq!(receipts.counts["20"], 2); // newly joined member: no guessed old reads
    // An older API response must not bridge an unknown reader's history gap.
    receipts.merge_ranges(
        &json!({"new":[{"startMessageId":"1","endMessageId":"10"}]}),
        "me",
    );
    assert_eq!(receipts.counts["20"], 2);
}

struct Fake {
    events: mpsc::Sender<Value>,
}
impl Transport for Fake {
    fn call(&self, method: &str, _: Value, _: u64) -> Result<Value> {
        if method == "bridge_read_ranges" {
            return Ok(json!([{ "chatId":"c1", "ranges": {
                "u1":[{"startMessageId":"1","endMessageId":"20"}],
                "u2":[{"startMessageId":"10","endMessageId":"20"}]
            }}]));
        }
        bail!("offline")
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        Ok(row.clone())
    }
    fn emit(&self, value: Value) {
        self.events.send(value).unwrap();
    }
}

fn wait(rx: &mpsc::Receiver<Value>, matches: impl Fn(&Value) -> bool) -> Value {
    loop {
        let value = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if matches(&value) {
            return value;
        }
    }
}

#[test]
fn batch_timer_persists_idle_changes_and_shutdown_flushes_account_scoped_data() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "line-receipts-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("messages.sqlite3");
    let store = Store::open(&path).unwrap();
    let (events, rx) = mpsc::channel();
    let (tx, commands) = mpsc::channel();
    let engine = Engine::new(
        store,
        Fake { events },
        Arc::new(AtomicU64::new(1)),
        root.join("legacy"),
    );
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "me".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Live {
        chat: "c1".into(),
        message: json!({"id":"10","mine":true}),
        generation: 1,
    })
    .unwrap();
    wait(&rx, |v| v["event"] == "message");
    tx.send(Command::SyncReceipts {
        chat: "c1".into(),
        generation: 1,
    })
    .unwrap();
    wait(&rx, |v| v["counts"]["10"] == 2);
    let mut reopened = Store::open(&path).unwrap();
    reopened.set_account("me");
    assert!(reopened.receipts("c1").unwrap().counts.is_empty());
    // No further command: the worker must wake itself to meet the 10-second flush.
    std::thread::sleep(Duration::from_secs(11));
    assert_eq!(reopened.receipts("c1").unwrap().counts["10"], 2);
    tx.send(Command::PeerRead {
        chat: "u1".into(),
        reader: "u1".into(),
        message: "9007199254740993".into(),
        generation: 1,
    })
    .unwrap();
    wait(&rx, |v| v["event"] == "read_counts" && v["chatMid"] == "u1");
    tx.send(Command::Session {
        account: "other".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Live {
        chat: "c1".into(),
        message: json!({"id":"10","mine":true,"readCount":7}),
        generation: 1,
    })
    .unwrap();
    wait(&rx, |v| {
        v["event"] == "message" && v["message"]["readCount"] == 7
    });
    assert_eq!(
        reopened.receipts("u1").unwrap().readers["u1"][0].end,
        9007199254740993
    );
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    reopened.set_account("other");
    assert_eq!(reopened.receipts("c1").unwrap().counts["10"], 7);
    reopened.clear().unwrap();
    assert!(reopened.receipts("c1").unwrap().counts.is_empty());
    reopened.set_account("me");
    assert_eq!(reopened.receipts("c1").unwrap().counts["10"], 2);
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cached_history_restores_counts_and_failed_range_rpc_uses_server_snapshot() {
    struct Fallback {
        events: mpsc::Sender<Value>,
    }
    impl Transport for Fallback {
        fn call(&self, method: &str, _: Value, _: u64) -> Result<Value> {
            match method {
                "bridge_read_ranges" => bail!("unsupported"),
                "bridge_fetch_page" => Ok(
                    json!({"messages":[{"id":"10","mine":true,"readCount":6}],"nextCursor":null,"hasMore":false}),
                ),
                _ => bail!("unexpected RPC"),
            }
        }
        fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
            Ok(row.clone())
        }
        fn emit(&self, event: Value) {
            self.events.send(event).unwrap();
        }
    }
    let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
    store.set_account("me");
    store
        .save_page(
            "c1",
            None,
            &line_rust_core::store::Page {
                messages: vec![json!({"id":"10","mine":true,"readCount":1})],
                next_cursor: None,
                has_more: false,
                cached: false,
            },
        )
        .unwrap();
    let mut saved = Receipts::default();
    saved.observe(&[json!({"id":"10","mine":true,"readCount":4})]);
    store
        .save_receipts(&[(
            "me".into(),
            "c1".into(),
            serde_json::to_string(&saved).unwrap(),
        )])
        .unwrap();
    let (events, rx) = mpsc::channel();
    let (tx, commands) = mpsc::channel();
    let engine = Engine::new(
        store,
        Fallback { events },
        Arc::new(AtomicU64::new(1)),
        std::path::PathBuf::new(),
    );
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "me".into(),
        generation: 1,
    })
    .unwrap();
    tx.send(Command::Fetch {
        id: 1,
        chat: "c1".into(),
        before: None,
        generation: 1,
    })
    .unwrap();
    let page = wait(&rx, |v| v["id"] == 1);
    assert_eq!(page["result"]["messages"][0]["readCount"], 4);
    assert_eq!(page["result"]["cached"], true);
    tx.send(Command::SyncReceipts {
        chat: "c1".into(),
        generation: 1,
    })
    .unwrap();
    wait(&rx, |v| v["counts"]["10"] == 6);
    let (done, flushed) = mpsc::sync_channel(1);
    tx.send(Command::FlushReceipts { done }).unwrap();
    flushed
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    tx.send(Command::Clear {
        id: 2,
        generation: 1,
    })
    .unwrap();
    wait(&rx, |v| v["id"] == 2);
    // Clear drops dirty and loaded state, so a later flush cannot resurrect it.
    tx.send(Command::SyncReceipts {
        chat: "c1".into(),
        generation: 1,
    })
    .unwrap();
    let cleared = wait(&rx, |v| v["event"] == "read_counts");
    assert_eq!(cleared["counts"], json!({}));
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
}
