//! Presentation and metadata interpretation belong to the application, not linejs.
use serde_json::{Value, json};

/// A failed message remains a row in the page, without displaying ciphertext
/// or executing embedded Flex actions. Original metadata is retained for retry.
pub fn from_envelope(envelope: &Value, account: &str) -> Value {
    let decoded = envelope.get("decodedMessage").filter(|v| v.is_object());
    let raw = envelope.get("rawMessage").filter(|v| v.is_object());
    if envelope.get("decodedMessage").is_none() && envelope.get("decodeError").is_none() {
        return envelope.clone(); // legacy normalized cache/event
    }
    let mut message = normalize(decoded.or(raw).unwrap_or(&Value::Null), account);
    let failed = decoded.is_none() || envelope["decodeError"].is_string();
    message["decodeError"] = if failed {
        json!("message_decode_failed")
    } else {
        Value::Null
    };
    message["rawMessage"] = envelope["rawMessage"].clone();
    if failed {
        let encrypted = raw.is_some_and(|r| {
            r["chunks"].as_array().is_some_and(|v| !v.is_empty())
                || !r["contentMetadata"]["e2eeVersion"].is_null()
        });
        if encrypted || message["text"].as_str().is_none_or(|s| s.is_empty()) {
            message["text"] = json!("[Unable to decrypt message]");
        }
        message["flex"] = Value::Null;
        message["needsMedia"] = json!(false);
    }
    message
}

pub fn normalize(raw: &Value, account: &str) -> Value {
    let meta = &raw["contentMetadata"];
    let content_type = match raw["contentType"].as_u64() {
        Some(0) => "NONE",
        Some(1) => "IMAGE",
        Some(2) => "VIDEO",
        Some(3) => "AUDIO",
        Some(7) => "STICKER",
        Some(14) => "FILE",
        Some(15) => "LOCATION",
        Some(22) => "FLEX",
        _ => raw["contentType"].as_str().unwrap_or("NONE"),
    };
    let text = raw["text"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| meta["ALT_TEXT"].as_str())
        .or_else(|| meta["FILE_NAME"].as_str())
        .unwrap_or(match content_type {
            "IMAGE" => "[Image]",
            "VIDEO" => "[Video]",
            "AUDIO" => "Voice message",
            "STICKER" => "[Sticker]",
            "FILE" => "[File]",
            _ => "",
        });
    let relation = raw["messageRelationType"] == "REPLY" || raw["messageRelationType"] == 3;
    json!({
        "id": string(&raw["id"]), "text":text, "from":raw["from"].as_str().unwrap_or_default(),
        "to":raw["to"].as_str().unwrap_or_default(),"mine":raw["from"]==account,
        "createdTime":integer(&raw["createdTime"]),"deliveredTime":string(&raw["deliveredTime"]),
        "contentType":content_type,"fileName":meta["FILE_NAME"].as_str().or_else(||meta["FILENAME"].as_str()),
        "imagePath":null,"audioPath":null,"filePath":null,
        "imageUrl":meta["PREVIEW_URL"].as_str().or_else(||meta["DOWNLOAD_URL"].as_str()),
        "stickerId":meta["STKID"],"stickerPackageId":meta["STKPKGID"],"stickerVersion":meta["STKVER"],
        "durationMs":integer(&meta["DURATION"]),
        "relatedMessageId":if relation{raw["relatedMessageId"].as_str()}else{None},
        "flex":if content_type=="FLEX"{flex(meta)}else{Value::Null},
        "needsMedia":matches!(content_type,"IMAGE"|"VIDEO"|"AUDIO"|"STICKER"),
    })
}

pub fn string(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| {
        if value.is_null() {
            String::new()
        } else {
            value.to_string()
        }
    })
}
pub fn integer(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse().ok())
        .unwrap_or(0)
}

fn flex(meta: &Value) -> Value {
    let root: Value = meta["FLEX_JSON"]
        .as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    let mut texts = Vec::new();
    let mut actions = Vec::new();
    fn walk(node: &Value, depth: usize, texts: &mut Vec<String>, actions: &mut Vec<Value>) {
        if depth > 12 {
            return;
        }
        if let Some(text) = node["text"].as_str().filter(|s| !s.is_empty())
            && texts.len() < 12
            && !texts.iter().any(|s| s == text)
        {
            texts.push(text.to_owned());
        }
        if let Some(action) = node.get("action").filter(|a| a.is_object())
            && actions.len() < 24
        {
            let item = json!({"label":action["label"].as_str().or_else(||node["text"].as_str()).unwrap_or("Action"),"kind":action["type"].as_str().unwrap_or("postback"),"data":action["data"],"uri":action["uri"].as_str().or_else(||action["url"].as_str())});
            if !actions.contains(&item) {
                actions.push(item);
            }
        }
        if let Some(items) = node.as_array() {
            for item in items {
                walk(item, depth + 1, texts, actions);
            }
        }
        if let Some(items) = node.as_object() {
            for item in items.values() {
                walk(item, depth + 1, texts, actions);
            }
        }
    }
    walk(&root, 0, &mut texts, &mut actions);
    json!({"altText":meta["ALT_TEXT"].as_str().unwrap_or("Flex message"),"texts":texts,"actions":actions})
}
