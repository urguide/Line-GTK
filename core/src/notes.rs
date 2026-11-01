//! Normalize LINE Notes without converting identifiers through floating point.
use crate::message::{integer, string};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

fn post(value: &Value, account: &str) -> Result<Value> {
    let row = value.get("post").unwrap_or(value);
    let info = &row["postInfo"];
    let id = string(&info["postId"]);
    if id.is_empty() {
        bail!("notes_invalid_post");
    }
    let writer = &row["writerInfo"];
    let author = writer["writerMid"]
        .as_str()
        .or(writer["mid"].as_str())
        .unwrap_or_default();
    let contents = &row["contents"];
    Ok(json!({
        "id":id,"author":author,
        "authorName":crate::catalog::name(writer).or(writer["writerName"].as_str()).unwrap_or(author),
        "mine": !account.is_empty() && author == account,
        "text":contents["text"].as_str().unwrap_or_default(),
        "createdTime":integer(&info["createdTime"]),"updatedTime":integer(&info["updatedTime"]),
        "media":contents["media"].as_array().cloned().unwrap_or_default(),
        "stickers":contents["stickers"].as_array().cloned().unwrap_or_default(),
        "locations":contents["locations"].as_array().cloned().unwrap_or_default(),
        "likes":integer(&info["likeCount"]),"comments":integer(&info["commentCount"]),
        "liked": info["liked"].as_bool().or(info["like"].as_bool()).unwrap_or(false),
    }))
}

pub fn comments(value: &Value) -> Result<Value> {
    let rows = value["comments"]
        .as_array()
        .or_else(|| value["commentList"].as_array())
        .context("notes_invalid_comments")?;
    let items: Vec<Value> = rows.iter().map(|row| {
        let writer = &row["writerInfo"];
        json!({"id":string(&row["commentId"]),
            "author":writer["writerMid"].as_str().or(writer["mid"].as_str()).or(row["writerMid"].as_str()).unwrap_or_default(),
            "authorName": crate::catalog::name(writer).or(writer["writerName"].as_str()).or(row["displayName"].as_str()).unwrap_or_default(),
            "text":row["commentText"].as_str().unwrap_or_default(),
            "createdTime":integer(&row["createdTime"]),
            "attachments":row["contentsList"].as_array().map_or(0,Vec::len)})
    }).collect();
    Ok(json!({"comments":items,"scrollId":value["scrollId"]}))
}

pub fn normalize(action: &str, value: &Value, account: &str) -> Result<Value> {
    match action {
        "list" => {
            let feeds = value["feeds"]
                .as_array()
                .or_else(|| value["posts"].as_array())
                .context("notes_invalid_list")?;
            let posts: Vec<Value> = feeds
                .iter()
                .map(|v| post(v, account))
                .collect::<Result<_>>()?;
            let cursor = posts
                .last()
                .filter(|p| p["updatedTime"].as_i64().unwrap_or(0) > 0)
                .map(|p| json!({"postId":p["id"],"updatedTime":p["updatedTime"]}));
            Ok(json!({"posts":posts,"cursor":cursor}))
        }
        "get" => {
            let mut result = json!({"post":post(&value["post"],account)?,"comments":[],"commentsError":value["commentsError"]});
            if !value["comments"].is_null() {
                match comments(&value["comments"]) {
                    Ok(c) => {
                        result["comments"] = c["comments"].clone();
                        result["scrollId"] = c["scrollId"].clone();
                    }
                    Err(_) => result["commentsError"] = json!(true),
                }
            }
            Ok(result)
        }
        "comments" => comments(value),
        _ => Ok(json!({"ok":true})),
    }
}

/// Notes carry public writer names; resolve each distinct author through the
/// account's contact API so posts and comments use the same alias as chats.
pub fn resolve_author_names(value: &mut Value, mut contact: impl FnMut(&str) -> Result<Value>) {
    fn walk(
        value: &mut Value,
        names: &mut std::collections::HashMap<String, Option<String>>,
        contact: &mut impl FnMut(&str) -> Result<Value>,
    ) {
        if let Some(author) = value["author"]
            .as_str()
            .filter(|mid| !mid.is_empty())
            .map(str::to_owned)
        {
            let name = names.entry(author.clone()).or_insert_with(|| {
                contact(&author)
                    .ok()
                    .and_then(|profile| crate::catalog::name(&profile).map(str::to_owned))
            });
            if let Some(name) = name {
                value["authorName"] = json!(name);
            }
        }
        if let Some(rows) = value.as_array_mut() {
            for row in rows {
                walk(row, names, contact);
            }
        } else if let Some(fields) = value.as_object_mut() {
            for row in fields.values_mut() {
                if row.is_array() || row.is_object() {
                    walk(row, names, contact);
                }
            }
        }
    }
    walk(value, &mut std::collections::HashMap::new(), &mut contact);
}
