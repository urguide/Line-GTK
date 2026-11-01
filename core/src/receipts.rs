//! Deduplicated reader ranges and server count snapshots. Counts are never added
//! to snapshots: a server count can already include any of the observed readers.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Receipts {
    pub readers: BTreeMap<String, Vec<ReadRange>>,
    pub counts: BTreeMap<String, u64>,
}

pub fn message_id(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

impl Receipts {
    pub fn observe(&mut self, rows: &[Value]) -> bool {
        let mut changed = false;
        for row in rows.iter().filter(|r| r["mine"] == true) {
            let Some(id) = row["id"].as_str().filter(|id| message_id(id).is_some()) else {
                continue;
            };
            let old = self.counts.get(id).copied();
            let count = old.unwrap_or(0).max(row["readCount"].as_u64().unwrap_or(0));
            // Track outgoing messages even when their count is not yet known.
            if old != Some(count) {
                self.counts.insert(id.into(), count);
                changed = true;
            }
        }
        self.recount() || changed
    }

    pub fn peer_read(&mut self, reader: &str, id: &str, account: &str, direct: bool) -> bool {
        if reader.is_empty() || reader == account {
            return false;
        }
        let Some(end) = message_id(id).filter(|id| *id > 0) else {
            return false;
        };
        let ranges = self.readers.entry(reader.into()).or_default();
        if let Some(last) = ranges.iter_mut().max_by_key(|r| r.end) {
            if last.end >= end {
                return false;
            }
            last.end = end;
        } else {
            // Unknown group readers may have joined recently; the API supplies
            // their lower boundary. Do not guess they read the entire history.
            ranges.push(ReadRange {
                start: if direct { 0 } else { end },
                end,
            });
        }
        self.recount();
        true
    }

    pub fn merge_ranges(&mut self, value: &Value, account: &str) -> bool {
        let Some(readers) = value.as_object() else {
            return false;
        };
        let mut changed = false;
        for (reader, entries) in readers {
            if reader == account || reader.is_empty() {
                continue;
            }
            let Some(entries) = entries.as_array() else {
                continue;
            };
            let mut incoming = Vec::new();
            for entry in entries {
                let start = entry["startMessageId"].as_str().and_then(message_id);
                let end = entry["endMessageId"].as_str().and_then(message_id);
                if let (Some(start), Some(end)) = (start, end)
                    && start <= end
                    && end > 0
                {
                    incoming.push(ReadRange { start, end });
                }
            }
            if incoming.is_empty() {
                continue;
            }
            let old = self.readers.get(reader).cloned().unwrap_or_default();
            // Preserve live progress newer than the fetched snapshot. Use API
            // boundaries instead of treating every historical message as read.
            let latest = incoming.iter().map(|r| r.end).max().unwrap();
            for previous in old.iter().filter(|r| r.end > latest) {
                incoming.push(ReadRange {
                    start: previous.start.max(latest.saturating_add(1)),
                    end: previous.end,
                });
            }
            incoming.sort_by_key(|r| r.start);
            let mut merged: Vec<ReadRange> = Vec::new();
            for range in incoming {
                if let Some(last) = merged.last_mut()
                    && range.start <= last.end.saturating_add(1)
                {
                    last.end = last.end.max(range.end);
                } else {
                    merged.push(range);
                }
            }
            if merged != old {
                self.readers.insert(reader.clone(), merged);
                changed = true;
            }
        }
        self.recount() || changed
    }

    fn recount(&mut self) -> bool {
        let mut changed = false;
        for (id, count) in &mut self.counts {
            let Some(id) = message_id(id) else { continue };
            let observed = self
                .readers
                .values()
                .filter(|ranges| ranges.iter().any(|r| r.start <= id && id <= r.end))
                .count() as u64;
            if observed > *count {
                *count = observed;
                changed = true;
            }
        }
        changed
    }
}
