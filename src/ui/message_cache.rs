use super::*;
use std::time::{Duration, Instant};

const MAX_CHATS: usize = 32;
const MAX_MESSAGES: usize = 100;
const FRESH_FOR: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(super) struct Page {
    pub messages: Vec<MessageInfo>,
    pub cursor: Option<serde_json::Value>,
    pub has_more: bool,
    fetched: Instant,
}

impl Page {
    pub fn fresh(&self) -> bool {
        self.fetched.elapsed() < FRESH_FOR
    }
}

/// Account/session-local latest pages. Never stores GTK widgets or marks reads.
#[derive(Default)]
pub(super) struct MessageCache {
    pages: HashMap<String, Page>,
    recent: VecDeque<String>,
    queued: VecDeque<String>,
    scheduled: bool,
}

impl MessageCache {
    pub fn get(&mut self, mid: &str) -> Option<Page> {
        let page = self.pages.get(mid)?.clone();
        self.touch(mid);
        Some(page)
    }

    fn touch(&mut self, mid: &str) {
        self.recent.retain(|id| id != mid);
        self.recent.push_back(mid.into());
        while self.recent.len() > MAX_CHATS {
            if let Some(old) = self.recent.pop_front() {
                self.pages.remove(&old);
            }
        }
    }

    pub fn remove(&mut self, mid: &str) {
        self.pages.remove(mid);
        self.recent.retain(|id| id != mid);
        self.queued.retain(|id| id != mid);
    }

    fn store(&mut self, mid: &str, mut page: Page) -> Page {
        // Preserve live messages received after the snapshot was requested.
        let newest = page
            .messages
            .iter()
            .map(|m| m.created_time)
            .max()
            .unwrap_or(0);
        if let Some(old) = self.pages.get(mid) {
            for message in &old.messages {
                if let Some(incoming) = page.messages.iter_mut().find(|m| m.id == message.id) {
                    for (slot, saved) in [
                        (&mut incoming.image_path, &message.image_path),
                        (&mut incoming.audio_path, &message.audio_path),
                        (&mut incoming.file_path, &message.file_path),
                    ] {
                        if slot.is_none() {
                            *slot = saved.clone();
                        }
                    }
                } else if message.created_time >= newest {
                    page.messages.push(message.clone());
                }
            }
        }
        page.messages.sort_by_key(|m| m.created_time);
        if page.messages.len() <= MAX_MESSAGES {
            self.pages.insert(mid.into(), page.clone());
            self.touch(mid);
        } else {
            self.remove(mid);
        }
        page
    }

    pub fn live(&mut self, mid: &str, message: &MessageInfo) {
        let Some(page) = self.pages.get_mut(mid) else {
            return;
        };
        if let Some(old) = page.messages.iter_mut().find(|m| m.id == message.id) {
            *old = message.clone();
        } else {
            page.messages.push(message.clone());
            page.messages.sort_by_key(|m| m.created_time);
        }
        // Evict rather than truncate: the saved pagination cursor must never skip messages.
        if page.messages.len() > MAX_MESSAGES {
            self.remove(mid);
        }
    }

    pub fn unsend(&mut self, mid: &str, id: &str) {
        if let Some(page) = self.pages.get_mut(mid) {
            page.messages.retain(|m| m.id != id);
        }
    }

    pub fn media_ready(
        &mut self,
        mid: &str,
        id: &str,
        image: &str,
        audio: Option<&str>,
        file: Option<&str>,
    ) {
        if let Some(message) = self
            .pages
            .get_mut(mid)
            .and_then(|p| p.messages.iter_mut().find(|m| m.id == id))
        {
            for (slot, path) in [
                (&mut message.image_path, Some(image)),
                (&mut message.audio_path, audio),
                (&mut message.file_path, file),
            ] {
                if let Some(path) =
                    path.filter(|p| !p.is_empty() && !crate::memory_media::is_ram_path(p))
                {
                    *slot = Some(path.into());
                }
            }
        }
    }
}

fn eligible(state: &AppState, mid: &str) -> bool {
    *state.session_ready.borrow() && !chat_is_hidden(state, mid) && !chat_is_muted(state, mid)
}

pub(super) fn request(state: &AppState, mid: &str) -> Result<()> {
    state
        .message_cache
        .borrow_mut()
        .queued
        .retain(|id| id != mid);
    if state
        .pending
        .borrow()
        .values()
        .any(|p| matches!(p, Pending::LatestMessages { chat_mid, .. } if chat_mid == mid))
    {
        return Ok(());
    }
    let id = state.sidecar.fetch_messages(mid, 20)?;
    state.pending.borrow_mut().insert(
        id,
        Pending::LatestMessages {
            chat_mid: mid.into(),
            account: state.self_mid.borrow().clone(),
            live: Vec::new(),
        },
    );
    Ok(())
}

pub(super) fn prefetch(state: &AppState, mid: &str) {
    if !eligible(state, mid) {
        state.message_cache.borrow_mut().remove(mid);
        return;
    }
    {
        let mut cache = state.message_cache.borrow_mut();
        if !cache.queued.iter().any(|id| id == mid) {
            cache.queued.push_back(mid.into());
        }
        while cache.queued.len() > MAX_CHATS {
            cache.queued.pop_front();
        }
    }
    schedule(state);
}

pub(super) fn live(state: &AppState, mid: &str, message: &MessageInfo) {
    state.message_cache.borrow_mut().live(mid, message);
    for pending in state.pending.borrow_mut().values_mut() {
        if let Pending::LatestMessages { chat_mid, live, .. } = pending
            && chat_mid == mid
        {
            live.retain(|m| m.id != message.id);
            live.push(message.clone());
        }
    }
}

fn schedule(state: &AppState) {
    {
        let mut cache = state.message_cache.borrow_mut();
        if cache.scheduled || cache.queued.is_empty() {
            return;
        }
        cache.scheduled = true;
    }
    let s = state.clone();
    glib::timeout_add_local_once(Duration::from_millis(150), move || {
        s.message_cache.borrow_mut().scheduled = false;
        pump(&s);
    });
}

fn pump(state: &AppState) {
    // Keep background work bounded so foreground requests can reach the worker quickly.
    if state
        .pending
        .borrow()
        .values()
        .any(|p| matches!(p, Pending::LatestMessages { .. }))
    {
        return;
    }
    loop {
        let mid = state.message_cache.borrow_mut().queued.pop_front();
        let Some(mid) = mid else { return };
        if eligible(state, &mid) {
            if let Err(error) = request(state, &mid) {
                tracing::debug!(%error, "message prefetch could not start");
                continue;
            }
            return;
        }
    }
}

pub(super) fn response(
    state: &AppState,
    pending: &Pending,
    ok: bool,
    result: &serde_json::Value,
    error: Option<&str>,
) {
    let Pending::LatestMessages {
        chat_mid: mid,
        account,
        live,
    } = pending
    else {
        return;
    };
    if account.as_deref() != state.self_mid.borrow().as_deref() || !*state.session_ready.borrow() {
        return;
    }
    let viewing = state.current_chat.borrow().as_deref() == Some(mid.as_str());
    if !viewing && !eligible(state, mid) {
        state.message_cache.borrow_mut().remove(mid);
        schedule(state);
        return;
    }
    let messages = ok
        .then(|| serde_json::from_value::<Vec<MessageInfo>>(result["messages"].clone()).ok())
        .flatten();
    if let Some(mut messages) = messages {
        for message in live {
            messages.retain(|m| m.id != message.id);
            messages.push(message.clone());
        }
        messages.retain(|m| !state.unsent_msg_ids.borrow().contains(&m.id));
        let page = state.message_cache.borrow_mut().store(
            mid,
            Page {
                messages,
                cursor: result.get("nextCursor").filter(|v| !v.is_null()).cloned(),
                has_more: result["hasMore"].as_bool().unwrap_or(false),
                fetched: Instant::now(),
            },
        );
        if viewing {
            // Preserve the older-page cursor and scroll position during background refreshes.
            let loading_older = state.pending.borrow().values().any(|p| {
                matches!(p,
                Pending::FetchMessages { chat_mid, generation, older: true }
                    if chat_mid == mid && *generation == *state.history_generation.borrow())
            });
            let initial = *state.history_loading.borrow() && !loading_older;
            if initial {
                *state.history_loading.borrow_mut() = false;
                *state.history_cursor.borrow_mut() = page.cursor.clone();
                *state.history_has_more.borrow_mut() = page.has_more;
            }
            if initial || state.message_index.borrow().is_empty() {
                let mut messages = page.messages;
                for message in state.message_index.borrow().values() {
                    if !messages.iter().any(|m| m.id == message.id) {
                        messages.push(message.clone());
                    }
                }
                apply_messages(state, messages);
            } else {
                let stuck = *state.stick_bottom.borrow();
                set_msg_state(state, "list", None);
                let oldest = state
                    .message_index
                    .borrow()
                    .values()
                    .map(|m| m.created_time)
                    .min()
                    .unwrap_or(0);
                let older: Vec<_> = page
                    .messages
                    .iter()
                    .filter(|m| m.created_time < oldest)
                    .cloned()
                    .collect();
                prepend_messages(state, older);
                for message in &page.messages {
                    append_message(state, message, false);
                }
                if stuck {
                    pin_messages_to_latest(state);
                }
                if stuck && state.window.is_active() {
                    mark_chat_read(state, mid);
                    clear_unread_badge(state, mid);
                }
            }
        }
    } else if viewing
        && *state.history_loading.borrow()
        && !state.pending.borrow().values().any(|p| {
            matches!(p,
            Pending::FetchMessages { chat_mid, generation, older: true }
                if chat_mid == mid && *generation == *state.history_generation.borrow())
        })
    {
        *state.history_loading.borrow_mut() = false;
        if state.message_index.borrow().is_empty() {
            set_msg_state(
                state,
                "empty",
                Some(error.unwrap_or("Failed to load messages")),
            );
        }
    }
    schedule(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: usize) -> MessageInfo {
        serde_json::from_value(serde_json::json!({"id": id.to_string(), "createdTime": id}))
            .unwrap()
    }

    fn page(ids: std::ops::Range<usize>) -> Page {
        Page {
            messages: ids.map(message).collect(),
            cursor: Some(serde_json::json!({"messageId":"1","deliveredTime":"1"})),
            has_more: true,
            fetched: Instant::now(),
        }
    }

    #[test]
    fn reopening_keeps_page_cursor_and_freshness_even_for_empty_chats() {
        let mut cache = MessageCache::default();
        let original = page(1..21);
        cache.store("a", original.clone());
        cache.store("b", page(0..0));
        let restored = cache.get("a").unwrap();
        assert_eq!(restored.messages.len(), 20);
        assert_eq!(restored.cursor, original.cursor);
        assert!(restored.has_more && restored.fresh());
        assert!(cache.get("b").unwrap().messages.is_empty());
        cache.pages.get_mut("a").unwrap().fetched = Instant::now() - FRESH_FOR;
        assert!(!cache.get("a").unwrap().fresh());
    }

    #[test]
    fn live_races_and_unsends_do_not_lose_messages_or_restore_deleted_ones() {
        let mut cache = MessageCache::default();
        cache.store("a", page(1..21));
        cache.live("a", &message(21));
        cache.live("a", &message(21));
        cache.store("a", page(1..21));
        let restored = cache.get("a").unwrap();
        assert_eq!(restored.messages.len(), 21);
        assert_eq!(restored.messages.last().unwrap().id, "21");
        cache.unsend("a", "21");
        assert_eq!(cache.get("a").unwrap().messages.len(), 20);
    }

    #[test]
    fn cache_is_lru_bounded_and_never_truncates_across_a_history_cursor() {
        let mut cache = MessageCache::default();
        for i in 0..MAX_CHATS {
            cache.store(&i.to_string(), page(1..21));
        }
        cache.get("0");
        cache.store("new", page(1..21));
        assert!(cache.get("1").is_none());
        assert!(cache.get("0").is_some());
        cache.store("busy", page(0..MAX_MESSAGES));
        cache.live("busy", &message(MAX_MESSAGES));
        assert!(cache.get("busy").is_none());
        cache.queued.push_back("new".into());
        cache.remove("new");
        assert!(cache.get("new").is_none());
        assert!(cache.queued.is_empty());
    }

    #[test]
    fn hydrated_audio_and_files_survive_page_refreshes() {
        let mut cache = MessageCache::default();
        cache.store("a", page(1..3));
        cache.media_ready(
            "a",
            "1",
            "thumb.png",
            Some("voice.m4a"),
            Some("original.mp4"),
        );
        cache.store("a", page(1..3));
        let page = cache.get("a").unwrap();
        let message = &page.messages[0];
        assert_eq!(message.image_path.as_deref(), Some("thumb.png"));
        assert_eq!(message.audio_path.as_deref(), Some("voice.m4a"));
        assert_eq!(message.file_path.as_deref(), Some("original.mp4"));
    }
}
