//! Single source of truth for the original retention modes (milliseconds).
pub const MINUTE: i64 = 60_000;
pub const DAY: i64 = 86_400_000;
pub const WEEK: i64 = 7 * DAY;
pub const MONTH: i64 = 30 * DAY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    pub messages: i64,
    pub chats: i64,
    pub contacts: i64,
    pub disk_messages: i64,
    pub disk_chats: i64,
    pub owned_stickers: i64,
    pub animation_miss: Option<u64>,
}
impl CachePolicy {
    pub fn for_mode(mode: &str) -> Self {
        let (disk, owned, chats, messages, contacts, animation) =
            match mode.trim().to_ascii_lowercase().as_str() {
                "day" => (DAY, DAY, 30, 15, 30, Some(DAY as u64)),
                "week" => (WEEK, WEEK, 60, 30, 120, Some(WEEK as u64)),
                "month" => (MONTH, MONTH, 120, 60, 360, Some(MONTH as u64)),
                "forever" => (i64::MAX, WEEK, 360, 120, 360, None),
                _ => (360 * DAY, DAY, 30, 20, 30, Some(WEEK as u64)),
            };
        Self {
            messages: messages * MINUTE,
            chats: chats * MINUTE,
            contacts: contacts * MINUTE,
            disk_messages: disk,
            disk_chats: disk,
            owned_stickers: owned,
            animation_miss: animation,
        }
    }
    pub fn catalog_ttl(self, kind: &str) -> i64 {
        match kind {
            "list_chats" => self.chats,
            "list_friends" => self.contacts,
            "list_stickers" | "owned_stickers" => self.owned_stickers,
            _ => i64::MAX,
        }
    }
}
impl Default for CachePolicy {
    fn default() -> Self {
        Self::for_mode("smart")
    }
}
