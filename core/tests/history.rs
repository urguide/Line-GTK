use line_rust_core::store::{Cursor, Page, Store};
use serde_json::json;
use std::path::Path;

fn page(start: u64, count: u64) -> Page {
    Page {
        messages: (start - count + 1..=start)
            .map(|i| json!({"id":i.to_string(),"createdTime":i,"text":"hello"}))
            .collect(),
        next_cursor: Some(Cursor {
            message_id: (start - count + 1).to_string(),
            delivered_time: (start - count + 1).to_string(),
        }),
        has_more: count == 20,
        cached: false,
    }
}

#[test]
fn atomic_pages_preserve_history_paths_and_gaps() {
    let mut store = Store::open(Path::new(":memory:")).unwrap();
    store.set_account("u1");
    store
        .save_messages(
            "chat",
            &[json!({"id":"100","createdTime":100,"filePath":"/cached/file"})],
        )
        .unwrap();
    let first = store.save_page("chat", None, &page(150, 20)).unwrap();
    assert_eq!(first.messages.len(), 20);
    let gap = store.gap("chat").unwrap().unwrap();
    assert_eq!(gap.target_id, "100");
    store
        .save_page("chat", first.next_cursor.as_ref(), &page(130, 20))
        .unwrap();
    store
        .save_page(
            "chat",
            Some(&Cursor {
                message_id: "111".into(),
                delivered_time: "111".into(),
            }),
            &page(110, 20),
        )
        .unwrap();
    assert!(store.message("chat", "101").unwrap().is_some());
    assert_eq!(
        store.message("chat", "100").unwrap().unwrap()["filePath"],
        "/cached/file"
    );
    store.advance("chat", &gap, None).unwrap();
    assert!(store.gap("chat").unwrap().is_none());
    store.unsend("chat", "100").unwrap();
    store
        .save_messages("chat", &[json!({"id":"100","createdTime":100})])
        .unwrap();
    assert!(store.message("chat", "100").unwrap().is_none());
    store.set_account("u2");
    assert!(store.message("chat", "150").unwrap().is_none());
}

#[test]
fn large_ids_and_invalid_cursor() {
    let mut store = Store::open(Path::new(":memory:")).unwrap();
    store.set_account("u1");
    store
        .save_messages("chat", &[json!({"id":"9007199254740993","createdTime":1})])
        .unwrap();
    assert!(store.message("chat", "9007199254740993").unwrap().is_some());
    assert!(
        Cursor {
            message_id: "../escape".into(),
            delivered_time: "1".into()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn legacy_import_is_idempotent_and_does_not_resurrect_cleared_cache() {
    let root = std::env::temp_dir().join(format!("line-core-migration-{}", std::process::id()));
    std::fs::create_dir_all(root.join("messages")).unwrap();
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::write(root.join("messages/chat.json"),json!({"at":time,"messages":[{"id":"1","createdTime":1},{"id":"2","createdTime":2}],"pages":{"3":{"ids":["1","2"],"nextCursor":{"messageId":"1","deliveredTime":"1"},"hasMore":false}},"gaps":{"gap":{"before":{"messageId":"3","deliveredTime":"3"},"targetId":"1"}},"syncHead":"2"}).to_string()).unwrap();
    std::fs::write(root.join("unsent.json"), "[\"1\"]").unwrap();
    let mut store = Store::open(Path::new(":memory:")).unwrap();
    store.set_account("u1");
    store.import_legacy(&root.join("messages")).unwrap();
    assert!(store.message("chat", "1").unwrap().is_none());
    assert!(store.message("chat", "2").unwrap().is_some());
    assert!(store.gap("chat").unwrap().is_some());
    store.clear().unwrap();
    store.import_legacy(&root.join("messages")).unwrap();
    assert!(store.message("chat", "2").unwrap().is_none());
    store.set_account("u2");
    store.import_legacy(&root.join("messages")).unwrap();
    assert!(store.message("chat", "2").unwrap().is_none());
    assert!(root.join("messages/chat.json").is_file());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_stickers_import_without_history_merge_once_and_are_account_scoped() {
    let root = std::env::temp_dir().join(format!("line-stickers-migration-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("stickers-index.json");
    let source = json!([
        {"stickerId":"10","packageId":"20","version":"1","at":1},
        {"stickerId":11,"packageId":20,"version":2,"at":2},
        {"stickerId":"11","packageId":"20","at":0},
        {"stickerId":"../bad","packageId":"20"}
    ])
    .to_string();
    std::fs::write(&path, &source).unwrap();
    let db = root.join("history.sqlite3");
    let mut store = Store::open(&db).unwrap();
    store.set_account("first");
    store
        .save_catalog(
            "recent_stickers",
            &json!([{"stickerId":"10","packageId":"20","version":"9"}]),
        )
        .unwrap();
    store.import_legacy(&root.join("cache/messages")).unwrap();
    let recent = store.catalog("recent_stickers", i64::MAX).unwrap().unwrap();
    assert_eq!(recent.as_array().unwrap().len(), 2);
    assert_eq!(recent[0]["version"], "9");
    assert_eq!(recent[1]["stickerId"], "11");
    assert_eq!(recent[1]["version"], "2");
    drop(store);
    let mut reopened = Store::open(&db).unwrap();
    reopened.set_account("first");
    reopened
        .save_catalog("recent_stickers", &json!([]))
        .unwrap();
    reopened
        .import_legacy(&root.join("cache/messages"))
        .unwrap();
    assert_eq!(
        reopened
            .catalog("recent_stickers", i64::MAX)
            .unwrap()
            .unwrap(),
        json!([])
    );
    reopened.set_account("second");
    reopened
        .import_legacy(&root.join("cache/messages"))
        .unwrap();
    assert!(
        reopened
            .catalog("recent_stickers", i64::MAX)
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}
