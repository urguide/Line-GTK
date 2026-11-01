use anyhow::bail;
use line_rust_core::{expressions, stickers};
use serde_json::{Value, json};

#[test]
fn owned_catalog_fetches_all_pages_including_short_pages_and_thrift_fields() {
    let mut offsets = Vec::new();
    let result = expressions::owned(|offset| {
        offsets.push(offset);
        Ok(match offset {
            0 => json!({"productList":(0..80).map(|id|json!({"id":id.to_string()})).collect::<Vec<_>>(),"totalSize":123}),
            80 => json!({"1":(80..120).map(|id|json!({"1":id.to_string(),"11":"Pack","21":2})).collect::<Vec<_>>(),"3":123}),
            120 => json!({"productList":[{"id":"120"},{"id":"121"},{"id":"122"}],"totalSize":123}),
            _ => panic!("unexpected page"),
        })
    }).unwrap();
    assert_eq!(offsets, [0, 80, 120]);
    assert_eq!(result["products"].as_array().unwrap().len(), 123);
    assert_eq!(result["products"][80]["latestVersion"], 2);
    assert!(expressions::owned(|_| Ok(json!({"productList":[{"id":"1"}]}))).is_err());
    assert!(
        expressions::owned(|offset| if offset == 0 {
            Ok(json!({"productList":[{"id":"1"}]}))
        } else {
            bail!("offline")
        })
        .is_err()
    );
}

#[test]
fn emoji_metadata_preserves_leading_zero_ids_and_utf16_positions() {
    let pack = expressions::emoji_pack(
        &json!({"orders":["001","002"],"altTexts":{"001":"笑","002":"❤️"}}),
        &json!({"id":"abc123","name":"Emoji","latestVersion":7}),
    )
    .unwrap();
    let library = json!({"packs":[pack]});
    let (text, metadata) =
        expressions::compose("好😀[emoji:abc123:001]文字[emoji:abc123:002]", &library);
    assert_eq!(text, "好😀(笑)文字(❤️)");
    let resources = expressions::resources(&metadata);
    assert_eq!(resources[0]["S"], 3);
    assert_eq!(resources[0]["E"], 6);
    assert_eq!(resources[0]["sticonId"], "001");
    assert_eq!(resources[0]["version"], 7);
    assert_eq!(resources[1]["S"], 8);
    assert_eq!(
        serde_json::from_str::<Value>(metadata["STICON_OWNERSHIP"].as_str().unwrap()).unwrap(),
        json!(["abc123"])
    );
    assert_eq!(expressions::char_offset(&text, 3), Some(2));
    assert_eq!(expressions::char_offset(&text, 2), None);
    for input in [
        "[emoji:unknown:001]",
        "未完成[emoji:abc",
        "普通文字😀",
        "[emoji:abc123:003]",
    ] {
        assert_eq!(
            expressions::compose(input, &library),
            (input.into(), json!({}))
        );
    }
    assert!(
        expressions::emoji_pack(&json!({"orders":["../escape"]}), &json!({"id":"abc"})).is_err()
    );
}

#[test]
fn sticker_catalog_does_not_truncate_packs_or_items() {
    let products: Vec<_> = (1..=15).map(|id| json!({"id":id.to_string()})).collect();
    let result = stickers::load(
        Ok(json!({"productList":products})),
        &Value::Null,
        &json!([]),
        |_| {
            Ok(
                json!({"stickers":(1..=60).map(|id|json!({"id":id})).collect::<Vec<_>>()} )
                    .to_string()
                    .into_bytes(),
            )
        },
        |id| Ok(format!("/{id}.png")),
    )
    .unwrap();
    assert_eq!(result["packs"].as_array().unwrap().len(), 15);
    assert!(
        result["packs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["stickers"].as_array().unwrap().len() == 60)
    );
}
