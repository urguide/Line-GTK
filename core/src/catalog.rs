//! Partial-failure recovery and LINE profile compatibility, owned by Rust.
use crate::message::{integer, string};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn text(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn sources(row: &Value) -> [&Value; 8] {
    [
        row,
        &row["targetProfileDetail"],
        &row["raw"],
        &row["raw"]["targetProfileDetail"],
        &row["raw"]["contact"],
        &row["contact"],
        &row["extra"]["groupExtra"],
        &row["raw"]["extra"]["groupExtra"],
    ]
}

pub fn picture(row: &Value) -> Option<&str> {
    for source in sources(row) {
        for candidate in [
            &source["picturePath"],
            &source["pictureStatus"],
            &source["picture"]["path"],
            &source["thumbnailUrl"],
        ] {
            if let Some(path) = text(candidate) {
                return Some(path);
            }
        }
    }
    None
}

pub fn name(row: &Value) -> Option<&str> {
    for source in sources(row) {
        for key in ["displayName", "profileName", "chatName", "name"] {
            if let Some(name) = text(&source[key]) {
                return Some(name);
            }
        }
    }
    None
}

fn mid(row: &Value) -> Option<&str> {
    for source in sources(row) {
        for key in ["mid", "chatMid", "targetUserMid"] {
            if let Some(mid) = text(&source[key]) {
                return Some(mid);
            }
        }
    }
    None
}

fn entry(mid: &str) -> Value {
    json!({"mid":mid,"name":mid,"kind":if mid.starts_with('c')||mid.starts_with('r'){"group"}else{"dm"},"lastActivity":0,"unread":0,"preview":"","muted":false})
}

fn merge_profile(
    rows: &mut BTreeMap<String, Value>,
    profile: &Value,
    group: bool,
) -> Option<String> {
    let mid = mid(profile)?;
    let row = rows.entry(mid.to_owned()).or_insert_with(|| entry(mid));
    if let Some(name) = name(profile) {
        row["name"] = json!(name);
    }
    if let Some(path) = picture(profile) {
        row["picturePath"] = json!(path);
    }
    if group {
        row["kind"] = json!("group");
    } else if sources(profile)
        .iter()
        .any(|s| string(&s["type"]).contains("BOT"))
    {
        row["kind"] = json!("bot");
    }
    // Absent fields in a fallback response must not reset known mute state.
    if let Some(muted) = sources(profile)
        .iter()
        .find_map(|s| s["notificationDisabled"].as_bool())
    {
        row["muted"] = json!(muted);
    }
    Some(mid.to_owned())
}

fn array(value: Value) -> Result<Vec<Value>> {
    value
        .as_array()
        .cloned()
        .context("invalid catalog response: expected array")
}

/// Every RPC is synchronous here: no parallel fallback batches or hidden retries.
/// `previous` contains only the current account's cached lists.
pub fn load(
    method: &str,
    previous: &Value,
    mut call: impl FnMut(&str, Value) -> Result<Value>,
) -> Result<Value> {
    let friends = method == "list_friends";
    let mut rows = BTreeMap::new();
    for key in ["chats", "friends"] {
        for row in previous[key].as_array().into_iter().flatten() {
            if let Some(mid) = mid(row)
                && (!friends || mid.starts_with('u'))
            {
                rows.insert(mid.to_owned(), row.clone());
            }
        }
    }
    let had_cache = !rows.is_empty();
    let mut degraded = false;
    let mut succeeded = false;
    if !friends {
        match call("bridge_boxes", json!({})).and_then(|v| array(v["messageBoxes"].clone())) {
            Ok(boxes) => {
                succeeded = true;
                for box_ in boxes {
                    let id = string(&box_["id"]);
                    if id.is_empty() {
                        continue;
                    }
                    let row = rows.entry(id.clone()).or_insert_with(|| entry(&id));
                    row["lastActivity"] =
                        json!(integer(&box_["lastDeliveredMessageId"]["deliveredTime"]));
                    row["unread"] = json!(integer(&box_["unreadCount"]));
                }
            }
            Err(_) => degraded = true,
        }
    }
    let mut ids: BTreeSet<String> = rows
        .keys()
        .filter(|id| id.starts_with('u'))
        .cloned()
        .collect();
    let mut need_users = false;
    match call("bridge_contact_ids", json!({})).and_then(array) {
        Ok(found) => {
            succeeded = true;
            ids.extend(
                found
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            );
        }
        Err(_) => {
            degraded = true;
            need_users = true;
        }
    }
    let mut missing = BTreeSet::new();
    let ids: Vec<_> = ids.into_iter().collect();
    for batch in ids.chunks(100) {
        match call("bridge_contacts", json!({"mids":batch})).and_then(array) {
            Ok(contacts) => {
                succeeded = true;
                let resolved: BTreeSet<_> = contacts
                    .iter()
                    .filter_map(|c| merge_profile(&mut rows, c, false))
                    .collect();
                missing.extend(batch.iter().filter(|id| !resolved.contains(*id)).cloned());
            }
            Err(_) => {
                degraded = true;
                missing.extend(batch.iter().cloned());
            }
        }
    }
    if need_users || !missing.is_empty() {
        degraded = true;
        match call("bridge_users", json!({})).and_then(array) {
            Ok(users) => {
                succeeded = true;
                for user in users {
                    if let Some(id) = merge_profile(&mut rows, &user, false) {
                        missing.remove(&id);
                    }
                }
            }
            Err(_) => degraded = true,
        }
    }
    for id in missing {
        match call("bridge_contact", json!({"mid":id})) {
            Ok(contact) if mid(&contact) == Some(id.as_str()) => {
                succeeded = true;
                merge_profile(&mut rows, &contact, false);
            }
            _ => {
                degraded = true;
                rows.entry(id.clone()).or_insert_with(|| entry(&id));
            }
        }
    }
    if !friends {
        match call("bridge_groups", json!({})).and_then(array) {
            Ok(groups) => {
                succeeded = true;
                for group in groups {
                    merge_profile(&mut rows, &group, true);
                }
            }
            Err(_) => degraded = true,
        }
    }
    if !succeeded && !had_cache && rows.is_empty() {
        bail!("all catalog sources unavailable");
    }
    let mut rows: Vec<_> = rows
        .into_values()
        .filter(|row| !friends || row["mid"].as_str().is_some_and(|s| s.starts_with('u')))
        .collect();
    rows.sort_by(|a, b| {
        integer(&b["lastActivity"])
            .cmp(&integer(&a["lastActivity"]))
            .then_with(|| string(&a["name"]).cmp(&string(&b["name"])))
    });
    let key = if friends { "friends" } else { "chats" };
    Ok(json!({key:rows,"cached":had_cache&&degraded,"partial":degraded}))
}
