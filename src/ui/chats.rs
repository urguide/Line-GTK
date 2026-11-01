use super::*;
use std::collections::BTreeSet;

fn pinned_chats(state: &AppState) -> BTreeSet<String> {
    let account = state.self_mid.borrow();
    account
        .as_ref()
        .and_then(|mid| state.config.borrow().pinned_chats.get(mid).cloned())
        .unwrap_or_default()
}

pub(super) fn chat_is_hidden(state: &AppState, mid: &str) -> bool {
    state
        .config
        .borrow()
        .chat_is_hidden(state.self_mid.borrow().as_deref(), mid)
}

pub(super) fn chat_memory_media(state: &AppState, mid: &str) -> bool {
    state
        .config
        .borrow()
        .chat_memory_media(state.self_mid.borrow().as_deref(), mid)
}

fn toggle_memory_media(state: &AppState, mid: &str) {
    state.message_cache.borrow_mut().remove(mid);
    let Some(account) = state.self_mid.borrow().clone() else {
        return;
    };
    {
        let mut config = state.config.borrow_mut();
        let chats = config.memory_media_chats.entry(account).or_default();
        if !chats.remove(mid) {
            chats.insert(mid.into());
        }
        state.sidecar.set_memory_media(&config);
        config.save(&state.data_dir);
    }
    if state.current_chat.borrow().as_deref() == Some(mid) {
        let chat = state.chats.borrow().iter().find(|c| c.mid == mid).cloned();
        if let Some(chat) = chat {
            for id in state.media_msgs.borrow().keys() {
                state.media_ready_paths.borrow_mut().remove(id);
            }
            *state.current_chat.borrow_mut() = None;
            open_chat(state, &chat);
        }
    }
}

pub(super) fn chat_is_muted(state: &AppState, mid: &str) -> bool {
    let server_muted = state
        .chats
        .borrow()
        .iter()
        .any(|chat| chat.mid == mid && chat.muted);
    state
        .config
        .borrow()
        .chat_is_muted(state.self_mid.borrow().as_deref(), mid, server_muted)
}

pub(super) fn refresh_chat_visibility(state: &AppState) {
    let query = if *state.sidebar_compact.borrow() {
        state.compact_search_entry.text()
    } else {
        state.search_entry.text()
    };
    filter_chats(state, &query);
    let count = state
        .chats
        .borrow()
        .iter()
        .filter(|chat| !chat_is_hidden(state, &chat.mid))
        .count();
    if count == 0 {
        set_side_state(state, "empty", Some(&crate::i18n::t("no_chats")));
    } else {
        set_side_state(state, "list", None);
    }
    refresh_tray_menu(state);
}

fn hide_chat(state: &AppState, mid: &str) {
    state.message_cache.borrow_mut().remove(mid);
    let Some(account) = state.self_mid.borrow().clone() else {
        return;
    };
    let Some(chat) = state.chats.borrow().iter().find(|c| c.mid == mid).cloned() else {
        return;
    };
    {
        let mut config = state.config.borrow_mut();
        config
            .hidden_chats
            .entry(account)
            .or_default()
            .insert(mid.to_string(), chat);
        if config.last_chat_mid == mid {
            config.last_chat_mid.clear();
        }
        config.save(&state.data_dir);
    }
    if state.current_chat.borrow().as_deref() == Some(mid) {
        *state.current_chat.borrow_mut() = None;
        let _ = state.sidecar.focus_chat("");
        mentions::reset(state);
        clear_reply(state);
        clear_messages(state);
        state.chat_list.unselect_all();
        state.chat_title.set_text(&crate::i18n::t("chats"));
        state.chat_subtitle.set_text("");
        state.mute_btn.set_sensitive(false);
        state.notes_btn.set_sensitive(false);
        refresh_call_controls(state);
        set_msg_state(state, "idle", None);
        sync_discord_rpc(state);
    }
    refresh_chat_visibility(state);
}

pub(super) fn unhide_chats(state: &AppState, account: &str, mids: &[String]) -> bool {
    if state.self_mid.borrow().as_deref() != Some(account) {
        return false;
    }
    let restored = {
        let mut config = state.config.borrow_mut();
        let restored: Vec<_> = config
            .hidden_chats
            .get_mut(account)
            .map(|hidden| mids.iter().filter_map(|mid| hidden.remove(mid)).collect())
            .unwrap_or_default();
        config.save(&state.data_dir);
        restored
    };
    let mut chats = state.chats.borrow().clone();
    for chat in restored {
        if !chats.iter().any(|c| c.mid == chat.mid) {
            chats.push(chat);
        }
    }
    apply_chats(state, chats, false);
    true
}

fn sort_chats(chats: &mut [ChatInfo], pinned: &BTreeSet<String>) {
    chats.sort_by_key(|chat| {
        (
            !pinned.contains(&chat.mid),
            std::cmp::Reverse(chat.last_activity),
        )
    });
}

/// New activity moves to the front of its own section, below all pins if unpinned.
fn activity_position(chats: &[ChatInfo], pinned: &BTreeSet<String>, mid: &str) -> usize {
    if pinned.contains(mid) {
        0
    } else {
        chats
            .iter()
            .take_while(|chat| pinned.contains(&chat.mid))
            .count()
    }
}

fn toggle_chat_pin(state: &AppState, mid: &str) {
    let Some(account) = state.self_mid.borrow().clone() else {
        return;
    };
    {
        let mut config = state.config.borrow_mut();
        let pinned = config.pinned_chats.entry(account).or_default();
        if !pinned.remove(mid) {
            pinned.insert(mid.to_string());
        }
        config.save(&state.data_dir);
    }
    let chats = state.chats.borrow().clone();
    apply_chats(state, chats, false);
}

pub(super) fn wire_chat_context_menu(state: &AppState) {
    let click = gtk::GestureClick::new();
    click.set_button(3);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let s = state.clone();
    click.connect_pressed(move |gesture, _, x, y| {
        let Some(row) = s.chat_list.row_at_y(y as i32) else {
            return;
        };
        let Some(mid) = s
            .chats
            .borrow()
            .get(row.index() as usize)
            .map(|c| c.mid.clone())
        else {
            return;
        };
        if s.self_mid.borrow().is_none() {
            return;
        }
        let Some(point) = s
            .chat_list
            .compute_point(&row, &gtk::graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.set_parent(&row);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(
            point.x() as i32,
            point.y() as i32,
            1,
            1,
        )));
        let actions = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let button = gtk::Button::with_label(&crate::i18n::t(if pinned_chats(&s).contains(&mid) {
            "unpin_chat"
        } else {
            "pin_chat"
        }));
        button.add_css_class("flat");
        actions.append(&button);
        let mute = gtk::Button::with_label(&crate::i18n::t(if chat_is_muted(&s, &mid) {
            "unmute_chat"
        } else {
            "mute_chat"
        }));
        mute.add_css_class("flat");
        actions.append(&mute);
        let memory = gtk::Button::with_label(&crate::i18n::t(if chat_memory_media(&s, &mid) {
            "disable_memory_media"
        } else {
            "enable_memory_media"
        }));
        memory.add_css_class("flat");
        actions.append(&memory);
        let state = s.clone();
        let memory_mid = mid.clone();
        let popup = popover.downgrade();
        memory.connect_clicked(move |_| {
            if let Some(popup) = popup.upgrade() {
                popup.popdown();
            }
            toggle_memory_media(&state, &memory_mid);
        });
        let hide = gtk::Button::with_label(&crate::i18n::t("hide_chat"));
        hide.add_css_class("flat");
        actions.append(&hide);
        let copy_id = gtk::Button::with_label(&crate::i18n::t("copy_chat_id"));
        copy_id.add_css_class("flat");
        actions.append(&copy_id);
        popover.set_child(Some(&actions));

        // A live list refresh can remove the anchor while its menu is open.
        let popup = popover.downgrade();
        let unmap_handler = row.connect_unmap(move |_| {
            if let Some(popup) = popup.upgrade() {
                popup.popdown();
            }
        });
        let row = row.downgrade();
        let handler = RefCell::new(Some(unmap_handler));
        popover.connect_closed(move |popup| {
            if let Some(row) = row.upgrade()
                && let Some(handler) = handler.borrow_mut().take()
            {
                row.disconnect(handler);
            }
            if popup.parent().is_some() {
                popup.unparent();
            }
        });
        let state = s.clone();
        let popup = popover.downgrade();
        let pin_mid = mid.clone();
        button.connect_clicked(move |_| {
            if let Some(popup) = popup.upgrade() {
                popup.popdown();
            }
            toggle_chat_pin(&state, &pin_mid);
        });
        let state = s.clone();
        let popup = popover.downgrade();
        let mute_mid = mid.clone();
        mute.connect_clicked(move |_| {
            if let Some(popup) = popup.upgrade() {
                popup.popdown();
            }
            toggle_chat_mute_for(&state, &mute_mid);
        });
        let state = s.clone();
        let popup = popover.downgrade();
        let copy_mid = mid.clone();
        copy_id.connect_clicked(move |_| {
            state.window.clipboard().set_text(&copy_mid);
            if let Some(popup) = popup.upgrade() {
                popup.popdown();
            }
            toast(&state, &crate::i18n::t("chat_id_copied"));
        });
        let state = s.clone();
        let popup = popover.downgrade();
        hide.connect_clicked(move |_| {
            if let Some(popup) = popup.upgrade() {
                popup.popdown();
            }
            hide_chat(&state, &mid);
        });
        popover.popup();
    });
    state.chat_list.add_controller(click);
}

pub(super) fn apply_chats(state: &AppState, mut chats: Vec<ChatInfo>, cached: bool) {
    let pinned = pinned_chats(state);
    sort_chats(&mut chats, &pinned);
    clear_list(&state.chat_list);
    state.chat_avatars.borrow_mut().clear();
    state.chat_previews.borrow_mut().clear();
    state.chat_unread_badges.borrow_mut().clear();

    for chat in &mut chats {
        if chat.mid.starts_with('u') {
            senders::apply_profile(state, chat);
        }
        if !chat.preview.is_empty() {
            chat.preview = localize_preview(&chat.preview);
        }
    }

    for chat in &chats {
        let (row, avatar, preview, badge) = build_chat_row(
            chat,
            *state.sidebar_compact.borrow(),
            pinned.contains(&chat.mid),
        );
        state
            .chat_avatars
            .borrow_mut()
            .insert(chat.mid.clone(), avatar);
        state
            .chat_previews
            .borrow_mut()
            .insert(chat.mid.clone(), preview);
        state
            .chat_unread_badges
            .borrow_mut()
            .insert(chat.mid.clone(), badge);
        state.chat_list.append(&row);
    }

    let n = chats
        .iter()
        .filter(|chat| !chat_is_hidden(state, &chat.mid))
        .count();
    *state.chats.borrow_mut() = chats;
    if let Some(mid) = state.current_chat.borrow().as_deref()
        && let Some(chat) = state.chats.borrow().iter().find(|c| c.mid == mid)
    {
        state.chat_title.set_text(&chat.name);
    }
    filter_chats(
        state,
        &if *state.sidebar_compact.borrow() {
            state.compact_search_entry.text()
        } else {
            state.search_entry.text()
        },
    );

    if n == 0 {
        set_side_state(state, "empty", Some(&crate::i18n::t("no_chats")));
        state.status.set_text(&crate::i18n::t("status_no_chats"));
    } else {
        set_side_state(state, "list", None);
        let n_str = n.to_string();
        let status = if cached {
            crate::i18n::tf("status_chats_cache", &[("n", &n_str)])
        } else {
            crate::i18n::tf("status_chats_latest", &[("n", &n_str)])
        };
        state.status.set_text(&status);
        // After every rebuild, keep the open chat highlighted in the sidebar.
        select_sidebar_chat(state, state.current_chat.borrow().as_deref());
        maybe_restore_last_chat(state);
    }
    refresh_tray_menu(state);
}

pub(super) fn select_sidebar_chat(state: &AppState, mid: Option<&str>) {
    let Some(mid) = mid else {
        return;
    };
    if chat_is_hidden(state, mid) {
        return;
    }
    let Some(pos) = state.chats.borrow().iter().position(|c| c.mid == mid) else {
        return;
    };
    if let Some(row) = state.chat_list.row_at_index(pos as i32) {
        state.chat_list.select_row(Some(&row));
        // Ensure the selected row is scrolled into view.
        row.grab_focus();
    }
}

/// Ensure a sidebar row exists for this mid (first message from a new friend).
pub(super) fn ensure_chat_visible(state: &AppState, chat: &ChatInfo) {
    if state.chats.borrow().iter().any(|c| c.mid == chat.mid) {
        return;
    }
    upsert_chat_row(state, chat.clone());
}

pub(super) fn upsert_chat_row(state: &AppState, chat: ChatInfo) {
    let mid = chat.mid.clone();
    let existed = state.chats.borrow().iter().any(|c| c.mid == mid);

    if existed {
        if let Some(cur) = state.chats.borrow_mut().iter_mut().find(|c| c.mid == mid) {
            if !chat.name.is_empty() && chat.name != mid {
                cur.name = chat.name.clone();
                cur.alias = chat.alias.clone();
            }
            if !chat.kind.is_empty() {
                cur.kind = chat.kind.clone();
            }
            if !chat.preview.is_empty() {
                cur.preview = chat.preview.clone();
            }
            if chat.last_activity > cur.last_activity {
                cur.last_activity = chat.last_activity;
            }
            if let Some(path) = chat.avatar_path.clone() {
                cur.avatar_path = Some(path);
            }
        }
        if let Some(label) = state.chat_previews.borrow().get(&mid)
            && !chat.preview.is_empty()
        {
            label.set_text(&chat.preview);
        }
        // Refresh visible name on the row: rebuild is heavy; update via child labels if needed.
        // Promote to top so new activity is obvious.
        promote_chat_to_top(state, &mid);
        // Update name label on the row (first heading label in the row).
        if let Some(pos) = state.chats.borrow().iter().position(|c| c.mid == mid)
            && let Some(row) = state.chat_list.row_at_index(pos as i32)
        {
            update_chat_row_name(&row, &chat.name);
            if let Some(path) = chat.avatar_path.as_deref()
                && std::path::Path::new(path).exists()
                && let Some(img) = state.chat_avatars.borrow().get(&mid).cloned()
            {
                attach_texture_async(img, path.to_string(), 72);
            }
        }
        return;
    }

    let pinned = pinned_chats(state);
    let (row, avatar, preview, badge) = build_chat_row(
        &chat,
        *state.sidebar_compact.borrow(),
        pinned.contains(&mid),
    );
    state.chat_avatars.borrow_mut().insert(mid.clone(), avatar);
    state
        .chat_previews
        .borrow_mut()
        .insert(mid.clone(), preview);
    state
        .chat_unread_badges
        .borrow_mut()
        .insert(mid.clone(), badge);
    let target = activity_position(&state.chats.borrow(), &pinned, &mid);
    state.chats.borrow_mut().insert(target, chat);
    state.chat_list.insert(&row, target as i32);
    refresh_chat_visibility(state);
    let n = state
        .chats
        .borrow()
        .iter()
        .filter(|chat| !chat_is_hidden(state, &chat.mid))
        .count();
    state.status.set_text(&crate::i18n::tf(
        "status_chats_live",
        &[("n", &n.to_string())],
    ));
    refresh_tray_menu(state);
}

pub(super) fn promote_chat_to_top(state: &AppState, mid: &str) {
    let pos = {
        let chats = state.chats.borrow();
        chats.iter().position(|c| c.mid == mid)
    };
    let Some(pos) = pos else { return };
    let target = activity_position(&state.chats.borrow(), &pinned_chats(state), mid);
    if pos == target {
        return;
    }
    {
        let mut chats = state.chats.borrow_mut();
        let chat = chats.remove(pos);
        chats.insert(target, chat);
    }
    if let Some(row) = state.chat_list.row_at_index(pos as i32) {
        let visible = row.is_visible();
        state.chat_list.remove(&row);
        state.chat_list.insert(&row, target as i32);
        row.set_visible(visible && !chat_is_hidden(state, mid));
        if state.config.borrow().animations {
            row.add_css_class("line-chat-bump");
            let row_c = row.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(340), move || {
                row_c.remove_css_class("line-chat-bump");
            });
        }
    }
    // Keep selection on the open chat after row reorder.
    select_sidebar_chat(state, state.current_chat.borrow().as_deref());
    refresh_tray_menu(state);
}

pub(super) fn update_chat_row_name(row: &gtk::ListBoxRow, name: &str) {
    if name.is_empty() {
        return;
    }
    row.set_tooltip_text(Some(name));
    // row > box > overlay + text_col > top > name label
    let Some(outer) = row.child() else { return };
    let Ok(box_) = outer.downcast::<gtk::Box>() else {
        return;
    };
    let mut child = box_.first_child();
    while let Some(w) = child {
        if w.css_classes().iter().any(|c| c == "line-chat-text")
            && let Ok(inner) = w.clone().downcast::<gtk::Box>()
            && let Some(top) = inner.first_child()
            && let Ok(top_box) = top.downcast::<gtk::Box>()
            && let Some(name_w) = top_box.first_child()
            && let Ok(label) = name_w.downcast::<gtk::Label>()
            && label.css_classes().iter().any(|c| c == "line-chat-name")
        {
            label.set_text(name);
            return;
        }
        child = w.next_sibling();
    }
}

pub(super) fn maybe_restore_last_chat(state: &AppState) {
    if *state.restored_last_chat.borrow() {
        return;
    }
    if state.current_chat.borrow().is_some() {
        *state.restored_last_chat.borrow_mut() = true;
        return;
    }
    if !*state.session_ready.borrow() {
        return;
    }
    let last = state.config.borrow().last_chat_mid.clone();
    if last.is_empty() || chat_is_hidden(state, &last) || chat_memory_media(state, &last) {
        *state.restored_last_chat.borrow_mut() = true;
        return;
    }
    let chats = state.chats.borrow();
    let Some(chat) = chats.iter().find(|c| c.mid == last).cloned() else {
        return; // keep trying until the mid appears in a later refresh
    };
    drop(chats);
    *state.restored_last_chat.borrow_mut() = true;
    open_chat(state, &chat);
    select_sidebar_chat(state, Some(chat.mid.as_str()));
}

pub(super) fn build_chat_row(
    chat: &ChatInfo,
    compact: bool,
    pinned: bool,
) -> (gtk::ListBoxRow, gtk::Picture, gtk::Label, gtk::Label) {
    let row = gtk::ListBoxRow::new();
    row.set_tooltip_text(Some(&chat.name));
    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(if compact { 0 } else { 12 })
        .margin_start(if compact { 0 } else { 8 })
        .margin_end(if compact { 0 } else { 8 })
        .margin_top(8)
        .margin_bottom(8)
        .hexpand(!compact)
        .halign(if compact {
            gtk::Align::Center
        } else {
            gtk::Align::Fill
        })
        .css_classes(["line-chat-row"])
        .build();

    let avatar_px = if compact {
        AVATAR_COMPACT_PX
    } else {
        AVATAR_CHAT_PX
    };
    let avatar_frame = gtk::Box::builder()
        .width_request(avatar_px)
        .height_request(avatar_px)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["line-avatar-frame"])
        .build();
    let avatar = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .can_shrink(true)
        .width_request(avatar_px)
        .height_request(avatar_px)
        .css_classes(["line-avatar"])
        .build();
    if let Some(path) = chat.avatar_path.as_deref()
        && std::path::Path::new(path).exists()
    {
        attach_texture_async(avatar.clone(), path.to_string(), avatar_px * 2);
    }
    avatar_frame.append(&avatar);

    let avatar_overlay = gtk::Overlay::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["line-avatar-overlay"])
        .build();
    avatar_overlay.set_child(Some(&avatar_frame));

    let badge = gtk::Label::builder()
        .label(if chat.unread > 0 {
            chat.unread.to_string()
        } else {
            String::new()
        })
        .css_classes(["line-unread", "line-unread-overlay"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .visible(chat.unread > 0)
        .build();
    avatar_overlay.add_overlay(&badge);

    if pinned {
        let pin = gtk::Image::builder()
            .icon_name("view-pin-symbolic")
            .pixel_size(12)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .tooltip_text(crate::i18n::t("chat_pinned"))
            .css_classes(["line-chat-pin"])
            .build();
        avatar_overlay.add_overlay(&pin);
    }

    let text_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .visible(!compact)
        .css_classes(["line-chat-text"])
        .build();

    let top = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let name = gtk::Label::builder()
        .label(&chat.name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["line-chat-name"])
        .build();
    let time = gtk::Label::builder()
        .label(format_activity(chat.last_activity))
        .css_classes(["dim-label", "caption"])
        .build();
    top.append(&name);
    top.append(&time);

    let bottom = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let preview_text = if chat.preview.trim().is_empty() {
        if chat.last_activity > 0 {
            crate::i18n::t("tap_open")
        } else {
            crate::i18n::t("no_recent")
        }
    } else {
        localize_preview(&chat.preview)
    };
    let preview = gtk::Label::builder()
        .label(&preview_text)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["line-chat-preview"])
        .build();
    bottom.append(&preview);

    text_col.append(&top);
    text_col.append(&bottom);
    box_.append(&avatar_overlay);
    box_.append(&text_col);
    row.set_child(Some(&box_));
    (row, avatar, preview, badge)
}

pub(super) fn format_activity(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let secs = ts / 1000;
    let now = chrono::Utc::now().timestamp();
    let diff = (now - secs).max(0);
    if diff < 60 {
        "now".into()
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86400 {
        format!("{}h", diff / 3600)
    } else if diff < 86400 * 7 {
        format!("{}d", diff / 86400)
    } else {
        chrono::DateTime::from_timestamp(secs, 0)
            .map(|d| d.format("%m/%d").to_string())
            .unwrap_or_default()
    }
}

pub(super) fn clear_list(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

pub(super) fn mark_chat_read(state: &AppState, chat_mid: &str) {
    if !state.config.borrow().auto_mark_read {
        return;
    }
    let Some(last_id) = state.last_incoming_id.borrow().clone() else {
        return;
    };
    if last_id.is_empty() {
        return;
    }
    let _ = state.sidecar.mark_read(chat_mid, &last_id);
}

pub(super) fn clear_unread_badge(state: &AppState, chat_mid: &str) {
    if let Some(chat) = state
        .chats
        .borrow_mut()
        .iter_mut()
        .find(|c| c.mid == chat_mid)
    {
        chat.unread = 0;
    }
    if let Some(badge) = state.chat_unread_badges.borrow().get(chat_mid) {
        badge.set_text("");
        badge.set_visible(false);
    }
    if state.current_chat.borrow().as_deref() == Some(chat_mid) {
        state
            .chat_subtitle
            .set_text(&crate::i18n::t("conversation"));
    }
    refresh_tray_menu(state);
}

pub(super) fn bump_unread(state: &AppState, chat_mid: &str) {
    let n = if let Some(chat) = state
        .chats
        .borrow_mut()
        .iter_mut()
        .find(|c| c.mid == chat_mid)
    {
        chat.unread = chat.unread.saturating_add(1);
        chat.unread
    } else {
        1
    };
    if let Some(badge) = state.chat_unread_badges.borrow().get(chat_mid) {
        badge.set_text(&n.to_string());
        badge.set_visible(true);
    }
    refresh_tray_menu(state);
}

pub(super) fn update_mute_btn(state: &AppState, muted: bool) {
    if muted {
        state.mute_btn.set_icon_name("audio-volume-muted-symbolic");
        state
            .mute_btn
            .set_tooltip_text(Some(&crate::i18n::t("unmute_chat")));
    } else {
        state.mute_btn.set_icon_name("audio-volume-high-symbolic");
        state
            .mute_btn
            .set_tooltip_text(Some(&crate::i18n::t("mute_chat")));
    }
}

pub(super) fn toggle_chat_mute(state: &AppState) {
    let Some(mid) = state.current_chat.borrow().clone() else {
        return;
    };
    toggle_chat_mute_for(state, &mid);
}

fn toggle_chat_mute_for(state: &AppState, mid: &str) {
    let next = !chat_is_muted(state, mid);
    if !mid.starts_with('u') {
        let Some(account) = state.self_mid.borrow().clone() else {
            return;
        };
        {
            let mut config = state.config.borrow_mut();
            let muted = config.muted_chats.entry(account).or_default();
            muted.insert(mid.to_string(), next);
            config.save(&state.data_dir);
        }
        if state.current_chat.borrow().as_deref() == Some(mid) {
            update_mute_btn(state, next);
        }
        toast(
            state,
            &crate::i18n::t(if next { "chat_muted" } else { "chat_unmuted" }),
        );
        return;
    }
    match state.sidecar.mute_chat(mid, next) {
        // The worker's ChatMute event updates UI only after LINE and SQLite
        // acknowledge the change. Enqueuing a request is not confirmation.
        Ok(_) => {}
        Err(e) => toast(
            state,
            &crate::i18n::tf("mute_failed", &[("error", &e.to_string())]),
        ),
    }
}

pub(super) fn apply_read_counts(state: &AppState, chat_mid: &str, counts: HashMap<String, u64>) {
    let mut all = state.read_counts.borrow_mut();
    let current = all.entry(chat_mid.to_owned()).or_default();
    for (id, count) in counts {
        let previous = current.entry(id).or_default();
        *previous = (*previous).max(count);
    }
    if state.current_chat.borrow().as_deref() != Some(chat_mid) {
        return;
    }
    let times = state.msg_created.borrow().clone();
    for (id, label) in state.receipt_slots.borrow().iter() {
        if let Some(count) = current.get(id).filter(|n| **n > 0) {
            let ts = times.get(id).copied().unwrap_or(0);
            label.set_text(&format_outgoing_status(Some(*count), ts));
            label.set_tooltip_text(Some(&crate::i18n::t("status_read")));
            label.add_css_class("line-msg-status-read");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(mid: &str, last_activity: i64) -> ChatInfo {
        serde_json::from_value(serde_json::json!({
            "mid": mid,
            "name": mid,
            "lastActivity": last_activity,
        }))
        .unwrap()
    }

    fn mids(chats: &[ChatInfo]) -> Vec<&str> {
        chats.iter().map(|chat| chat.mid.as_str()).collect()
    }

    #[test]
    fn refresh_and_pin_changes_sort_each_section_by_activity() {
        let mut chats = vec![
            chat("u1", 40),
            chat("c1", 10),
            chat("u2", 30),
            chat("c2", 20),
        ];
        let mut pinned = BTreeSet::from(["c1".into(), "u2".into()]);
        sort_chats(&mut chats, &pinned);
        assert_eq!(mids(&chats), ["u2", "c1", "u1", "c2"]);

        // Pinning an older conversation does not fake new message activity.
        pinned.insert("c2".into());
        sort_chats(&mut chats, &pinned);
        assert_eq!(mids(&chats), ["u2", "c2", "c1", "u1"]);

        // An unpinned conversation returns to its chronological position.
        pinned.remove("u2");
        sort_chats(&mut chats, &pinned);
        assert_eq!(mids(&chats), ["c2", "c1", "u1", "u2"]);
    }

    #[test]
    fn live_activity_and_new_chats_stay_in_their_section() {
        let pinned = BTreeSet::from(["u1".into(), "c1".into(), "c3".into()]);
        let mut chats = vec![
            chat("u1", 30),
            chat("c1", 20),
            chat("u2", 50),
            chat("c2", 40),
        ];
        let target = activity_position(&chats, &pinned, "c1");
        let updated = chats.remove(1);
        chats.insert(target, updated);
        assert_eq!(mids(&chats), ["c1", "u1", "u2", "c2"]);

        let target = activity_position(&chats, &pinned, "c2");
        let updated = chats.remove(3);
        chats.insert(target, updated);
        assert_eq!(mids(&chats), ["c1", "u1", "c2", "u2"]);

        let target = activity_position(&chats, &pinned, "new-user");
        chats.insert(target, chat("new-user", 60));
        let target = activity_position(&chats, &pinned, "c3");
        chats.insert(target, chat("c3", 70));
        assert_eq!(mids(&chats), ["c3", "c1", "u1", "new-user", "c2", "u2"]);
        assert_eq!(activity_position(&chats, &BTreeSet::new(), "u2"), 0);
        assert_eq!(activity_position(&[], &pinned, "new-user"), 0);
    }
}
