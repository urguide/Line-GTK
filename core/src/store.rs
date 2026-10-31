use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const PAGE_SIZE: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    pub message_id: String,
    pub delivered_time: String,
}

impl Cursor {
    pub fn validate(&self) -> Result<()> {
        for value in [&self.message_id, &self.delivered_time] {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                bail!("invalid history cursor");
            }
            value.parse::<u64>().context("history cursor overflow")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub messages: Vec<Value>,
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
    #[serde(default)]
    pub cached: bool,
}

#[derive(Debug, Clone)]
pub struct Gap {
    pub key: String,
    pub before: Cursor,
    pub target_id: String,
}

pub struct Store {
    db: Connection,
    account: String,
    ttl_ms: i64,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn id(row: &Value) -> &str {
    row.get("id").and_then(Value::as_str).unwrap_or_default()
}

pub fn merge(old: Value, incoming: &Value) -> Value {
    let mut merged = old.as_object().cloned().unwrap_or_default();
    if let Some(fields) = incoming.as_object() {
        for (key, value) in fields {
            if value.is_null()
                && matches!(
                    key.as_str(),
                    "imagePath" | "filePath" | "audioPath" | "rawMessage"
                )
            {
                continue;
            }
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS messages (
               account TEXT NOT NULL, chat TEXT NOT NULL, id TEXT NOT NULL,
               created INTEGER NOT NULL, updated INTEGER NOT NULL, body TEXT NOT NULL,
               PRIMARY KEY(account,chat,id));
             CREATE INDEX IF NOT EXISTS messages_time ON messages(account,chat,created,id);
             CREATE TABLE IF NOT EXISTS pages (
               account TEXT NOT NULL, chat TEXT NOT NULL, boundary TEXT NOT NULL,
               updated INTEGER NOT NULL, body TEXT NOT NULL, PRIMARY KEY(account,chat,boundary));
             CREATE TABLE IF NOT EXISTS gaps (
               account TEXT NOT NULL, chat TEXT NOT NULL, key TEXT NOT NULL,
               cursor TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY(account,chat,key));
             CREATE TABLE IF NOT EXISTS heads (
               account TEXT NOT NULL, chat TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(account,chat));
             CREATE TABLE IF NOT EXISTS tombstones (
               account TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(account,id));
             CREATE TABLE IF NOT EXISTS migrations (path TEXT PRIMARY KEY, account TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS catalogs (account TEXT NOT NULL,kind TEXT NOT NULL,updated INTEGER NOT NULL,body TEXT NOT NULL,PRIMARY KEY(account,kind));"
        )?;
        Ok(Self {
            db,
            account: String::new(),
            ttl_ms: 360 * 86_400_000,
        })
    }

    pub fn set_account(&mut self, account: &str) {
        self.account = account.to_owned();
    }

    pub fn catalog(&self, kind: &str, ttl: i64) -> Result<Option<Value>> {
        let body: Option<String> = self
            .db
            .query_row(
                "SELECT body FROM catalogs WHERE account=?1 AND kind=?2 AND updated>=?3",
                params![self.account, kind, now().saturating_sub(ttl)],
                |r| r.get(0),
            )
            .optional()?;
        body.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }

    pub fn save_catalog(&self, kind: &str, body: &Value) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO catalogs VALUES(?1,?2,?3,?4)",
            params![self.account, kind, now(), body.to_string()],
        )?;
        Ok(())
    }

    pub fn invalidate_catalogs(&self) -> Result<()> {
        self.db.execute(
            "UPDATE catalogs SET updated=0 WHERE account=?1 AND kind!='recent_stickers'",
            params![self.account],
        )?;
        Ok(())
    }

    pub fn set_retention(&mut self, mode: &str) {
        self.ttl_ms = match mode {
            "day" => 86_400_000,
            "week" => 7 * 86_400_000,
            "month" => 30 * 86_400_000,
            "forever" => i64::MAX,
            _ => 360 * 86_400_000,
        };
    }

    pub fn decorate_chats(&self, value: &mut Value) -> Result<()> {
        if let Some(rows) = value["chats"].as_array_mut() {
            for row in rows {
                let mid = row["mid"].as_str().unwrap_or_default();
                let latest:Option<String>=self.db.query_row("SELECT body FROM messages WHERE account=?1 AND chat=?2 ORDER BY created DESC,LENGTH(id) DESC,id DESC LIMIT 1",params![self.account,mid],|r|r.get(0)).optional()?;
                if let Some(latest) = latest {
                    let message: Value = serde_json::from_str(&latest)?;
                    row["preview"] = message["text"].clone();
                    if message["createdTime"].as_i64().unwrap_or(0)
                        > row["lastActivity"].as_i64().unwrap_or(0)
                    {
                        row["lastActivity"] = message["createdTime"].clone();
                    }
                }
            }
        }
        Ok(())
    }

    fn cutoff(&self) -> i64 {
        now().saturating_sub(self.ttl_ms)
    }

    pub fn message(&self, chat: &str, message_id: &str) -> Result<Option<Value>> {
        let body: Option<String> = self
            .db
            .query_row(
                "SELECT body FROM messages WHERE account=?1 AND chat=?2 AND id=?3 AND updated>=?4",
                params![self.account, chat, message_id, self.cutoff()],
                |r| r.get(0),
            )
            .optional()?;
        body.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }

    pub fn save_messages(&mut self, chat: &str, messages: &[Value]) -> Result<()> {
        let tx = self.db.savepoint()?;
        for row in messages {
            let message_id = id(row);
            if message_id.is_empty() {
                continue;
            }
            let removed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tombstones WHERE account=?1 AND id=?2)",
                params![self.account, message_id],
                |r| r.get(0),
            )?;
            if removed {
                continue;
            }
            let old: Option<String> = tx
                .query_row(
                    "SELECT body FROM messages WHERE account=?1 AND chat=?2 AND id=?3",
                    params![self.account, chat, message_id],
                    |r| r.get(0),
                )
                .optional()?;
            let merged = merge(
                old.map(|s| serde_json::from_str(&s))
                    .transpose()?
                    .unwrap_or(Value::Null),
                row,
            );
            tx.execute("INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(account,chat,id) DO UPDATE SET created=excluded.created,updated=excluded.updated,body=excluded.body",
                params![self.account, chat, message_id, merged["createdTime"].as_i64().unwrap_or(0), now(), merged.to_string()])?;
        }
        tx.execute(
            "DELETE FROM pages WHERE account=?1 AND updated<?2",
            params![self.account, now().saturating_sub(self.ttl_ms)],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE account=?1 AND updated<?2",
            params![self.account, now().saturating_sub(self.ttl_ms)],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn page(&self, chat: &str, before: Option<&Cursor>, fresh_ms: i64) -> Result<Option<Page>> {
        let boundary = before.map(|c| c.message_id.as_str()).unwrap_or("latest");
        let cutoff = if before.is_some() {
            self.cutoff()
        } else {
            now().saturating_sub(fresh_ms)
        };
        let body: Option<String> = self.db.query_row("SELECT body FROM pages WHERE account=?1 AND chat=?2 AND boundary=?3 AND updated>=?4", params![self.account, chat, boundary, cutoff], |r| r.get(0)).optional()?;
        let Some(body) = body else { return Ok(None) };
        let value: Value = serde_json::from_str(&body)?;
        let mut page = Page {
            messages: vec![],
            next_cursor: serde_json::from_value(value["nextCursor"].clone())?,
            has_more: value["hasMore"].as_bool().unwrap_or(false),
            cached: true,
        };
        for message_id in value["ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if let Some(row) = self.message(chat, message_id)? {
                page.messages.push(row);
            }
        }
        sort_messages(&mut page.messages);
        Ok(Some(page))
    }

    pub fn save_page(&mut self, chat: &str, before: Option<&Cursor>, page: &Page) -> Result<Page> {
        // An outer savepoint makes messages, page boundaries and gap creation atomic.
        self.db.execute_batch("SAVEPOINT save_page")?;
        let result = self.save_page_inner(chat, before, page);
        if result.is_ok() {
            self.db.execute_batch("RELEASE save_page")?;
        } else {
            self.db
                .execute_batch("ROLLBACK TO save_page; RELEASE save_page")?;
        }
        result
    }

    fn save_page_inner(
        &mut self,
        chat: &str,
        before: Option<&Cursor>,
        page: &Page,
    ) -> Result<Page> {
        let target: Option<String> = self
            .db
            .query_row(
                "SELECT id FROM heads WHERE account=?1 AND chat=?2",
                params![self.account, chat],
                |r| r.get(0),
            )
            .optional()?;
        let target = match target {
            Some(t) => Some(t),
            None => self.db.query_row("SELECT id FROM messages WHERE account=?1 AND chat=?2 AND updated>=?3 ORDER BY created,LENGTH(id),id LIMIT 1", params![self.account,chat,self.cutoff()], |r| r.get(0)).optional()?,
        };
        // save_messages uses its own savepoint inside this transaction.
        self.save_messages(chat, &page.messages)?;
        let mut rows = page.messages.clone();
        sort_messages(&mut rows);
        let body = json!({"ids": rows.iter().map(id).collect::<Vec<_>>(), "nextCursor": page.next_cursor, "hasMore": page.has_more});
        self.db.execute(
            "INSERT OR REPLACE INTO pages VALUES(?1,?2,?3,?4,?5)",
            params![
                self.account,
                chat,
                before.map(|c| c.message_id.as_str()).unwrap_or("latest"),
                now(),
                body.to_string()
            ],
        )?;
        if before.is_none()
            && let Some(head) = rows.last()
        {
            if let (Some(target), Some(cursor)) = (target, page.next_cursor.as_ref())
                && page.has_more
                && !rows.iter().any(|m| id(m) == target)
            {
                self.db.execute(
                    "INSERT OR IGNORE INTO gaps VALUES(?1,?2,?3,?4,?5)",
                    params![
                        self.account,
                        chat,
                        format!("{}:{target}", id(head)),
                        serde_json::to_string(cursor)?,
                        target
                    ],
                )?;
            }
            self.db.execute(
                "INSERT OR REPLACE INTO heads VALUES(?1,?2,?3)",
                params![self.account, chat, id(head)],
            )?;
        }
        self.page(chat, before, i64::MAX)?
            .context("saved page missing")
    }

    pub fn invalidate(&self, chat: &str) -> Result<()> {
        self.db.execute(
            "DELETE FROM pages WHERE account=?1 AND chat=?2 AND boundary='latest'",
            params![self.account, chat],
        )?;
        Ok(())
    }

    pub fn gap(&self, chat: &str) -> Result<Option<Gap>> {
        let row: Option<(String,String,String)> = self.db.query_row("SELECT key,cursor,target FROM gaps WHERE account=?1 AND chat=?2 ORDER BY rowid LIMIT 1",params![self.account,chat],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        row.map(|(key, cursor, target_id)| {
            Ok(Gap {
                key,
                before: serde_json::from_str(&cursor)?,
                target_id,
            })
        })
        .transpose()
    }

    pub fn advance(&self, chat: &str, gap: &Gap, next: Option<&Cursor>) -> Result<()> {
        if let Some(next) = next {
            self.db.execute(
                "UPDATE gaps SET cursor=?1 WHERE account=?2 AND chat=?3 AND key=?4",
                params![serde_json::to_string(next)?, self.account, chat, gap.key],
            )?;
        } else {
            self.db.execute(
                "DELETE FROM gaps WHERE account=?1 AND chat=?2 AND key=?3",
                params![self.account, chat, gap.key],
            )?;
        }
        Ok(())
    }

    pub fn unsend(&mut self, chat: &str, message_id: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO tombstones VALUES(?1,?2)",
            params![self.account, message_id],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE account=?1 AND chat=?2 AND id=?3",
            params![self.account, chat, message_id],
        )?;
        tx.commit()?;
        self.invalidate(chat)
    }

    pub fn clear(&mut self) -> Result<()> {
        let tx = self.db.transaction()?;
        for table in ["messages", "pages", "gaps", "heads"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE account=?1"),
                params![self.account],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Legacy files are never deleted. Claim each file once, after account login.
    pub fn import_legacy(&mut self, directory: &Path) -> Result<()> {
        if let Some(data) = directory.parent().and_then(Path::parent) {
            self.import_legacy_stickers(&data.join("stickers-index.json"))?;
        }
        if !directory.exists() {
            return Ok(());
        }
        // Import withdrawal markers before any legacy rows can be restored.
        if let Some(parent) = directory.parent() {
            let removed = parent.join("unsent.json");
            if removed.is_file() {
                let ids: Vec<String> = serde_json::from_slice(&std::fs::read(removed)?)?;
                for message_id in ids {
                    self.db.execute(
                        "INSERT OR IGNORE INTO tombstones VALUES(?1,?2)",
                        params![self.account, message_id],
                    )?;
                    self.db.execute(
                        "DELETE FROM messages WHERE account=?1 AND id=?2",
                        params![self.account, message_id],
                    )?;
                }
            }
        }
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(chat) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let key = path.to_string_lossy();
            let imported: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM migrations WHERE path=?1)",
                params![key],
                |r| r.get(0),
            )?;
            if imported {
                continue;
            }
            let file: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            if file["at"].as_i64().unwrap_or(0) < self.cutoff() {
                continue;
            }
            self.save_messages(
                chat,
                file["messages"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )?;
            for (boundary, page) in file["pages"].as_object().into_iter().flatten() {
                self.db.execute(
                    "INSERT OR IGNORE INTO pages VALUES(?1,?2,?3,?4,?5)",
                    params![self.account, chat, boundary, now(), page.to_string()],
                )?;
            }
            for (key, gap) in file["gaps"].as_object().into_iter().flatten() {
                self.db.execute(
                    "INSERT OR IGNORE INTO gaps VALUES(?1,?2,?3,?4,?5)",
                    params![
                        self.account,
                        chat,
                        key,
                        gap["before"].to_string(),
                        gap["targetId"].as_str().unwrap_or_default()
                    ],
                )?;
            }
            if let Some(head) = file["syncHead"].as_str() {
                self.db.execute(
                    "INSERT OR IGNORE INTO heads VALUES(?1,?2,?3)",
                    params![self.account, chat, head],
                )?;
            }
            self.db.execute(
                "INSERT OR IGNORE INTO migrations VALUES(?1,?2)",
                params![key, self.account],
            )?;
        }
        Ok(())
    }

    /// Catalog and migration claim commit together, including installations with
    /// no history directory. Existing SQLite recents take precedence on duplicates.
    pub fn import_legacy_stickers(&mut self, path: &Path) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let key = path.to_string_lossy();
        let tx = self.db.transaction()?;
        let claimed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM migrations WHERE path=?1)",
            params![key],
            |r| r.get(0),
        )?;
        if claimed {
            return Ok(());
        }
        let legacy: Vec<Value> = serde_json::from_slice(&std::fs::read(path)?)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT body FROM catalogs WHERE account=?1 AND kind='recent_stickers'",
                params![self.account],
                |r| r.get(0),
            )
            .optional()?;
        let mut rows: Vec<Value> = existing
            .map(|v| serde_json::from_str(&v))
            .transpose()?
            .unwrap_or_default();
        rows.extend(legacy);
        let mut seen = std::collections::HashSet::new();
        rows.retain_mut(|row| {
            let sticker = crate::message::string(&row["stickerId"]);
            let package = crate::message::string(&row["packageId"]);
            if sticker.is_empty()
                || package.is_empty()
                || !sticker.bytes().all(|c| c.is_ascii_digit())
                || !package.bytes().all(|c| c.is_ascii_digit())
                || !seen.insert((package.clone(), sticker.clone()))
            {
                return false;
            }
            row["stickerId"] = json!(sticker);
            row["packageId"] = json!(package);
            if !row["version"].is_null() {
                row["version"] = json!(crate::message::string(&row["version"]));
            }
            true
        });
        rows.truncate(200);
        tx.execute(
            "INSERT OR REPLACE INTO catalogs VALUES(?1,'recent_stickers',?2,?3)",
            params![self.account, now(), serde_json::to_string(&rows)?],
        )?;
        tx.execute(
            "INSERT INTO migrations VALUES(?1,?2)",
            params![key, self.account],
        )?;
        tx.commit()?;
        Ok(())
    }
}

pub fn sort_messages(rows: &mut [Value]) {
    rows.sort_by(|a, b| {
        a["createdTime"]
            .as_i64()
            .cmp(&b["createdTime"].as_i64())
            .then_with(|| id(a).len().cmp(&id(b).len()))
            .then_with(|| id(a).cmp(id(b)))
    });
}
