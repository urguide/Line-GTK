use anyhow::bail;
use line_rust_core::stickers;
use serde_json::{Value, json};

fn pack<'a>(result: &'a Value, id: &str) -> Option<&'a Value> {
    result["packs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
}

#[test]
fn single_download_failure_or_corrupt_metadata_never_hides_other_packs_or_recents() {
    for corrupt in [false, true] {
        let mut requested = Vec::new();
        let result=stickers::load(Ok(json!({"productList":[{"id":"1"},{"id":"2"},{"id":"3"},{"id":"../bad"}]})),&Value::Null,&json!([{"packageId":"1","stickerId":"10"}]),|id|{
            requested.push(id.to_owned());
            if id=="2" {if corrupt{return Ok(b"invalid json".to_vec());}bail!("HTTP 503");}
            Ok(json!({"title":{"en":format!("Pack {id}")},"version":1,"stickers":[{"id":format!("{id}0")}]}).to_string().into_bytes())
        },|id|Ok(format!("/download/{id}.png"))).unwrap();
        assert_eq!(requested, vec!["1", "2", "3"]);
        assert!(pack(&result, "1").is_some());
        assert!(pack(&result, "2").is_none());
        assert!(pack(&result, "3").is_some());
        assert_eq!(
            pack(&result, "__recent__").unwrap()["stickers"][0]["stickerId"],
            "10"
        );
        assert_eq!(result["partial"], true);
    }
}

#[test]
fn previous_pack_and_existing_images_survive_failed_metadata_and_image_fetch() {
    let root = std::env::temp_dir().join(format!("line-sticker-fallback-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let image = root.join("20.png");
    std::fs::write(&image, b"cached image").unwrap();
    let previous = json!({"packs":[{"id":"2","name":"Saved","version":"9","iconPath":image,"stickers":[{"packageId":"2","stickerId":"20","imagePath":image}]}]});
    let result = stickers::load(
        Ok(json!({"productList":[{"id":"2"},{"id":"3"}]})),
        &previous,
        &json!([{"packageId":"2","stickerId":"20"}]),
        |id| {
            if id == "2" {
                return Ok(b"{\"stickers\":null}".to_vec());
            }
            Ok(br#"{"stickers":[{"id":30}],"title":"New"}"#.to_vec())
        },
        |id| {
            if id == "20" {
                bail!("offline");
            }
            Ok("/new/30.png".into())
        },
    )
    .unwrap();
    assert_eq!(pack(&result, "2").unwrap()["name"], "Saved");
    assert_eq!(pack(&result, "2").unwrap()["version"], "9");
    assert_eq!(
        pack(&result, "2").unwrap()["stickers"][0]["imagePath"],
        json!(image)
    );
    assert_eq!(
        pack(&result, "__recent__").unwrap()["stickers"][0]["imagePath"],
        json!(image)
    );
    assert!(pack(&result, "3").is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn ownership_refresh_failure_uses_only_known_owned_cache_and_retry_can_recover() {
    let previous = json!({"products":[{"id":"1"}],"packs":[{"id":"1","name":"Saved","stickers":[{"packageId":"1","stickerId":"10"}]}]});
    let recent = json!([{"packageId":"1","stickerId":"10"},{"packageId":"2","stickerId":"20"}]);
    let offline = stickers::load(
        Err(anyhow::anyhow!("offline")),
        &previous,
        &recent,
        |_| bail!("offline"),
        |_| bail!("offline"),
    )
    .unwrap();
    assert!(pack(&offline, "1").is_some());
    assert_eq!(
        pack(&offline, "__recent__").unwrap()["stickers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let recovered = stickers::load(
        Ok(json!({"1":[{"1":"1","11":"Name","21":"2"}]})),
        &offline,
        &recent,
        |_| Ok(br#"{"version":2,"stickers":[{"id":10}]}"#.to_vec()),
        |_| Ok("/image.png".into()),
    )
    .unwrap();
    assert_eq!(recovered["partial"], false);
    assert_eq!(pack(&recovered, "1").unwrap()["version"], "2");
    let removed = stickers::load(
        Ok(json!({"productList":[]})),
        &previous,
        &recent,
        |_| panic!("no owned packs"),
        |_| panic!("no owned stickers"),
    )
    .unwrap();
    assert_eq!(removed["packs"], json!([]));
    assert!(
        stickers::load(
            Err(anyhow::anyhow!("offline")),
            &Value::Null,
            &recent,
            |_| unreachable!(),
            |_| unreachable!()
        )
        .is_err()
    );
}
