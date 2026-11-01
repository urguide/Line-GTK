use line_rust_core::session::authentication_failure;

#[test]
fn an_old_session_timer_cannot_reset_crash_recovery_budget() {
    use line_rust_core::session::recovery_is_stable;
    assert!(recovery_is_stable(Some(100), Some(100), true, true));
    for (expected, current, running, ready) in [
        (Some(100), Some(101), true, true),
        (Some(100), Some(100), false, true),
        (Some(100), Some(100), true, false),
        (None, None, true, true),
    ] {
        assert!(!recovery_is_stable(expected, current, running, ready));
    }
}

#[test]
fn only_authentication_codes_terminate_a_session() {
    for error in [
        "NOT_AUTHORIZED",
        "TalkException: NOT_AUTHORIZED_DEVICE",
        "LOGGED_OUT",
        "V3_TOKEN_CLIENT_LOGGED_OUT",
        "AUTHENTICATION",
        "AUTHENTICATION_FAILED",
        "{\"code\":\"AUTHENTICATION_ERROR\"}",
    ] {
        assert!(authentication_failure(error), "{error}");
    }
    for error in [
        "message_decode_failed",
        "cannot read contentMetadata",
        "network timeout",
        "E2EE key missing",
        "USER_NOT_STICKER_OWNER",
        "authentication server temporarily unreachable",
        "NOT_AUTHORIZED_DEVICE_EXTRA",
        "",
    ] {
        assert!(!authentication_failure(error), "{error}");
    }
}

#[test]
fn failed_live_message_and_following_good_message_both_reach_ui_and_disk() {
    use line_rust_core::{
        store::Store,
        sync::{Command, Engine, Transport},
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, atomic::AtomicU64, mpsc};
    struct Fake(mpsc::Sender<Value>);
    impl Transport for Fake {
        fn call(&self, _: &str, _: Value, _: u64) -> anyhow::Result<Value> {
            unreachable!()
        }
        fn materialize(&self, _: &Value, _: u64) -> anyhow::Result<Value> {
            panic!("failed image must not download")
        }
        fn emit(&self, e: Value) {
            self.0.send(e).unwrap();
        }
    }
    let root = std::env::temp_dir().join(format!("line-live-decode-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("cache.sqlite");
    let (events, rx) = mpsc::channel();
    let (tx, commands) = mpsc::channel();
    let engine = Engine::new(
        Store::open(&db).unwrap(),
        Fake(events),
        Arc::new(AtomicU64::new(1)),
        root.join("legacy"),
    );
    let worker = std::thread::spawn(move || engine.run(commands));
    tx.send(Command::Session {
        account: "me".into(),
        generation: 1,
    })
    .unwrap();
    for (id, failed) in [("1", true), ("2", false)] {
        let raw = json!({"id":id,"from":"peer","to":"me","contentType":if failed{"IMAGE"}else{"NONE"},"text":if failed{"cipher"}else{"hello"},"chunks":if failed{json!(["encrypted"])}else{json!([])}});
        tx.send(Command::Live{chat:"peer".into(),message:json!({"rawMessage":raw,"decodedMessage":if failed{Value::Null}else{raw.clone()},"decodeError":if failed{json!("message_decode_failed")}else{Value::Null}}),generation:1}).unwrap();
        let event = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(event["event"], "message");
        assert_eq!(
            event["message"]["text"],
            if failed {
                "[Unable to decrypt message]"
            } else {
                "hello"
            }
        );
    }
    tx.send(Command::Stop).unwrap();
    worker.join().unwrap();
    let mut store = Store::open(&db).unwrap();
    store.set_account("me");
    for id in ["1", "2"] {
        assert!(store.message("peer", id).unwrap().is_some());
    }
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
