use line_rust_core::{message::normalize, store::Store};
use serde_json::json;
use std::path::Path;

#[test]
fn reply_media_and_flex_are_interpreted_in_rust() {
    let row = normalize(
        &json!({"id":"9007199254740993","from":"me","to":"group","createdTime":"42","contentType":2,"relatedMessageId":"9007199254740992","messageRelationType":"REPLY","contentMetadata":{"FILE_NAME":"video.mp4","DURATION":"3000"}}),
        "me",
    );
    assert_eq!(row["id"], "9007199254740993");
    assert_eq!(row["relatedMessageId"], "9007199254740992");
    assert_eq!(row["contentType"], "VIDEO");
    assert_eq!(row["durationMs"], 3000);
    assert_eq!(row["mine"], true);
    let flex = normalize(
        &json!({"id":"1","contentType":"FLEX","contentMetadata":{"ALT_TEXT":"Card","FLEX_JSON":json!({"body":{"contents":[{"text":"Hello","action":{"type":"uri","label":"Open","uri":"https://example.test"}}]}}).to_string()}}),
        "me",
    );
    assert_eq!(flex["flex"]["texts"], json!(["Hello"]));
    assert_eq!(flex["flex"]["actions"][0]["uri"], "https://example.test");
}

#[test]
fn chat_previews_recover_textless_cached_media_and_follow_the_latest_message() {
    let mut store = Store::open(Path::new(":memory:")).unwrap();
    store.set_account("one");
    // Cover legacy normalized rows as well as freshly decoded API messages.
    for (content_type, expected) in [
        ("IMAGE", "Photo"),
        ("VIDEO", "Video"),
        ("AUDIO", "Voice message"),
        ("STICKER", "Sticker"),
        ("FILE", "File"),
        ("FLEX", "Flex message"),
        ("LOCATION", "Message"),
        ("UNKNOWN", "Message"),
    ] {
        for text in [json!(""), json!(" \n "), serde_json::Value::Null] {
            let mut catalog = json!({"chats":[{"mid":"chat","preview":"","lastActivity":10}]});
            store
                .save_messages(
                    "chat",
                    &[
                        json!({"id":"1","createdTime":1,"contentType":"NONE","text":"older text"}),
                        json!({"id":"2","createdTime":10,"contentType":content_type,"text":text}),
                    ],
                )
                .unwrap();
            store.decorate_chats(&mut catalog).unwrap();
            assert_eq!(catalog["chats"][0]["preview"], expected);
            assert_eq!(catalog["chats"][0]["lastActivity"], 10);
        }
        let row = normalize(
            &json!({"id":"2","createdTime":10,"contentType":content_type}),
            "one",
        );
        store.save_messages("chat", &[row]).unwrap();
        let mut catalog = json!({"chats":[{"mid":"chat"}]});
        store.decorate_chats(&mut catalog).unwrap();
        assert_eq!(catalog["chats"][0]["preview"], expected);
    }
    store
        .save_messages(
            "chat",
            &[json!({"id":"3","createdTime":20,"contentType":"NONE","text":"new text"})],
        )
        .unwrap();
    let mut catalog = json!({"chats":[{"mid":"chat","lastActivity":10}]});
    store.decorate_chats(&mut catalog).unwrap();
    assert_eq!(catalog["chats"][0]["preview"], "new text");
    assert_eq!(catalog["chats"][0]["lastActivity"], 20);
    store.set_account("two");
    let mut catalog = json!({"chats":[{"mid":"chat","preview":""}]});
    store.decorate_chats(&mut catalog).unwrap();
    assert_eq!(catalog["chats"][0]["preview"], "");
}

#[test]
fn catalogs_are_account_scoped_expire_and_preserve_recent_stickers() {
    let mut store = Store::open(Path::new(":memory:")).unwrap();
    store.set_account("one");
    store
        .save_catalog("list_chats", &json!({"chats":[{"mid":"u1"}]}))
        .unwrap();
    store
        .save_catalog("recent_stickers", &json!([{"stickerId":"1"}]))
        .unwrap();
    assert!(store.catalog("list_chats", 60_000).unwrap().is_some());
    store.set_account("two");
    assert!(store.catalog("list_chats", 60_000).unwrap().is_none());
    store.set_account("one");
    store.invalidate_catalogs().unwrap();
    assert!(store.catalog("list_chats", 60_000).unwrap().is_none());
    assert!(
        store
            .catalog("recent_stickers", i64::MAX)
            .unwrap()
            .is_some()
    );
}
