use line_rust_core::mentions::{self, Member, Mention};
use serde_json::{Value, json};

fn mention(start: usize, label: &str, mid: &str) -> Mention {
    Mention {
        start,
        end: start + label.chars().count(),
        mid: mid.into(),
        label: label.into(),
    }
}

#[test]
fn completion_requires_a_boundary_and_filters_substrings_case_insensitively() {
    for (text, expected) in [
        ("@", Some((0, ""))),
        ("@Jo", Some((0, "jo"))),
        ("你好😀 @J", Some((4, "j"))),
        ("hello\t@J", Some((6, "j"))),
        ("mail@johnny", None),
        ("@@j", None),
        ("hi @j next", None),
    ] {
        assert_eq!(
            mentions::query(text, text.chars().count(), &[]),
            expected.map(|(s, q)| (s, q.into()))
        );
    }
    assert_eq!(mentions::query("@j suffix", 2, &[]), Some((0, "j".into())));
    assert_eq!(
        mentions::query("@Johnny ", 7, &[mention(0, "@Johnny", "u1")]),
        None
    );
    let names: Vec<_> = ["Johnny", "AJ", "Joanna", "Alice"]
        .into_iter()
        .map(|name| Member {
            mid: name.into(),
            name: name.into(),
        })
        .collect();
    let search = |query| {
        names
            .iter()
            .filter(|m| mentions::matches(m, query))
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(search("j"), ["Johnny", "AJ", "Joanna"]);
    assert_eq!(search("JO"), ["Johnny", "Joanna"]);
    assert_eq!(search("john"), ["Johnny"]);
    assert_eq!(search("zz"), Vec::<&str>::new());
}

#[test]
fn edits_shift_identity_but_modified_or_deleted_names_lose_it() {
    let mut items = vec![mention(0, "@Johnny", "u1"), mention(8, "@小明", "u2")];
    mentions::inserted(&mut items, 0, 2);
    assert_eq!((items[0].start, items[1].start), (2, 10));
    mentions::deleted(&mut items, 0, 2);
    mentions::inserted(&mut items, 2, 1);
    assert_eq!(items, [mention(9, "@小明", "u2")]);
    mentions::deleted(&mut items, 8, 2);
    assert!(items.is_empty());
    let mut items = vec![mention(0, "@Johnny", "u1")];
    mentions::inserted(&mut items, 7, 1);
    mentions::deleted(&mut items, 7, 1);
    assert_eq!(items, [mention(0, "@Johnny", "u1")]);
    mentions::deleted(&mut items, 0, 7);
    assert!(items.is_empty());
}

#[test]
fn trim_and_inline_emoji_preserve_utf16_mentions_and_distinct_same_name_members() {
    let input = "  😀[emoji:abc:001] @Johnny @Johnny  ";
    let start = input[..input.find('@').unwrap()].chars().count();
    let (input, items) = mentions::trimmed(
        input,
        &[
            mention(start, "@Johnny", "u1"),
            mention(start + 8, "@Johnny", "u2"),
        ],
    );
    let packs = json!({"packs":[{"id":"abc","stickers":[{"stickerId":"001","packageId":"abc","altText":"笑","version":"1"}]}]});
    let (text, metadata) = mentions::compose(&input, &packs, &items);
    assert_eq!(text, "😀(笑) @Johnny @Johnny");
    let wire: Value = serde_json::from_str(metadata["MENTION"].as_str().unwrap()).unwrap();
    assert_eq!(
        wire,
        json!({"MENTIONEES":[{"S":"6","E":"13","M":"u1"},{"S":"14","E":"21","M":"u2"}]})
    );
    assert_eq!(line_rust_core::expressions::resources(&metadata)[0]["S"], 2);
    assert_eq!(mentions::compose("@Johnny", &json!({}), &[]).1, json!({}));
    assert_eq!(
        mentions::compose("@Jonnny", &json!({}), &[mention(0, "@Johnny", "u1")]).1,
        json!({})
    );
}

#[test]
fn membership_excludes_invitees_and_unrelated_contacts_and_recovers_missing_profiles() {
    let mut calls = Vec::new();
    let result = mentions::members("c1", |method, args| {
        calls.push(method.to_owned());
        Ok(match method {
            "bridge_chat_members" => {
                assert_eq!(args["chatMid"], "c1");
                json!({"extra":{"groupExtra":{"memberMids":{"u1":"1","u2":"2"},"inviteeMids":{"u3":"3"}}}})
            }
            "bridge_contacts" => {
                assert_eq!(args["mids"], json!(["u1","u2"]));
                json!([{"mid":"u1","displayName":"Johnny"},{"mid":"u3","displayName":"Invited"}])
            }
            "bridge_contact" => {
                assert_eq!(args["mid"], "u2");
                json!({"mid":"u2","displayName":"AJ"})
            }
            _ => panic!("unexpected RPC {method}"),
        })
    }).unwrap();
    assert_eq!(
        result,
        json!({"members":[{"mid":"u2","name":"AJ"},{"mid":"u1","name":"Johnny"}]})
    );
    assert_eq!(
        calls,
        ["bridge_chat_members", "bridge_contacts", "bridge_contact"]
    );
    assert!(mentions::members("c1", |_, _| Ok(json!({}))).is_err());
    let dm = mentions::members("u1", |method, _| {
        assert_eq!(method, "bridge_contacts");
        Ok(json!([{"mid":"u1","displayName":"Johnny"}]))
    })
    .unwrap();
    assert_eq!(dm["members"].as_array().unwrap().len(), 1);
}
