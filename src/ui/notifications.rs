use super::{
    AppState, PendingNotif, chat_is_hidden, chat_is_muted, chat_memory_media,
    notifications_allowed, preview_body_ui,
};
use crate::desktop_notify::ChatNotification;
use crate::protocol::MessageInfo;
use gtk::gio;
use gtk::prelude::*;
use std::path::{Path, PathBuf};

pub(super) fn notify_incoming(state: &AppState, msg: &MessageInfo, peer_mid: &str) {
    if !notifications_allowed(state) {
        return;
    }
    let directly_mentioned = state.self_mid.borrow().as_deref().is_some_and(|mid| {
        !mid.is_empty() && msg.mentioned_mids.iter().any(|mentioned| mentioned == mid)
    });
    if !chat_alert_allowed(state, peer_mid, directly_mentioned) {
        return;
    }
    let (name, avatar_path) = state
        .chats
        .borrow()
        .iter()
        .find(|c| c.mid == peer_mid)
        .map(|c| (c.name.clone(), c.avatar_path.clone()))
        .unwrap_or_else(|| (peer_mid.to_string(), None));
    let body = preview_body_ui(msg);
    let image_path = msg
        .image_path
        .as_deref()
        .filter(|_| !chat_memory_media(state, peer_mid))
        .filter(|p| !p.is_empty() && Path::new(p).exists())
        .map(str::to_string);

    if !msg.id.is_empty() {
        let mut pending = state.notif_pending.borrow_mut();
        if pending.len() > 64 {
            pending.clear();
        }
        pending.insert(
            msg.id.clone(),
            PendingNotif {
                chat_mid: peer_mid.to_string(),
                title: name.clone(),
                body: body.clone(),
                avatar_path: avatar_path.clone(),
                directly_mentioned,
            },
        );
    }

    send_desktop_notification(
        state,
        ChatNotification {
            title: &name,
            body: &body,
            avatar_path: avatar_path.as_deref(),
            image_path: image_path.as_deref(),
            chat_mid: peer_mid,
            message_id: &msg.id,
            suppress_sound: false,
        },
    );

    if state.config.borrow().notification_sound {
        play_notification_sound(state);
    }
}

pub(super) fn refresh_notification_media(state: &AppState, message_id: &str, image_path: &str) {
    if !notifications_allowed(state) {
        return;
    }
    let Some(meta) = state.notif_pending.borrow().get(message_id).cloned() else {
        return;
    };
    if !chat_alert_allowed(state, &meta.chat_mid, meta.directly_mentioned) {
        return;
    }
    let viewing = state.current_chat.borrow().as_deref() == Some(meta.chat_mid.as_str());
    if viewing && state.window.is_active() {
        state.notif_pending.borrow_mut().remove(message_id);
        return;
    }

    send_desktop_notification(
        state,
        ChatNotification {
            title: &meta.title,
            body: &meta.body,
            avatar_path: meta.avatar_path.as_deref(),
            image_path: Some(image_path),
            chat_mid: &meta.chat_mid,
            message_id,
            suppress_sound: true,
        },
    );
}

fn chat_alert_allowed(state: &AppState, mid: &str, directly_mentioned: bool) -> bool {
    allows_chat_alert(
        chat_is_hidden(state, mid),
        chat_is_muted(state, mid),
        directly_mentioned,
    )
}

fn allows_chat_alert(hidden: bool, muted: bool, directly_mentioned: bool) -> bool {
    directly_mentioned || !(hidden || muted)
}

fn send_desktop_notification(state: &AppState, request: ChatNotification<'_>) {
    match crate::desktop_notify::show_chat_notification(&request, state.tray_tx.clone()) {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(%error, "notify-rust failed; falling back to gio::Notification");
            let notification = gio::Notification::new(request.title);
            notification.set_body(Some(request.body));
            notification.set_priority(gio::NotificationPriority::Normal);
            notification.set_default_action_and_target_value(
                "app.open-chat",
                Some(&request.chat_mid.to_variant()),
            );
            if let Some(path) = request.avatar_path.filter(|p| Path::new(p).exists()) {
                notification.set_icon(&gio::FileIcon::new(&gio::File::for_path(path)));
            } else if let Some(path) = request.image_path.filter(|p| Path::new(p).exists()) {
                notification.set_icon(&gio::FileIcon::new(&gio::File::for_path(path)));
            } else {
                notification.set_icon(&gio::ThemedIcon::new("line-rust"));
            }
            let id = if request.message_id.is_empty() {
                format!("line-rust-{}", request.chat_mid)
            } else {
                format!("line-rust-{}", request.message_id)
            };
            state.app.send_notification(Some(&id), &notification);
        }
    }
}

fn play_notification_sound(state: &AppState) {
    let volume = state
        .config
        .borrow()
        .notification_sound_volume
        .clamp(0.0, 2.0);
    let _ = play_system_event_sound("message-new-instant", volume)
        || play_system_event_sound("message", volume)
        || play_system_event_sound("bell", volume);
}

fn play_system_event_sound(event: &str, volume: f64) -> bool {
    if std::process::Command::new("canberra-gtk-play")
        .args(["-i", event])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
    {
        return true;
    }
    if std::process::Command::new("canberra-gtk-play")
        .args(["--id", event])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
    {
        return true;
    }
    let Some(path) = find_xdg_theme_sound(event) else {
        return false;
    };
    play_system_sound_file(&path, volume)
}

fn xdg_data_dirs() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        if !data_home.is_empty() {
            paths.push(PathBuf::from(data_home));
        }
    } else if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".local/share"));
    }
    if let Ok(data_dirs) = std::env::var("XDG_DATA_DIRS") {
        paths.extend(
            data_dirs
                .split(':')
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        );
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("/usr/local/share"));
        paths.push(PathBuf::from("/usr/share"));
    }
    paths
}

fn find_xdg_theme_sound(event: &str) -> Option<PathBuf> {
    const THEMES: &[&str] = &[
        "freedesktop",
        "gnome",
        "oxygen",
        "Yaru",
        "deepin",
        "ubuntu",
        "elementary",
    ];
    const EXTENSIONS: &[&str] = &["oga", "ogg", "wav", "flac", "mp3"];
    for root in xdg_data_dirs() {
        let sounds = root.join("sounds");
        for theme in THEMES {
            for extension in EXTENSIONS {
                for subdirectory in ["stereo", ""] {
                    let path = if subdirectory.is_empty() {
                        sounds.join(theme).join(format!("{event}.{extension}"))
                    } else {
                        sounds
                            .join(theme)
                            .join(subdirectory)
                            .join(format!("{event}.{extension}"))
                    };
                    if path.is_file() {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

fn play_system_sound_file(path: &Path, volume: f64) -> bool {
    let path = path.to_string_lossy().to_string();
    let paplay_volume = ((volume * 65536.0).round() as i32).clamp(0, 130_000);
    if std::process::Command::new("paplay")
        .args(["--volume", &paplay_volume.to_string(), &path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
    {
        return true;
    }
    std::process::Command::new("pw-play")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_and_muted_chats_require_a_direct_mention() {
        for (hidden, muted) in [(false, false), (true, false), (false, true), (true, true)] {
            for (metadata, expected_mention) in [
                (serde_json::json!({}), false),
                (
                    serde_json::json!({"MENTION":{"MENTIONEES":[{"A":"1"}]}}),
                    false,
                ),
                (
                    serde_json::json!({"MENTION":{"MENTIONEES":[{"M":"other"}]}}),
                    false,
                ),
                (
                    serde_json::json!({"MENTION":{"MENTIONEES":[{"M":"me"}]}}),
                    true,
                ),
                (
                    serde_json::json!({"MENTION":{"MENTIONEES":[{"A":"1"},{"M":"me"}]}}),
                    true,
                ),
            ] {
                let row = line_rust_core::message::normalize(
                    &serde_json::json!({
                        "contentMetadata":metadata,
                    }),
                    "me",
                );
                let msg: MessageInfo = serde_json::from_value(row).unwrap();
                let mentioned = msg.mentioned_mids.iter().any(|mid| mid == "me");
                assert_eq!(mentioned, expected_mention);
                assert_eq!(
                    allows_chat_alert(hidden, muted, mentioned),
                    expected_mention || !(hidden || muted)
                );
            }
        }
    }
}
