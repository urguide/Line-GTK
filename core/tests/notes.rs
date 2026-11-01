use line_rust_core::notes::normalize;
use serde_json::json;

#[test]
fn notes_keep_ids_ownership_contents_and_page_cursor() {
    let post = json!({"postInfo":{"postId":"9007199254740993","createdTime":123,"updatedTime":456,"likeCount":2,"commentCount":3,"liked":true},"writerInfo":{"writerMid":"me","writerName":"Author"},"contents":{"text":"hello","media":[{"objectId":"photo","type":"PHOTO"}]}});
    let list = normalize("list", &json!({"feeds":[{"post":post}]}), "me").unwrap();
    assert_eq!(list["posts"][0]["mine"], true);
    assert_eq!(list["posts"][0]["media"][0]["objectId"], "photo");
    assert_eq!(
        list["cursor"],
        json!({"postId":"9007199254740993","updatedTime":456})
    );
    let detail = normalize("get", &json!({"post":{"post":post},"comments":{"comments":[{"commentId":"99","writerInfo":{"writerName":"Friend"},"commentText":"reply"}],"scrollId":"next"}}), "other").unwrap();
    assert_eq!(detail["post"]["mine"], false);
    assert_eq!(detail["comments"][0]["text"], "reply");
    assert_eq!(detail["scrollId"], "next");
}

#[test]
fn malformed_notes_are_errors_but_empty_lists_are_valid() {
    assert!(normalize("list", &json!({}), "me").is_err());
    assert!(normalize("list", &json!({"feeds":[{"post":{}}]}), "me").is_err());
    assert_eq!(
        normalize("list", &json!({"feeds":[]}), "me").unwrap(),
        json!({"posts":[],"cursor":null})
    );
}
#[test]
fn post_and_comment_authors_use_contact_alias_once_per_author() {
    let mut notes = serde_json::json!({
        "post":{"author":"u1","authorName":"Public"},
        "comments":[{"author":"u1","authorName":"Public"},{"author":"u2","authorName":"Unknown"}]
    });
    let mut calls = Vec::new();
    line_rust_core::notes::resolve_author_names(&mut notes, |mid| {
        calls.push(mid.to_owned());
        if mid == "u1" {
            Ok(serde_json::json!({"displayName":"Public","displayNameOverridden":"Alias"}))
        } else {
            anyhow::bail!("offline")
        }
    });
    assert_eq!(notes["post"]["authorName"], "Alias");
    assert_eq!(notes["comments"][0]["authorName"], "Alias");
    assert_eq!(notes["comments"][1]["authorName"], "Unknown");
    assert_eq!(calls.iter().filter(|mid| *mid == "u1").count(), 1);
}
