//! Composer character ranges and LINE's UTF-16 mention metadata.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub mid: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    /// GTK character offsets (not bytes or UTF-16 units).
    pub start: usize,
    pub end: usize,
    pub mid: String,
    pub label: String,
}

pub fn byte_offset(text: &str, character: usize) -> Option<usize> {
    text.char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(text.len()))
        .nth(character)
}

/// Only an @ at the start or following whitespace opens completion. Email
/// addresses and already selected mentions never become completion queries.
pub fn query(text: &str, cursor: usize, mentions: &[Mention]) -> Option<(usize, String)> {
    if mentions.iter().any(|m| cursor > m.start && cursor <= m.end) {
        return None;
    }
    let prefix = &text[..byte_offset(text, cursor)?];
    let start = prefix.rfind('@')?;
    if prefix[..start]
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace())
    {
        return None;
    }
    let query = &prefix[start + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((prefix[..start].chars().count(), query.to_lowercase()))
}

pub fn matches(member: &Member, query: &str) -> bool {
    member.name.to_lowercase().contains(&query.to_lowercase())
}

/// Editing a selected name invalidates its identity; edits outside it shift it.
pub fn inserted(mentions: &mut Vec<Mention>, position: usize, count: usize) {
    if count == 0 {
        return;
    }
    mentions.retain_mut(|m| {
        if position <= m.start {
            m.start += count;
            m.end += count;
        } else if position < m.end {
            return false;
        }
        true
    });
}

pub fn deleted(mentions: &mut Vec<Mention>, position: usize, count: usize) {
    if count == 0 {
        return;
    }
    let end = position + count;
    mentions.retain_mut(|m| {
        if end <= m.start {
            m.start -= count;
            m.end -= count;
        } else if position < m.end {
            return false;
        }
        true
    });
}

pub fn trimmed(input: &str, mentions: &[Mention]) -> (String, Vec<Mention>) {
    let text = input.trim();
    let leading = input[..input.len() - input.trim_start().len()]
        .chars()
        .count();
    let end = leading + text.chars().count();
    let mentions = mentions
        .iter()
        .filter(|m| m.start >= leading && m.end <= end)
        .cloned()
        .map(|mut m| {
            m.start -= leading;
            m.end -= leading;
            m
        })
        .collect();
    (text.to_owned(), mentions)
}

pub fn compose(input: &str, packs: &Value, mentions: &[Mention]) -> (String, Value) {
    let (text, mut metadata) = crate::expressions::compose(input, packs);
    let mut entries = Vec::new();
    for m in mentions {
        let (Some(start), Some(end)) = (byte_offset(input, m.start), byte_offset(input, m.end))
        else {
            continue;
        };
        if start >= end
            || !m.mid.starts_with('u')
            || input[start..end] != m.label
            || !m.label.starts_with('@')
        {
            continue;
        }
        // Emoji tokens before (or inside) a name change the transmitted length.
        let start = crate::expressions::compose(&input[..start], packs)
            .0
            .encode_utf16()
            .count();
        let end = crate::expressions::compose(&input[..end], packs)
            .0
            .encode_utf16()
            .count();
        entries.push(json!({"S":start.to_string(),"E":end.to_string(),"M":m.mid}));
    }
    if !entries.is_empty() {
        metadata["MENTION"] = json!(json!({"MENTIONEES":entries}).to_string());
    }
    (text, metadata)
}

/// Fetch actual participants, never the entire friend list or recent senders.
pub fn members(chat: &str, mut call: impl FnMut(&str, Value) -> Result<Value>) -> Result<Value> {
    let ids: BTreeSet<String> = if chat.starts_with('u') {
        BTreeSet::from([chat.to_owned()])
    } else if chat.starts_with(['c', 'r']) {
        let raw = call("bridge_chat_members", json!({"chatMid":chat}))?;
        raw["extra"]["groupExtra"]["memberMids"]
            .as_object()
            .context("missing chat membership")?
            .keys()
            .cloned()
            .collect()
    } else {
        bail!("unsupported chat membership");
    };
    let ids: Vec<_> = ids.into_iter().filter(|id| id.starts_with('u')).collect();
    let mut members = Vec::new();
    for batch in ids.chunks(100) {
        let contacts = call("bridge_contacts", json!({"mids":batch}))
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        for mid in batch {
            let profile = contacts
                .iter()
                .find(|c| c["mid"].as_str() == Some(mid))
                .cloned()
                .or_else(|| call("bridge_contact", json!({"mid":mid})).ok());
            let name = profile
                .as_ref()
                .and_then(crate::catalog::name)
                .unwrap_or(mid)
                .to_owned();
            members.push(Member {
                mid: mid.clone(),
                name,
            });
        }
    }
    members.sort_by_key(|m| (m.name.to_lowercase(), m.mid.clone()));
    Ok(json!({"members":members}))
}
