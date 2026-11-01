//! Owned sticker/emoji libraries and LINE's UTF-16 inline emoji metadata.
use crate::message::string;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashSet;

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub fn owned(mut page: impl FnMut(usize) -> Result<Value>) -> Result<Value> {
    let mut products = Vec::new();
    let mut seen = HashSet::new();
    let mut offset = 0;
    loop {
        let result = page(offset)?;
        let rows = result
            .as_array()
            .or_else(|| result["productList"].as_array())
            .or_else(|| result["1"].as_array())
            .context("invalid owned products response")?;
        if rows.is_empty() {
            break;
        }
        let before = products.len();
        for row in rows {
            let id = string(row.get("id").unwrap_or(&row["1"]));
            if !valid_id(&id) {
                bail!("invalid owned product ID");
            }
            if seen.insert(id.clone()) {
                products.push(json!({"id":id,"name":row.get("name").unwrap_or(&row["11"]),
                    "latestVersion":row.get("latestVersion").or_else(||row.get("version")).unwrap_or(&row["21"])}));
            }
        }
        if before == products.len() {
            bail!("owned products pagination did not advance");
        }
        offset += rows.len();
        let total = result
            .get("totalSize")
            .or_else(|| result.get("3"))
            .map(crate::message::integer);
        if total.is_some_and(|n| n > 0 && offset as i64 >= n) {
            break;
        }
    }
    Ok(json!({"products":products}))
}

pub fn emoji_pack(meta: &Value, product: &Value) -> Result<Value> {
    let package = string(&product["id"]);
    if !valid_id(&package) {
        bail!("invalid emoji package");
    }
    let order = meta["orders"]
        .as_array()
        .context("invalid emoji metadata")?;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for item in order {
        let id = string(item);
        if !valid_id(&id) {
            bail!("invalid emoji ID");
        }
        if seen.insert(id.clone()) {
            entries.push(json!({"stickerId":id,"packageId":package,"version":string(&product["latestVersion"]),
                "altText":meta["altTexts"][&id],"imagePath":null}));
        }
    }
    if entries.is_empty() {
        bail!("empty emoji package");
    }
    Ok(
        json!({"id":package,"name":product["name"],"version":string(&product["latestVersion"]),"stickers":entries}),
    )
}

/// Explicit editable tokens survive draft restoration, cursor edits and copy/paste.
/// Only tokens resolved against this account's downloaded library become emoji.
pub fn compose(input: &str, packs: &Value) -> (String, Value) {
    let mut text = String::new();
    let mut resources = Vec::new();
    let mut ownership = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find("[emoji:") {
        text.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(']') else {
            break;
        };
        let token = &rest[7..end];
        let entry = token.split_once(':').and_then(|(package, id)| {
            packs["packs"]
                .as_array()?
                .iter()
                .find(|p| p["id"] == package)?["stickers"]
                .as_array()?
                .iter()
                .find(|s| s["stickerId"] == id)
        });
        if let Some(entry) = entry {
            let alt = entry["altText"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("emoji");
            let replacement = format!("({alt})");
            let start = text.encode_utf16().count();
            text.push_str(&replacement);
            let package = string(&entry["packageId"]);
            resources.push(json!({"S":start,"E":text.encode_utf16().count(),"productId":package,
                "sticonId":entry["stickerId"],"version":string(&entry["version"]).parse::<u64>().unwrap_or(1),"resourceType":"STATIC"}));
            if !ownership.contains(&package) {
                ownership.push(package);
            }
        } else {
            text.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    text.push_str(rest);
    let metadata = if resources.is_empty() {
        json!({})
    } else {
        json!({"REPLACE":json!({"sticon":{"resources":resources}}).to_string(),"STICON_OWNERSHIP":json!(ownership).to_string()})
    };
    (text, metadata)
}

pub fn resources(meta: &Value) -> Value {
    meta["REPLACE"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v["sticon"]["resources"].as_array().cloned())
        .map(Value::Array)
        .unwrap_or(json!([]))
}

/// Translate wire UTF-16 positions to GTK character offsets, rejecting split surrogates.
pub fn char_offset(text: &str, offset: u64) -> Option<i32> {
    let mut units = 0;
    for (index, c) in text.chars().enumerate() {
        if units == offset {
            return Some(index as i32);
        }
        units += c.len_utf16() as u64;
    }
    (units == offset).then(|| text.chars().count() as i32)
}
