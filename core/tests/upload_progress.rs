use anyhow::{Result, bail};
use line_rust_core::{
    attachments::upload_progress,
    store::Store,
    sync::{Command, Engine, Transport},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicU64, mpsc},
    time::Duration,
};

struct Upload {
    events: mpsc::Sender<Value>,
    fail_at: Option<f64>,
}
impl Transport for Upload {
    fn call(&self, _: &str, _: Value, _: u64) -> Result<Value> {
        unreachable!()
    }
    fn materialize(&self, row: &Value, _: u64) -> Result<Value> {
        Ok(row.clone())
    }
    fn emit(&self, event: Value) {
        self.events.send(event).unwrap();
    }
    fn application(&self, method: &str, p: Value, _: u64) -> Result<Value> {
        assert!(matches!(method, "send_media" | "send_audio"));
        if self.fail_at == Some(0.0) {
            bail!("missing file");
        }
        for (fraction, label) in [
            (0.05, "Reading file…"),
            (0.10, "Preparing preview…"),
            (0.35, "Uploading…"),
            (0.90, "Saving attachment…"),
        ] {
            self.emit(upload_progress(
                p["chatMid"].as_str().unwrap(),
                fraction,
                label,
                false,
            ));
            if self.fail_at == Some(fraction) {
                bail!("failed at {fraction}");
            }
        }
        Ok(
            json!({"rawMessage":{"id":"9007199254740993","contentType":"FILE","text":"attachment","createdTime":1}}),
        )
    }
}

#[test]
fn uploads_finish_once_before_response_on_success_and_every_early_error() {
    for method in ["send_media", "send_audio"] {
        for fail_at in [None, Some(0.0), Some(0.10), Some(0.35), Some(0.90)] {
            let store = Store::open(Path::new(":memory:")).unwrap();
            let (events, rx) = mpsc::channel();
            let (tx, commands) = mpsc::channel();
            let engine = Engine::new(
                store,
                Upload { events, fail_at },
                Arc::new(AtomicU64::new(1)),
                PathBuf::from("/nonexistent/legacy"),
            );
            let worker = std::thread::spawn(move || engine.run(commands));
            tx.send(Command::Session {
                account: "me".into(),
                generation: 1,
            })
            .unwrap();
            tx.send(Command::Operation {
                id: 42,
                method: method.into(),
                params: json!({"chatMid":"u1"}),
                generation: 1,
            })
            .unwrap();
            let mut progress = Vec::new();
            let response = loop {
                let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                if event["id"] == 42 {
                    break event;
                }
                assert_eq!(event["event"], "upload_progress");
                assert_eq!(event["chatMid"], "u1");
                progress.push(event);
            };
            tx.send(Command::Stop).unwrap();
            worker.join().unwrap();
            assert_eq!(progress[0]["progress"], 0.02);
            assert_eq!(progress.iter().filter(|p| p["done"] == true).count(), 1);
            let finished = progress.last().unwrap();
            assert_eq!(finished["done"], true);
            assert_eq!(
                finished["progress"],
                if fail_at.is_some() { 0.0 } else { 1.0 }
            );
            assert_eq!(response["ok"], fail_at.is_none());
            assert_eq!(
                finished["label"],
                if fail_at.is_some() { "" } else { "Sent" }
            );
            assert!(rx.try_recv().is_err());
        }
    }
}
