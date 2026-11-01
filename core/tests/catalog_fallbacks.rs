use anyhow::{Result, bail};
use line_rust_core::{catalog, postback};
use serde_json::{Value, json};

fn row<'a>(result: &'a Value, key: &str, mid: &str) -> &'a Value {
    result[key]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["mid"] == mid)
        .unwrap()
}

#[test]
fn custom_alias_precedes_public_names_in_every_contact_shape() {
    for profile in [
        json!({"displayName":"Public", "displayNameOverridden":" My alias "}),
        json!({"displayName":"Public", "raw":{"displayNameOverridden":"My alias"}}),
        json!({"displayName":"Public", "raw":{"contact":{"displayNameOverridden":"My alias"}}}),
        json!({"displayName":"Public", "targetProfileDetail":{"displayNameOverridden":"My alias"}}),
    ] {
        assert_eq!(catalog::name(&profile), Some("My alias"));
    }
    assert_eq!(
        catalog::name(&json!({"displayName":"Public", "displayNameOverridden":" "})),
        Some("Public")
    );
}

#[test]
fn catalog_alias_survives_partial_profiles_and_can_be_renamed_or_cleared() {
    let previous =
        json!({"friends":[{"mid":"u1","name":"Old alias","displayNameOverridden":"Old alias"}]});
    for (profile, expected) in [
        (json!({"mid":"u1","displayName":"Public"}), "Old alias"),
        (
            json!({"mid":"u1","displayName":"Public","displayNameOverridden":"New alias"}),
            "New alias",
        ),
        (
            json!({"mid":"u1","displayName":"Public","displayNameOverridden":""}),
            "Public",
        ),
    ] {
        let result = catalog::load("list_friends", &previous, |method, _| match method {
            "bridge_contact_ids" => Ok(json!(["u1"])),
            "bridge_contacts" => Ok(json!([profile])),
            _ => panic!("unexpected {method}"),
        })
        .unwrap();
        assert_eq!(result["friends"][0]["name"], expected);
    }
}

#[test]
fn every_legacy_avatar_shape_and_friend_name_is_preserved() {
    let shapes = [
        json!({"raw":{"chatName":"Group","picturePath":" /group "}}),
        json!({"picturePath":" ","pictureStatus":"/group"}),
        json!({"thumbnailUrl":"https://profile.line-scdn.net/group"}),
        json!({"raw":{"contact":{"pictureStatus":"/group"}}}),
        json!({"raw":{"targetProfileDetail":{"picturePath":"/group"}}}),
        json!({"targetProfileDetail":{"pictureStatus":"/group"}}),
        json!({"picture":{"path":"/group"}}),
        json!({"raw":{"picture":{"path":"/group"}}}),
        json!({"extra":{"groupExtra":{"picturePath":"/group"}}}),
    ];
    for shape in shapes {
        assert!(
            catalog::picture(&shape).unwrap().ends_with("/group"),
            "{shape}"
        );
    }
    assert_eq!(
        catalog::picture(&json!({"picturePath":"/primary","pictureStatus":"/secondary"})),
        Some("/primary")
    );
    assert_eq!(
        catalog::picture(&json!({"raw":null,"picturePath":" "})),
        None
    );
    assert_eq!(
        catalog::name(
            &json!({"mid":"u1","raw":{"targetProfileDetail":{"profileName":" Friend "}}})
        ),
        Some("Friend")
    );
    assert_eq!(
        catalog::name(&json!({"raw":{"contact":{"displayName":"Friend"}}})),
        Some("Friend")
    );
}

#[test]
fn failed_batch_and_group_query_keep_boxes_cache_and_individual_contacts() {
    let previous = json!({"chats":[{"mid":"c1","name":"Cached group","avatarPath":"/cache/group","muted":true},{"mid":"u3","name":"Cached friend","avatarPath":"/cache/friend","muted":true}]});
    let mut singles = Vec::new();
    let result=catalog::load("list_chats",&previous,|method,p|->Result<Value>{
        match method {
            "bridge_boxes"=>Ok(json!({"messageBoxes":[{"id":"u1","lastDeliveredMessageId":{"deliveredTime":123},"unreadCount":7},{"id":"c1","unreadCount":3}]})),
            "bridge_contact_ids"=>Ok(json!(["u1","u2","u3"])),
            "bridge_contacts"=>bail!("batch API unavailable"),
            "bridge_users"=>Ok(json!([{"mid":"u1","raw":{"targetProfileDetail":{"profileName":"One","pictureStatus":"/one"}}}])),
            "bridge_contact"=>{singles.push(p["mid"].clone());if p["mid"]=="u2"{Ok(json!({"mid":"u2","displayName":"Two"}))}else{bail!("unreachable contact")}},
            "bridge_groups"=>bail!("group API unavailable"),
            _=>panic!("unexpected {method}"),
        }
    }).unwrap();
    assert_eq!(singles, json!(["u2", "u3"]).as_array().unwrap().clone());
    assert_eq!(row(&result, "chats", "u1")["name"], "One");
    assert_eq!(row(&result, "chats", "u1")["unread"], 7);
    assert_eq!(row(&result, "chats", "u2")["name"], "Two");
    assert_eq!(row(&result, "chats", "u3")["name"], "Cached friend");
    assert_eq!(row(&result, "chats", "u3")["muted"], true);
    assert_eq!(row(&result, "chats", "c1")["avatarPath"], "/cache/group");
    assert_eq!(result["partial"], true);
}

#[test]
fn missing_batch_rows_use_users_then_individual_lookup() {
    let result = catalog::load("list_friends", &Value::Null, |method, p| -> Result<Value> {
        match method {
            "bridge_contact_ids" => Ok(json!(["u1", "u2"])),
            "bridge_contacts" => Ok(json!([{"mid":"u1","displayName":"One"}])),
            "bridge_users" => bail!("fetchUsers failed"),
            "bridge_contact" => {
                assert_eq!(p["mid"], "u2");
                Ok(json!({"mid":"u2","displayName":"Two","picture":{"path":"/two"}}))
            }
            _ => panic!("unexpected {method}"),
        }
    })
    .unwrap();
    assert_eq!(result["friends"].as_array().unwrap().len(), 2);
    assert_eq!(row(&result, "friends", "u2")["picturePath"], "/two");
}

#[test]
fn id_api_failure_uses_users_and_total_failure_uses_account_cache() {
    let result = catalog::load("list_friends", &Value::Null, |method, _| -> Result<Value> {
        if method == "bridge_users" {
            Ok(json!([{"mid":"u1","raw":{"contact":{"displayName":"Recovered"}}}]))
        } else {
            bail!("offline")
        }
    })
    .unwrap();
    assert_eq!(row(&result, "friends", "u1")["name"], "Recovered");
    let previous = json!({"chats":[{"mid":"c1","name":"Group"}],"friends":[{"mid":"u1","name":"Saved","avatarPath":"/old"}]});
    let offline = catalog::load("list_friends", &previous, |_, _| bail!("offline")).unwrap();
    assert_eq!(offline["friends"].as_array().unwrap().len(), 1);
    assert_eq!(offline["cached"], true);
    assert_eq!(row(&offline, "friends", "u1")["avatarPath"], "/old");
    assert!(catalog::load("list_chats", &Value::Null, |_, _| bail!("offline")).is_err());
}

#[test]
fn healthy_catalog_does_not_issue_fallback_requests_and_empty_is_not_an_error() {
    let mut calls = Vec::new();
    let result = catalog::load("list_chats", &Value::Null, |method, _| {
        calls.push(method.to_owned());
        Ok(match method {
            "bridge_boxes" => json!({"messageBoxes":[]}),
            "bridge_contact_ids" => json!(["u1"]),
            "bridge_contacts" => json!([{"mid":"u1","displayName":"Friend"}]),
            "bridge_groups" => {
                json!([{"raw":{"chatMid":"c1","chatName":"Group","picturePath":"/group"}}])
            }
            _ => panic!("unnecessary fallback {method}"),
        })
    })
    .unwrap();
    assert_eq!(
        calls,
        vec![
            "bridge_boxes",
            "bridge_contact_ids",
            "bridge_contacts",
            "bridge_groups"
        ]
    );
    assert_eq!(result["partial"], false);
    assert_eq!(row(&result, "chats", "c1")["name"], "Group");
    assert_eq!(
        catalog::load("list_friends", &Value::Null, |_, _| Ok(json!([]))).unwrap()["friends"],
        json!([])
    );
}

#[test]
fn postback_success_never_sends_text_and_fallback_preserves_error() {
    let mut calls = Vec::new();
    let result = postback::send(&json!({"chatMid":"u1","data":"click"}), |method, _| {
        calls.push(method.to_owned());
        Ok(json!({"sent":true}))
    })
    .unwrap();
    assert_eq!(calls, vec!["bridge_postback"]);
    assert_eq!(result["sent"], true);
    for (p, text) in [
        (
            json!({"data":"click","uri":"https://example.test"}),
            "click",
        ),
        (
            json!({"data":"","uri":"https://example.test"}),
            "https://example.test",
        ),
        (json!({}), "OK"),
    ] {
        let mut p = p;
        p["chatMid"] = json!("c1");
        let mut calls = Vec::new();
        let result = postback::send(&p, |method, args| {
            calls.push(method.to_owned());
            if method == "bridge_postback" {
                bail!("unsupported");
            }
            assert_eq!(args, json!({"chatMid":"c1","text":text,"e2ee":true}));
            Ok(json!({"rawMessage":{"id":"9007199254740993","text":text}}))
        })
        .unwrap();
        assert_eq!(calls, vec!["bridge_postback", "bridge_send_text"]);
        assert_eq!(result["fallback"], "message");
        assert_eq!(result["rawMessage"]["id"], "9007199254740993");
    }
    assert_eq!(
        postback::send(&json!({}), |method, _| bail!("{method} failed"))
            .unwrap_err()
            .to_string(),
        "bridge_send_text failed"
    );
}
