use anyhow::{Result, bail};
use line_gtk_core::{
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

fn seed(store: &mut Store) {
    for account in ["me", "other"] {
        store.set_account(account);
        store
            .save_catalog(
                "list_chats",
                &json!({"chats":[{"mid":"u1","name":"Old chat"}]}),
            )
            .unwrap();
        store
            .save_catalog(
                "list_friends",
                &json!({"friends":[{"mid":"u1","name":"Old friend"}]}),
            )
            .unwrap();
        store
            .save_catalog("recent_stickers", &json!([{"stickerId":"1"}]))
            .unwrap();
        store
            .save_messages("u1", &[json!({"id":"123","text":"old message"})])
            .unwrap();
    }
    store.set_account("me");
}

#[test]
fn clear_scopes_delete_lists_only_when_requested_and_preserve_other_accounts() {
    for kind in ["chats", "all", "messages", "media", "stickers", "avatars"] {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        seed(&mut store);
        store.clear_scope(kind).unwrap();
        for catalog in ["list_chats", "list_friends"] {
            assert_eq!(
                store.stale_catalog(catalog, i64::MAX).unwrap().is_none(),
                matches!(kind, "chats" | "all")
            );
        }
        assert_eq!(
            store.message("u1", "123").unwrap().is_none(),
            matches!(kind, "messages" | "all")
        );
        assert!(
            store
                .catalog("recent_stickers", i64::MAX)
                .unwrap()
                .is_some()
        );
        store.set_account("other");
        assert!(store.catalog("list_chats", i64::MAX).unwrap().is_some());
        assert!(store.catalog("list_friends", i64::MAX).unwrap().is_some());
        assert!(store.message("u1", "123").unwrap().is_some());
    }
}

struct Offline {
    events: mpsc::Sender<Value>,
}
impl Transport for Offline {
    fn call(&self, _: &str, _: Value, _: u64) -> Result<Value> {
        unreachable!()
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        Ok(row.clone())
    }
    fn application(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        if method == "clear_cache" {
            return Ok(json!({"cleared":p["kind"]}));
        }
        assert!(matches!(method, "list_chats" | "list_friends"));
        assert!(p["previous"]["chats"].is_null());
        assert!(p["previous"]["friends"].is_null());
        line_gtk_core::catalog::load(method, &p["previous"], |_, _| bail!("offline"))
    }
}

fn run_offline(store: Store, legacy: PathBuf, clear: Option<&str>) {
    let (events, rx) = mpsc::channel();
    let (tx, commands) = mpsc::channel();
    let engine = Engine::new(
        store,
        Offline { events },
        Arc::new(AtomicU64::new(1)),
        legacy,
    );
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "me".into(),
        generation: 1,
    })
    .unwrap();
    if let Some(kind) = clear {
        tx.send(Command::Operation {
            id: 1,
            method: "clear_cache".into(),
            params: json!({"kind":kind}),
            generation: 1,
        })
        .unwrap();
        let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(result["ok"], true);
    }
    for (id, method) in [(2, "list_chats"), (3, "list_friends")] {
        tx.send(Command::Operation {
            id,
            method: method.into(),
            params: json!({}),
            generation: 1,
        })
        .unwrap();
        let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(result["id"], id);
        assert_eq!(result["ok"], false);
        assert!(result["result"].is_null());
    }
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
}

#[test]
fn cleared_lists_cannot_return_via_offline_refresh_or_restart() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for kind in ["chats", "all"] {
        let root = std::env::temp_dir().join(format!(
            "line-clear-catalogs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("cache.sqlite3");
        let mut store = Store::open(&db).unwrap();
        seed(&mut store);
        run_offline(store, root.join("legacy"), Some(kind));
        run_offline(Store::open(&db).unwrap(), root.join("legacy"), None);
        let mut store = Store::open(&db).unwrap();
        store.set_account("other");
        assert_eq!(
            store.catalog("list_chats", i64::MAX).unwrap().unwrap()["chats"][0]["name"],
            "Old chat"
        );
        assert_eq!(
            store.catalog("list_friends", i64::MAX).unwrap().unwrap()["friends"][0]["name"],
            "Old friend"
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
