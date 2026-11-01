use super::*;

#[derive(Default)]
pub(super) struct MessageLayout {
    rows: Vec<LayoutRow>,
}

struct LayoutRow {
    id: String,
    from: String,
    mine: bool,
    time: i64,
    day: gtk::Label,
    identity: Option<(gtk::Widget, gtk::Widget)>,
}

fn milliseconds(time: i64) -> i64 {
    if time > 1_000_000_000_000 {
        time
    } else {
        time.saturating_mul(1000)
    }
}

fn message_boundary(
    previous: Option<(&str, bool, i64)>,
    from: &str,
    mine: bool,
    time: i64,
) -> bool {
    let Some((sender, was_mine, last_time)) = previous else {
        return false;
    };
    !from.is_empty()
        && sender == from
        && was_mine == mine
        && last_time > 0
        && time > 0
        && (0..=5 * 60 * 1000).contains(&milliseconds(time).saturating_sub(milliseconds(last_time)))
}

fn update_layout_row(previous: Option<&LayoutRow>, row: &LayoutRow) {
    let consecutive = message_boundary(
        previous.map(|p| (p.from.as_str(), p.mine, p.time)),
        &row.from,
        row.mine,
        row.time,
    );
    let new_day = row.time > 0 && previous.is_none_or(|p| day_key(p.time) != day_key(row.time));
    row.day.set_visible(new_day);
    if let Some((avatar, name)) = &row.identity {
        avatar.set_visible(!consecutive);
        name.set_visible(!consecutive);
    }
}

fn refresh_message_layout(state: &AppState) {
    let layout = state.message_layout.borrow();
    for (index, row) in layout.rows.iter().enumerate() {
        update_layout_row(index.checked_sub(1).map(|i| &layout.rows[i]), row);
    }
}

pub(super) fn remove_message_layout(state: &AppState, id: &str) {
    state
        .message_layout
        .borrow_mut()
        .rows
        .retain(|r| r.id != id);
    refresh_message_layout(state);
}

pub(super) fn load_older_messages(state: &AppState) {
    if *state.history_loading.borrow()
        || !*state.history_has_more.borrow()
        || *state.scroll_pinning.borrow()
        || !*state.session_ready.borrow()
    {
        return;
    }
    let Some(mid) = state.current_chat.borrow().clone() else {
        return;
    };
    let Some(cursor) = state.history_cursor.borrow().clone() else {
        return;
    };
    match state.sidecar.fetch_older_messages(&mid, cursor) {
        Ok(id) => {
            *state.history_loading.borrow_mut() = true;
            *state.stick_bottom.borrow_mut() = false;
            state.pending.borrow_mut().insert(
                id,
                Pending::FetchMessages {
                    chat_mid: mid,
                    generation: *state.history_generation.borrow(),
                    older: true,
                },
            );
        }
        Err(error) => toast(state, &error.to_string()),
    }
}

pub(super) fn prepend_messages(state: &AppState, mut messages: Vec<MessageInfo>) {
    messages.retain(|m| {
        !state.seen_msg_ids.borrow().contains(&m.id)
            && !state.unsent_msg_ids.borrow().contains(&m.id)
    });
    if messages.is_empty() {
        return;
    }
    messages.sort_by_key(|m| m.created_time);
    let old_count = state.message_list.len();
    let adj = state.message_scroll.vadjustment();
    let old_upper = adj.upper();
    let old_value = adj.value();
    let old_layout_count = state.message_layout.borrow().rows.len();
    *state.scroll_pinning.borrow_mut() = true;
    *state.stick_bottom.borrow_mut() = false;
    for message in &messages {
        append_message(state, message, false);
    }
    state.message_list.move_tail_to_front(old_count);
    state
        .message_layout
        .borrow_mut()
        .rows
        .rotate_left(old_layout_count);
    refresh_message_layout(state);
    super::replies::refresh_all(state);
    *state.msg_list_fp.borrow_mut() = None;
    let generation = *state.history_generation.borrow();
    let s = state.clone();
    // Wait for GTK to allocate the inserted rows before compensating the scroll.
    let frames = std::cell::Cell::new(0);
    state.message_list.widget().add_tick_callback(move |_, _| {
        if *s.history_generation.borrow() != generation {
            *s.scroll_pinning.borrow_mut() = false;
            return glib::ControlFlow::Break;
        }
        frames.set(frames.get() + 1);
        if frames.get() < 3 {
            return glib::ControlFlow::Continue;
        }
        let adj = s.message_scroll.vadjustment();
        adj.set_value(old_value + (adj.upper() - old_upper).max(0.0));
        *s.scroll_pinning.borrow_mut() = false;
        glib::ControlFlow::Break
    });
}

pub(super) fn message_list_fingerprint(messages: &[MessageInfo]) -> (usize, String, String) {
    let first = messages.first().map(|m| m.id.clone()).unwrap_or_default();
    let last = messages.last().map(|m| m.id.clone()).unwrap_or_default();
    (messages.len(), first, last)
}

pub(super) fn same_message_list(
    state: &AppState,
    chat_mid: &str,
    messages: &[MessageInfo],
) -> bool {
    let (len, first, last) = message_list_fingerprint(messages);
    matches!(
        state.msg_list_fp.borrow().as_ref(),
        Some((mid, l, f, la)) if mid == chat_mid && *l == len && f == &first && la == &last
    )
}

pub(super) fn apply_messages(state: &AppState, mut messages: Vec<MessageInfo>) {
    clear_messages(state);
    messages.retain(|msg| !state.unsent_msg_ids.borrow().contains(&msg.id));
    state.media_queue.borrow_mut().clear();
    *state.stick_bottom.borrow_mut() = true;
    messages.sort_by_key(|m| m.created_time);
    state
        .message_index
        .borrow_mut()
        .extend(messages.iter().map(|m| (m.id.clone(), m.clone())));
    if let Some(mid) = state.current_chat.borrow().clone() {
        let (len, first, last) = message_list_fingerprint(&messages);
        *state.msg_list_fp.borrow_mut() = Some((mid, len, first, last));
    } else {
        *state.msg_list_fp.borrow_mut() = None;
    }
    if messages.is_empty() {
        set_msg_state(state, "empty", Some(&crate::i18n::t("no_messages")));
        state.chat_subtitle.set_text(&crate::i18n::t("empty"));
        return;
    }
    if let Some(mid) = state.current_chat.borrow().clone()
        && let Some(message) = messages.last()
    {
        update_chat_preview(state, &mid, message);
    }

    // Append in idle chunks so image-heavy threads don't freeze the window.
    let total = messages.len();
    set_msg_state(state, "list", None);
    state.chat_subtitle.set_text(&crate::i18n::tf(
        "messages_count",
        &[("n", &total.to_string())],
    ));

    let last_incoming = messages
        .iter()
        .rev()
        .find(|m| !m.mine)
        .map(|m| m.id.clone());
    if let Some(id) = last_incoming.clone() {
        *state.last_incoming_id.borrow_mut() = Some(id);
    }

    let chat_mid = state.current_chat.borrow().clone();
    let state_c = state.clone();
    let batch = Rc::new(RefCell::new(messages));
    let generation = *state.message_generation.borrow();
    glib::idle_add_local(move || {
        if *state_c.message_generation.borrow() != generation {
            return glib::ControlFlow::Break;
        }
        let mut left = batch.borrow_mut();
        let take = left.len().min(14);
        if take == 0 {
            pin_messages_to_latest(&state_c);
            if let Some(mid) = chat_mid.as_deref() {
                mark_chat_read(&state_c, mid);
                clear_unread_badge(&state_c, mid);
            }
            return glib::ControlFlow::Break;
        }
        let chunk: Vec<_> = left.drain(0..take).collect();
        drop(left);
        for msg in &chunk {
            append_message(&state_c, msg, false);
        }
        // Stay on the latest message while rows are still being inserted.
        scroll_messages_to_end(&state_c);
        glib::ControlFlow::Continue
    });
}

pub(super) fn append_message(state: &AppState, msg: &MessageInfo, live: bool) {
    if state.unsent_msg_ids.borrow().contains(&msg.id) {
        return;
    }
    if !msg.id.is_empty() && !state.seen_msg_ids.borrow_mut().insert(msg.id.clone()) {
        return;
    }
    state
        .message_index
        .borrow_mut()
        .insert(msg.id.clone(), msg.clone());

    let memory = state
        .current_chat
        .borrow()
        .as_deref()
        .is_some_and(|mid| chat_memory_media(state, mid))
        && matches!(msg.content_type.as_str(), "IMAGE" | "VIDEO")
        && !msg.id.starts_with("pending-");
    let memory_msg;
    let msg = if memory {
        memory_msg = MessageInfo {
            image_path: None,
            file_path: None,
            ..msg.clone()
        };
        &memory_msg
    } else {
        msg
    };

    // Prefer a path that already finished hydrating (race with list rebuild).
    let ready_path = {
        let missing = msg
            .image_path
            .as_deref()
            .map(|p| !std::path::Path::new(p).exists())
            .unwrap_or(true);
        if missing && !memory {
            state
                .media_ready_paths
                .borrow()
                .get(&msg.id)
                .filter(|p| std::path::Path::new(p).exists())
                .cloned()
        } else {
            None
        }
    };
    let msg_owned;
    let msg = if let Some(path) = ready_path {
        msg_owned = MessageInfo {
            image_path: Some(path),
            ..msg.clone()
        };
        &msg_owned
    } else {
        msg
    };

    let list = &state.message_list;

    let row = gtk::ListBoxRow::new();
    let row_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let day = gtk::Label::builder()
        .label(format_day_separator(msg.created_time))
        .halign(gtk::Align::Center)
        .css_classes(["line-day-sep", "line-day-sep-row"])
        .build();
    row_content.append(&day);
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .hexpand(true)
        .valign(gtk::Align::End)
        .build();

    let is_sticker = msg.content_type.eq_ignore_ascii_case("sticker");
    let is_flex = msg.content_type.eq_ignore_ascii_case("flex") || msg.flex.is_some();
    let is_pending = msg.id.starts_with("pending-");
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .halign(if msg.mine {
            gtk::Align::End
        } else {
            gtk::Align::Start
        })
        .build();
    let identity = senders::append(state, msg, &outer, &content);
    if let Some(id) = msg.related_message_id.as_deref() {
        super::replies::append(state, &content, id);
    }
    let bubble = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(if msg.mine {
            gtk::Align::End
        } else {
            gtk::Align::Start
        })
        .css_classes(if is_sticker {
            vec!["line-bubble", "line-bubble-sticker"]
        } else if is_flex {
            vec!["line-bubble", "line-bubble-in"]
        } else {
            vec![
                "line-bubble",
                if msg.mine {
                    "line-bubble-out"
                } else {
                    "line-bubble-in"
                },
            ]
        })
        .build();
    if is_pending {
        bubble.add_css_class("line-bubble-pending");
        outer.add_css_class("line-msg-pending");
    }

    let has_image = msg
        .image_path
        .as_deref()
        .map(|p| std::path::Path::new(p).exists())
        .unwrap_or(false);
    let waiting_visual = (msg.content_type.eq_ignore_ascii_case("image")
        || msg.content_type.eq_ignore_ascii_case("sticker")
        || msg.content_type.eq_ignore_ascii_case("video"))
        && !has_image;
    let is_audio = msg.content_type.eq_ignore_ascii_case("audio");
    let is_file = msg.content_type.eq_ignore_ascii_case("file");
    let is_video = msg.content_type.eq_ignore_ascii_case("video");
    let is_image = msg.content_type.eq_ignore_ascii_case("image");
    if memory {
        bubble.set_size_request(240, 180);
    }
    let has_audio = msg
        .audio_path
        .as_deref()
        .map(|p| std::path::Path::new(p).exists())
        .unwrap_or(false);

    if let Some(flex) = msg.flex.as_ref() {
        append_flex_card(state, &bubble, msg, flex);
    } else if is_audio {
        append_voice_card(state, &bubble, msg, has_audio);
    } else if is_file {
        append_file_card(state, &bubble, msg);
    } else {
        let show_text = !msg.text.is_empty()
            && msg.text != "[Image]"
            && msg.text != "[Video]"
            && msg.text != "[Sticker]"
            && !is_sticker;

        if show_text && !msg.emojis.is_empty() {
            super::expressions::append_inline(state, &bubble, msg);
            append_link_chips(state, &bubble, &msg.text);
        } else if show_text {
            let label = gtk::Label::builder()
                .label(&msg.text)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .selectable(true)
                .max_width_chars(42)
                .css_classes(["line-bubble-text"])
                .build();
            bubble.append(&label);
            append_link_chips(state, &bubble, &msg.text);
        } else if !has_image && !waiting_visual {
            let fallback = if msg.content_type.is_empty() {
                "(message)".to_string()
            } else {
                format!("({})", msg.content_type.to_lowercase())
            };
            let label = gtk::Label::builder()
                .label(&fallback)
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build();
            bubble.append(&label);
        }
    }

    if has_image {
        if let Some(path) = msg.image_path.as_deref() {
            let pic = make_media_picture_placeholder(is_sticker);
            let animate = is_sticker && state.config.borrow().animations;
            attach_texture_async_anim(
                pic.clone(),
                path.to_string(),
                if is_sticker { 128 } else { 320 },
                animate,
            );
            if !is_sticker && (is_image || is_video) {
                if is_video {
                    let overlay = wrap_video_thumb(state, &pic, msg);
                    bubble.append(&overlay);
                } else {
                    wire_media_open_click(state, &pic, msg, "image");
                    pic.set_tooltip_text(Some(&crate::i18n::t("media_open_image")));
                    bubble.append(&pic);
                }
            } else {
                bubble.append(&pic);
            }
        }
    } else if waiting_visual {
        if is_video {
            append_video_placeholder(state, &bubble, msg, false);
        } else {
            let placeholder = gtk::Label::builder()
                .label(if is_sticker {
                    crate::i18n::t("loading_sticker")
                } else {
                    crate::i18n::t("loading_image")
                })
                .xalign(0.0)
                .css_classes(["dim-label", "line-media-placeholder"])
                .build();
            bubble.append(&placeholder);
        }
    }

    if !msg.id.is_empty() && message_tracks_media(msg) {
        state
            .media_slots
            .borrow_mut()
            .insert(msg.id.clone(), bubble.clone());
        state
            .media_msgs
            .borrow_mut()
            .insert(msg.id.clone(), msg.clone());
    }

    if !msg.mine && !msg.id.is_empty() {
        *state.last_incoming_id.borrow_mut() = Some(msg.id.clone());
    }
    if !msg.id.is_empty() && msg.created_time > 0 {
        state
            .msg_created
            .borrow_mut()
            .insert(msg.id.clone(), msg.created_time);
    }

    let time_txt = format_msg_time(msg.created_time);
    content.append(&bubble);

    if msg.mine {
        let chat = state.current_chat.borrow().clone().unwrap_or_default();
        let count = state
            .read_counts
            .borrow()
            .get(&chat)
            .and_then(|counts| counts.get(&msg.id))
            .copied()
            .max(msg.read_count);
        if let Some(count) = count {
            state
                .read_counts
                .borrow_mut()
                .entry(chat.clone())
                .or_default()
                .insert(msg.id.clone(), count);
        }
        let read = count.is_some_and(|n| n > 0);
        let status = gtk::Label::builder()
            .label(format_outgoing_status(count, msg.created_time))
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .xalign(1.0)
            .justify(gtk::Justification::Right)
            .tooltip_text(if read {
                crate::i18n::t("status_read")
            } else {
                crate::i18n::t("status_sent")
            })
            .css_classes(if read {
                vec![
                    "line-msg-status",
                    "line-msg-status-outgoing",
                    "line-msg-status-read",
                ]
            } else {
                vec!["line-msg-status", "line-msg-status-outgoing"]
            })
            .build();
        if !msg.id.is_empty() {
            state
                .receipt_slots
                .borrow_mut()
                .insert(msg.id.clone(), status.clone());
        }
        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .valign(gtk::Align::End)
            .build();
        outer.append(&gtk::Box::builder().hexpand(true).build());
        if !is_pending && !msg.id.is_empty() {
            append_message_menu(state, &actions, &content, msg);
            outer.append(&actions);
        }
        outer.append(&status);
        outer.append(&content);
    } else {
        // Incoming: bubble + time on the right (mobile style).
        let time = gtk::Label::builder()
            .label(&time_txt)
            .valign(gtk::Align::End)
            .css_classes(["line-msg-time"])
            .build();
        let footer = gtk::Box::builder()
            .spacing(4)
            .valign(gtk::Align::End)
            .build();
        footer.append(&time);
        if !is_pending && !msg.id.is_empty() {
            append_message_menu(state, &footer, &content, msg);
        }
        outer.append(&content);
        outer.append(&footer);
        outer.append(&gtk::Box::builder().hexpand(true).build());
    }

    row_content.append(&outer);
    row.set_child(Some(&row_content));
    let layout_row = LayoutRow {
        id: msg.id.clone(),
        from: msg.from.clone(),
        mine: msg.mine,
        time: msg.created_time,
        day,
        identity,
    };
    {
        let mut layout = state.message_layout.borrow_mut();
        update_layout_row(layout.rows.last(), &layout_row);
        layout.rows.push(layout_row);
    }
    if !msg.id.is_empty() {
        state
            .message_rows
            .borrow_mut()
            .insert(msg.id.clone(), row.clone());
    }
    if is_pending {
        row.add_css_class("line-msg-pending-row");
        state
            .pending_rows
            .borrow_mut()
            .insert(msg.id.clone(), row.clone());
    }
    if live && state.config.borrow().animations {
        row.add_css_class("line-msg-enter");
        if msg.mine {
            row.add_css_class("line-msg-out");
        }
        let row_c = row.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(260), move || {
            row_c.remove_css_class("line-msg-enter");
            row_c.remove_css_class("line-msg-out");
        });
    }
    list.append(&row);
}

pub(super) fn clear_reply(state: &AppState) {
    *state.reply_target.borrow_mut() = None;
    state.reply_bar.set_visible(false);
    state.reply_label.set_text("");
}

pub(super) fn reply_summary(msg: &MessageInfo) -> String {
    let text = msg
        .file_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&msg.text);
    if text.is_empty() {
        format!("[{}]", msg.content_type)
    } else {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(160)
            .collect()
    }
}

pub(super) fn refresh_reply_preview(state: &AppState) {
    if let Some(target) = state.reply_target.borrow().as_ref() {
        state.reply_label.set_text(&crate::i18n::tf(
            "replying_to",
            &[("message", &reply_summary(&target.message))],
        ));
        state.reply_bar.set_visible(true);
    }
}

fn append_message_menu(state: &AppState, footer: &gtk::Box, bubble: &gtk::Box, msg: &MessageInfo) {
    let Some(chat_mid) = state.current_chat.borrow().clone() else {
        return;
    };
    let popover = gtk::Popover::new();
    let actions = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let reply = gtk::Button::with_label(&crate::i18n::t("reply_message"));
    reply.add_css_class("flat");
    actions.append(&reply);
    let unsend = gtk::Button::with_label(&crate::i18n::t("unsend_message"));
    unsend.add_css_class("flat");
    if msg.mine {
        actions.append(&unsend);
    }
    popover.set_child(Some(&actions));
    let menu = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text(crate::i18n::t("message_actions"))
        .css_classes(["flat", "circular"])
        .popover(&popover)
        .build();
    footer.append(&menu);

    let s = state.clone();
    let target = ReplyTarget {
        chat_mid: chat_mid.clone(),
        message: msg.clone(),
    };
    let popup = popover.downgrade();
    reply.connect_clicked(move |_| {
        if let Some(popup) = popup.upgrade() {
            popup.popdown();
        }
        if s.current_chat.borrow().as_deref() != Some(target.chat_mid.as_str())
            || s.unsent_msg_ids.borrow().contains(&target.message.id)
        {
            return;
        }
        *s.reply_target.borrow_mut() = Some(target.clone());
        refresh_reply_preview(&s);
        s.composer.grab_focus();
    });

    let s = state.clone();
    let message_id = msg.id.clone();
    let popup = popover.downgrade();
    unsend.connect_clicked(move |_| {
        if let Some(popup) = popup.upgrade() {
            popup.popdown();
        }
        if s.unsent_msg_ids.borrow().contains(&message_id)
            || s.pending.borrow().values().any(|pending| {
                matches!(pending, Pending::Unsend { message_id: id, .. } if id == &message_id)
            })
        {
            return;
        }
        if !*s.session_ready.borrow() {
            toast(&s, &crate::i18n::t("still_restoring"));
            return;
        }
        match s.sidecar.unsend_message(&chat_mid, &message_id) {
            Ok(id) => {
                s.pending.borrow_mut().insert(
                    id,
                    Pending::Unsend {
                        chat_mid: chat_mid.clone(),
                        message_id: message_id.clone(),
                    },
                );
            }
            Err(error) => toast(
                &s,
                &crate::i18n::tf("unsend_failed", &[("error", &error.to_string())]),
            ),
        }
    });

    // Capture secondary clicks before selectable labels open their copy menu.
    let click = gtk::GestureClick::new();
    click.set_button(3);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_menu = menu.downgrade();
    click.connect_pressed(move |gesture, _, _, _| {
        if let Some(menu) = weak_menu.upgrade() {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            menu.popup();
        }
    });
    bubble.add_controller(click);
}

pub(super) fn apply_message_unsent(state: &AppState, chat_mid: &str, message_id: &str) {
    state
        .unsent_msg_ids
        .borrow_mut()
        .insert(message_id.to_string());
    state.notif_pending.borrow_mut().remove(message_id);
    state.media_ready_paths.borrow_mut().remove(message_id);
    state
        .app
        .withdraw_notification(&format!("line-rust-{message_id}"));
    if state.current_chat.borrow().as_deref() != Some(chat_mid) {
        return;
    }
    let replying = state
        .reply_target
        .borrow()
        .as_ref()
        .is_some_and(|target| target.message.id == message_id);
    if replying {
        clear_reply(state);
    }
    state.message_index.borrow_mut().remove(message_id);
    super::replies::invalidate(state, message_id);
    remove_message_layout(state, message_id);
    let playing = state
        .voice_playback
        .borrow()
        .as_ref()
        .is_some_and(|voice| voice.msg_id == message_id);
    if playing {
        stop_voice_playback(state);
    }
    if let Some(row) = state.message_rows.borrow_mut().remove(message_id) {
        let label = gtk::Label::builder()
            .label(crate::i18n::t("message_unsent"))
            .css_classes(["dim-label"])
            .margin_top(6)
            .margin_bottom(6)
            .build();
        row.set_child(Some(&label));
    }
    state.media_slots.borrow_mut().remove(message_id);
    state.media_msgs.borrow_mut().remove(message_id);
    state.receipt_slots.borrow_mut().remove(message_id);
    state.msg_created.borrow_mut().remove(message_id);
    state
        .media_queue
        .borrow_mut()
        .retain(|(id, _)| id != message_id);
    *state.msg_list_fp.borrow_mut() = None;
}

pub(super) fn format_outgoing_status(count: Option<u64>, created_ms: i64) -> String {
    let t = format_msg_time(created_ms);
    if let Some(count) = count.filter(|n| *n > 0) {
        let read = crate::i18n::tf("status_read_count", &[("n", &count.to_string())]);
        if t.is_empty() {
            read
        } else {
            format!("{read}\n{t}")
        }
    } else {
        t
    }
}

pub(super) fn day_key(ts_ms: i64) -> String {
    let secs = if ts_ms > 1_000_000_000_000 {
        ts_ms / 1000
    } else {
        ts_ms
    };
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

pub(super) fn format_day_separator(ts_ms: i64) -> String {
    let secs = if ts_ms > 1_000_000_000_000 {
        ts_ms / 1000
    } else {
        ts_ms
    };
    let Some(dt) = chrono::DateTime::from_timestamp(secs, 0) else {
        return String::new();
    };
    let local = dt.with_timezone(&chrono::Local);
    let today = chrono::Local::now().date_naive();
    let day = local.date_naive();
    if day == today {
        crate::i18n::t("day_today")
    } else if day == today - chrono::Duration::days(1) {
        crate::i18n::t("day_yesterday")
    } else {
        local.format("%a, %b %-d").to_string()
    }
}

pub(super) fn format_msg_time(ts_ms: i64) -> String {
    if ts_ms <= 0 {
        return String::new();
    }
    let secs = if ts_ms > 1_000_000_000_000 {
        ts_ms / 1000
    } else {
        ts_ms
    };
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%-I:%M %p")
                .to_string()
        })
        .unwrap_or_default()
}

pub(super) fn append_voice_card(
    state: &AppState,
    bubble: &gtk::Box,
    msg: &MessageInfo,
    has_audio: bool,
) {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .css_classes(["line-voice-card"])
        .build();
    let play = gtk::Button::builder()
        .icon_name(if has_audio {
            "media-playback-start-symbolic"
        } else {
            "content-loading-symbolic"
        })
        .sensitive(has_audio)
        .css_classes(["circular", "line-voice-play"])
        .tooltip_text(crate::i18n::t("play_voice"))
        .build();

    let peaks = Rc::new(RefCell::new(placeholder_peaks(36)));
    let progress = Rc::new(RefCell::new(0.0_f32));
    let mine = msg.mine;
    let wave = gtk::DrawingArea::builder()
        .hexpand(true)
        .content_width(160)
        .content_height(32)
        .css_classes(["line-voice-wave"])
        .build();
    {
        let peaks_draw = peaks.clone();
        let progress_draw = progress.clone();
        wave.set_draw_func(move |area, cr, width, height| {
            draw_waveform(
                area,
                cr,
                width as f64,
                height as f64,
                &peaks_draw.borrow(),
                mine,
                Some(*progress_draw.borrow()),
            );
        });
    }

    let duration_ms = if let Some(ms) = msg.duration_ms.filter(|v| *v > 0) {
        ms as u64
    } else if has_audio {
        msg.audio_path
            .as_deref()
            .and_then(|p| ffprobe_duration_ms(std::path::Path::new(p)))
            .unwrap_or(0)
    } else {
        0
    };
    let dur_txt = if duration_ms > 0 {
        format_voice_duration(duration_ms)
    } else if has_audio {
        "--:--".into()
    } else {
        crate::i18n::t("loading_voice")
    };
    let dur = gtk::Label::builder()
        .label(&dur_txt)
        .css_classes(["line-voice-dur"])
        .build();

    if has_audio && let Some(path) = msg.audio_path.clone() {
        let s = state.clone();
        let play_btn = play.clone();
        let wave_btn = wave.clone();
        let dur_btn = dur.clone();
        let progress_btn = progress.clone();
        let msg_id = msg.id.clone();
        let total_label = dur_txt.clone();
        play.connect_clicked(move |_| {
            toggle_voice_playback(VoicePlaybackRequest {
                state: &s,
                msg_id: &msg_id,
                path: &path,
                play_btn: &play_btn,
                wave: &wave_btn,
                duration_label: &dur_btn,
                progress: progress_btn.clone(),
                duration_ms,
                total_label: &total_label,
            });
        });
    }

    let dl = gtk::Button::builder()
        .icon_name("folder-download-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text(crate::i18n::t("media_download"))
        .build();
    {
        let s = state.clone();
        let msg = msg.clone();
        dl.connect_clicked(move |_| {
            request_media_download(&s, &msg, "save_dialog");
        });
    }

    row.append(&play);
    row.append(&wave);
    row.append(&dur);
    row.append(&dl);
    bubble.append(&row);

    if has_audio && let Some(path) = msg.audio_path.clone() {
        let wave2 = wave.clone();
        let peaks2 = peaks.clone();
        let (tx, rx) = async_channel::bounded::<Vec<f32>>(1);
        std::thread::spawn(move || {
            let extracted = extract_audio_peaks(std::path::Path::new(&path), 36)
                .unwrap_or_else(|| placeholder_peaks(36));
            let _ = tx.send_blocking(extracted);
        });
        glib::spawn_future_local(async move {
            if let Ok(extracted) = rx.recv().await {
                *peaks2.borrow_mut() = extracted;
                wave2.queue_draw();
            }
        });
    }
}

pub(super) struct VoicePlaybackRequest<'a> {
    state: &'a AppState,
    msg_id: &'a str,
    path: &'a str,
    play_btn: &'a gtk::Button,
    wave: &'a gtk::DrawingArea,
    duration_label: &'a gtk::Label,
    progress: Rc<RefCell<f32>>,
    duration_ms: u64,
    total_label: &'a str,
}

pub(super) fn toggle_voice_playback(request: VoicePlaybackRequest<'_>) {
    let VoicePlaybackRequest {
        state,
        msg_id,
        path,
        play_btn,
        wave,
        duration_label,
        progress,
        duration_ms,
        total_label,
    } = request;
    // Second press on the same bubble stops playback.
    // Drop the borrow before stop_voice_playback (it needs borrow_mut).
    let same_bubble = state
        .voice_playback
        .borrow()
        .as_ref()
        .is_some_and(|cur| cur.msg_id == msg_id);
    if same_bubble {
        stop_voice_playback(state);
        return;
    }
    stop_voice_playback(state);

    let child = match spawn_audio_player(path, &state.config.borrow().audio_output, 1.0) {
        Ok(c) => c,
        Err(e) => {
            toast(
                state,
                &crate::i18n::tf("voice_play_failed", &[("error", &e)]),
            );
            return;
        }
    };

    play_btn.set_icon_name("media-playback-stop-symbolic");
    play_btn.set_tooltip_text(Some(&crate::i18n::t("stop_voice")));
    play_btn.add_css_class("playing");
    wave.add_css_class("playing");
    *progress.borrow_mut() = 0.0;
    wave.queue_draw();

    let playback = VoicePlayback {
        child,
        msg_id: msg_id.to_string(),
        play_btn: play_btn.clone(),
        wave: wave.clone(),
        dur: duration_label.clone(),
        duration_ms: duration_ms.max(500),
        started: std::time::Instant::now(),
        progress: progress.clone(),
        total_label: total_label.to_string(),
        tick: None,
    };
    *state.voice_playback.borrow_mut() = Some(playback);

    let s = state.clone();
    let tick = glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        let mut slot = s.voice_playback.borrow_mut();
        let Some(pb) = slot.as_mut() else {
            return glib::ControlFlow::Break;
        };
        // Finished?
        match pb.child.try_wait() {
            Ok(Some(_)) => {
                drop(slot);
                stop_voice_playback(&s);
                return glib::ControlFlow::Break;
            }
            Err(_) => {
                drop(slot);
                stop_voice_playback(&s);
                return glib::ControlFlow::Break;
            }
            Ok(None) => {}
        }
        let elapsed = pb.started.elapsed().as_millis() as u64;
        let frac = (elapsed as f32 / pb.duration_ms as f32).clamp(0.0, 1.0);
        *pb.progress.borrow_mut() = frac;
        pb.wave.queue_draw();
        let remain = pb.duration_ms.saturating_sub(elapsed);
        pb.dur.set_text(&format_voice_duration(remain.max(1)));
        if frac >= 1.0 {
            // Duration reached but process still alive — keep waiting for exit.
        }
        glib::ControlFlow::Continue
    });
    if let Some(pb) = state.voice_playback.borrow_mut().as_mut() {
        pb.tick = Some(tick);
    }
}

pub(super) fn stop_voice_playback(state: &AppState) {
    let Some(mut pb) = state.voice_playback.borrow_mut().take() else {
        return;
    };
    if let Some(tick) = pb.tick.take() {
        tick.remove();
    }
    let _ = pb.child.kill();
    let _ = pb.child.wait();
    *pb.progress.borrow_mut() = 0.0;
    pb.play_btn.set_icon_name("media-playback-start-symbolic");
    pb.play_btn
        .set_tooltip_text(Some(&crate::i18n::t("play_voice")));
    pb.play_btn.remove_css_class("playing");
    pb.wave.remove_css_class("playing");
    pb.wave.queue_draw();
    pb.dur.set_text(&pb.total_label);
}

pub(super) fn append_file_card(state: &AppState, bubble: &gtk::Box, msg: &MessageInfo) {
    let name = msg
        .file_name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if msg.text.is_empty() || msg.text == "[File]" {
                crate::i18n::t("preview_file")
            } else {
                msg.text.clone()
            }
        });
    let kind = viewer_kind_for("FILE", "", &name);
    let icon_name = match kind {
        ViewerKind::Pdf => "application-pdf-symbolic",
        ViewerKind::Text => "text-x-generic-symbolic",
        ViewerKind::Image => "image-x-generic-symbolic",
        ViewerKind::Video => "video-x-generic-symbolic",
        ViewerKind::Audio => "audio-x-generic-symbolic",
        ViewerKind::Generic => "folder-download-symbolic",
    };
    let row = gtk::Button::builder()
        .css_classes(["flat", "line-file-card"])
        .tooltip_text(crate::i18n::t("media_open_file"))
        .halign(gtk::Align::Fill)
        .build();
    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .build();
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(22);
    let label = gtk::Label::builder()
        .label(&name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(28)
        .hexpand(true)
        .css_classes(["line-bubble-text"])
        .build();
    let chevron = gtk::Image::from_icon_name("go-next-symbolic");
    inner.append(&icon);
    inner.append(&label);
    inner.append(&chevron);
    row.set_child(Some(&inner));
    let s = state.clone();
    let msg = msg.clone();
    row.connect_clicked(move |_| {
        request_media_download(&s, &msg, "open_viewer");
    });
    bubble.append(&row);
}

pub(super) fn format_voice_duration(ms: u64) -> String {
    let secs = ((ms + 500) / 1000).max(1);
    let m = secs / 60;
    let s = secs % 60;
    format!("{m}:{s:02}")
}

pub(super) fn placeholder_peaks(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            0.18 + 0.12 * ((t * std::f32::consts::PI * 4.0).sin()).abs()
        })
        .collect()
}

pub(super) fn draw_waveform(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: f64,
    height: f64,
    peaks: &[f32],
    mine: bool,
    progress: Option<f32>,
) {
    if width <= 1.0 || height <= 1.0 || peaks.is_empty() {
        return;
    }
    // Named theme colors still require StyleContext::lookup_color (GTK < 4.10 path).
    #[allow(deprecated)]
    let accent = area
        .style_context()
        .lookup_color("accent_bg_color")
        .or_else(|| area.style_context().lookup_color("accent_color"));
    let muted = area.color();
    let (base_r, base_g, base_b) = if mine {
        match accent {
            Some(c) => (
                f64::from(c.red()),
                f64::from(c.green()),
                f64::from(c.blue()),
            ),
            None => (
                f64::from(muted.red()),
                f64::from(muted.green()),
                f64::from(muted.blue()),
            ),
        }
    } else {
        (
            f64::from(muted.red()),
            f64::from(muted.green()),
            f64::from(muted.blue()),
        )
    };
    let n = peaks.len() as f64;
    let gap = 2.0_f64.min((width / n) * 0.35).max(1.0);
    // Spread bars across the full width (do not cap bar width).
    let bar_w = ((width - gap * (n - 1.0)) / n).max(1.0);
    let mid = height * 0.5;
    let max_h = (height * 0.42).max(3.0);
    let prog = progress.unwrap_or(-1.0);
    for (i, p) in peaks.iter().enumerate() {
        let amp = (*p).clamp(0.08, 1.0) as f64;
        let h = max_h * amp;
        let x = i as f64 * (bar_w + gap);
        let t = (i as f32 + 0.5) / peaks.len() as f32;
        let played = prog >= 0.0 && t <= prog;
        let alpha = if played {
            1.0
        } else if prog >= 0.0 {
            0.35
        } else if mine {
            0.95
        } else {
            0.75
        };
        cr.set_source_rgba(base_r, base_g, base_b, alpha);
        cr.rectangle(x, mid - h, bar_w, h * 2.0);
        let _ = cr.fill();
    }
}

/// How many live spectrum bars fit the record-wave width (~3px bar + gap).
pub(super) fn spectrum_bar_count(width: f64) -> usize {
    let slot = 5.0_f64;
    ((width / slot).round() as usize).clamp(32, 160)
}

pub(super) fn extract_audio_peaks(path: &std::path::Path, bars: usize) -> Option<Vec<f32>> {
    if bars == 0 || !path.exists() {
        return None;
    }
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            path.to_str()?,
            "-ac",
            "1",
            "-ar",
            "8000",
            "-f",
            "s16le",
            "-",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() < 4 {
        return None;
    }
    let pcm = out.stdout;
    let n = pcm.len() / 2;
    if n == 0 {
        return None;
    }
    let mut peaks = vec![0.0f32; bars];
    for i in 0..n {
        let s = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]) as f32 / 32768.0;
        let bi = (i * bars) / n;
        if bi < bars {
            peaks[bi] = peaks[bi].max(s.abs());
        }
    }
    let max = peaks.iter().cloned().fold(0.05f32, f32::max);
    Some(
        peaks
            .into_iter()
            .map(|p| (p / max).clamp(0.08, 1.0))
            .collect(),
    )
}

pub(super) fn wav_tail_level(path: &std::path::Path) -> f32 {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0.08;
    };
    let Ok(meta) = file.metadata() else {
        return 0.08;
    };
    let len = meta.len() as usize;
    if len < 44 + 512 {
        return 0.08;
    }
    let bytes = (1024 * 2).min(len.saturating_sub(44));
    let start = (len - bytes) as u64;
    if file.seek(SeekFrom::Start(start)).is_err() {
        return 0.08;
    }
    let mut buf = vec![0u8; bytes];
    if file.read_exact(&mut buf).is_err() {
        return 0.08;
    }
    let samples = buf.len() / 2;
    if samples == 0 {
        return 0.08;
    }
    let mut sum = 0.0f64;
    for i in 0..samples {
        let s = i16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]) as f64 / 32768.0;
        sum += s * s;
    }
    let rms = (sum / samples as f64).sqrt() as f32;
    (rms * 4.5).clamp(0.08, 1.0)
}

pub(super) fn wire_record_wave(state: &AppState) {
    let levels = state.recording_levels.clone();
    state
        .record_wave
        .set_draw_func(move |area, cr, width, height| {
            draw_waveform(
                area,
                cr,
                width as f64,
                height as f64,
                &levels.borrow(),
                true,
                None,
            );
        });

    // Custom DrawingAreas do not auto-repaint when the color scheme flips.
    let root = state.window.clone().upcast::<gtk::Widget>();
    let record_wave = state.record_wave.clone();
    libadwaita::StyleManager::default().connect_dark_notify(move |_| {
        record_wave.queue_draw();
        queue_draw_voice_waves(&root);
    });
}

pub(super) fn queue_draw_voice_waves(widget: &gtk::Widget) {
    if let Ok(area) = widget.clone().downcast::<gtk::DrawingArea>()
        && (area.has_css_class("line-voice-wave") || area.has_css_class("line-record-wave"))
    {
        area.queue_draw();
    }
    let mut child = widget.first_child();
    while let Some(w) = child {
        queue_draw_voice_waves(&w);
        child = w.next_sibling();
    }
}

pub(super) fn attach_audio_to_slot(state: &AppState, message_id: &str, path: &str) {
    let Some(bubble) = state.media_slots.borrow().get(message_id).cloned() else {
        return;
    };
    while let Some(child) = bubble.first_child() {
        bubble.remove(&child);
    }
    let duration_ms = ffprobe_duration_ms(std::path::Path::new(path)).map(|v| v as i64);
    let msg = MessageInfo {
        mentioned_mids: Vec::new(),
        id: message_id.to_string(),
        text: "Voice message".into(),
        related_message_id: None,
        from: String::new(),
        to: String::new(),
        mine: true,
        read_count: None,
        created_time: 0,
        content_type: "AUDIO".into(),
        image_path: None,
        audio_path: Some(path.to_string()),
        file_name: None,
        file_path: Some(path.to_string()),
        duration_ms,
        flex: None,
        emojis: Vec::new(),
    };
    append_voice_card(state, &bubble, &msg, true);
}

pub(super) fn play_audio_file(path: &str, output_sink: &str) -> Result<(), String> {
    spawn_audio_player(path, output_sink, 1.0).map(|mut child| {
        // Detach: notifications / one-shot viewers don't track the process.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    })
}

pub(super) fn spawn_audio_player(
    path: &str,
    output_sink: &str,
    gain: f64,
) -> Result<std::process::Child, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() < 256 {
        return Err("file too small / corrupt".into());
    }
    let sink = if output_sink.is_empty() || output_sink == "default" {
        None
    } else {
        Some(output_sink.to_string())
    };
    let gain = gain.clamp(0.0, 2.5);
    let mpv_vol = (gain * 100.0).round().clamp(0.0, 250.0);

    let mut mpv = std::process::Command::new("mpv");
    mpv.args(["--no-video", "--really-quiet"]);
    mpv.arg(format!("--volume={mpv_vol}"));
    if let Some(ref s) = sink {
        mpv.arg(format!("--audio-device=pulse/{s}"));
    }
    mpv.arg(path);
    mpv.stdout(std::process::Stdio::null());
    mpv.stderr(std::process::Stdio::null());
    if let Ok(child) = mpv.spawn() {
        return Ok(child);
    }

    let mut ff = std::process::Command::new("ffplay");
    ff.args(["-nodisp", "-autoexit", "-loglevel", "quiet"]);
    if gain != 1.0 {
        ff.args(["-af", &format!("volume={gain}")]);
    }
    if let Some(ref s) = sink {
        ff.args(["-audiodevice", s]);
    }
    ff.arg(path);
    ff.stdout(std::process::Stdio::null());
    ff.stderr(std::process::Stdio::null());
    if let Ok(child) = ff.spawn() {
        return Ok(child);
    }

    if (gain - 1.0).abs() < 0.01 {
        let mut paplay = std::process::Command::new("paplay");
        if let Some(ref s) = sink {
            paplay.args(["--device", s]);
        }
        paplay.arg(path);
        paplay.stdout(std::process::Stdio::null());
        paplay.stderr(std::process::Stdio::null());
        if let Ok(child) = paplay.spawn() {
            return Ok(child);
        }
    }

    Err("no audio player (mpv/ffplay)".into())
}

pub(super) fn filter_chats(state: &AppState, query: &str) {
    let q = query.trim().to_lowercase();
    let chats = state.chats.borrow();
    let mut idx = 0;
    while let Some(row) = state.chat_list.row_at_index(idx) {
        let visible = if q.is_empty() {
            true
        } else {
            chats
                .get(idx as usize)
                .map(|c| {
                    c.name.to_lowercase().contains(&q) || c.preview.to_lowercase().contains(&q)
                })
                .unwrap_or(true)
        };
        let hidden = chats
            .get(idx as usize)
            .is_some_and(|chat| chat_is_hidden(state, &chat.mid));
        row.set_visible(visible && !hidden);
        idx += 1;
    }
}

pub(super) fn append_flex_card(
    state: &AppState,
    bubble: &gtk::Box,
    msg: &MessageInfo,
    flex: &crate::protocol::FlexInfo,
) {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["line-flex-card"])
        .build();

    let title = if flex.alt_text.is_empty() {
        msg.text.clone()
    } else {
        flex.alt_text.clone()
    };
    if !title.is_empty() {
        card.append(
            &gtk::Label::builder()
                .label(&title)
                .xalign(0.0)
                .wrap(true)
                .max_width_chars(40)
                .css_classes(["line-flex-title"])
                .build(),
        );
    }
    for t in flex.texts.iter().take(6) {
        if *t == title {
            continue;
        }
        card.append(
            &gtk::Label::builder()
                .label(t)
                .xalign(0.0)
                .wrap(true)
                .max_width_chars(40)
                .css_classes(["dim-label"])
                .build(),
        );
    }

    for action in flex.actions.iter().take(12) {
        let btn = gtk::Button::builder()
            .label(&action.label)
            .hexpand(true)
            .css_classes(["line-flex-action"])
            .build();
        let state = state.clone();
        let action = action.clone();
        let msg_id = msg.id.clone();
        let chat = state.current_chat.borrow().clone().unwrap_or_default();
        btn.connect_clicked(move |_| {
            handle_flex_action(&state, &chat, &msg_id, &action);
        });
        card.append(&btn);
    }

    bubble.append(&card);
}

pub(super) fn handle_flex_action(
    state: &AppState,
    chat_mid: &str,
    message_id: &str,
    action: &FlexAction,
) {
    let kind = action.kind.to_ascii_lowercase();
    if (kind == "uri" || kind == "url")
        && let Some(uri) = action.uri.as_deref()
    {
        open_uri(uri);
        return;
    }
    if kind == "message" {
        let text = action.data.clone().unwrap_or_else(|| action.label.clone());
        if let Err(e) = state.sidecar.send_message(chat_mid, &text, None) {
            toast(
                state,
                &crate::i18n::tf("send_failed", &[("error", &e.to_string())]),
            );
        }
        return;
    }
    // postback / default
    let data = action.data.clone().unwrap_or_else(|| action.label.clone());
    match state
        .sidecar
        .send_postback(chat_mid, message_id, &data, action.uri.as_deref())
    {
        Ok(_) => toast(
            state,
            &crate::i18n::tf("action_sent", &[("label", &action.label)]),
        ),
        Err(e) => toast(
            state,
            &crate::i18n::tf("action_failed", &[("error", &e.to_string())]),
        ),
    }
}

pub(super) fn append_link_chips(_state: &AppState, bubble: &gtk::Box, text: &str) {
    for url in extract_urls(text).into_iter().take(4) {
        if youtube_id(&url).is_some() {
            let box_ = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(4)
                .css_classes(["line-flex-card"])
                .build();
            box_.append(
                &gtk::Label::builder()
                    .label(crate::i18n::t("youtube_video"))
                    .xalign(0.0)
                    .css_classes(["line-flex-title"])
                    .build(),
            );
            let btn = gtk::LinkButton::builder()
                .label(crate::i18n::t("open_play_browser"))
                .uri(&url)
                .css_classes(["line-link-chip"])
                .build();
            box_.append(&btn);
            bubble.append(&box_);
            continue;
        }
        let btn = gtk::LinkButton::builder()
            .label(if url.len() > 48 {
                format!("{}…", &url[..45])
            } else {
                url.clone()
            })
            .uri(&url)
            .css_classes(["line-link-chip"])
            .build();
        bubble.append(&btn);
    }
}

pub(super) fn clear_messages(state: &AppState) {
    senders::clear_slots(state);
    *state.message_layout.borrow_mut() = MessageLayout::default();
    *state.reply_ui.borrow_mut() = super::replies::ReplyUi::default();
    state.message_index.borrow_mut().clear();
    if state.current_chat.borrow().is_none() {
        clear_reply(state);
    }
    *state.message_generation.borrow_mut() += 1;
    state.message_rows.borrow_mut().clear();
    stop_voice_playback(state);
    state.message_list.clear();
    state.media_slots.borrow_mut().clear();
    state.media_msgs.borrow_mut().clear();
    state.receipt_slots.borrow_mut().clear();
    state.msg_created.borrow_mut().clear();
    state.pending_rows.borrow_mut().clear();
    state.seen_msg_ids.borrow_mut().clear();
    *state.last_incoming_id.borrow_mut() = None;
    state.media_queue.borrow_mut().clear();
    *state.media_pumping.borrow_mut() = false;
    state.memory_visible.borrow_mut().clear();
    let _ = state.sidecar.visible_media("", vec![]);
    state
        .media_ready_paths
        .borrow_mut()
        .retain(|_, path| !crate::memory_media::is_ram_path(path));
    *state.new_sep_row.borrow_mut() = None;
    *state.pending_new_below.borrow_mut() = 0;
    state.jump_banner.set_reveal_child(false);
    // Keep media_ready_paths across rebuilds so late slots can still attach.
}

pub(super) fn message_tracks_media(msg: &MessageInfo) -> bool {
    let ct = msg.content_type.to_ascii_uppercase();
    matches!(
        ct.as_str(),
        "IMAGE" | "VIDEO" | "AUDIO" | "FILE" | "STICKER"
    ) || msg.image_path.is_some()
        || msg.audio_path.is_some()
        || msg.file_path.is_some()
        || msg.flex.is_some()
}

pub(super) fn set_side_state(state: &AppState, name: &str, empty_text: Option<&str>) {
    if let Some(text) = empty_text {
        state.side_empty.set_text(text);
    }
    let loading = name == "loading";
    state.side_spinner.set_spinning(loading);
    state.side_spinner.set_visible(loading);
    state.side_stack.set_visible_child_name(name);
}

pub(super) fn set_msg_state(state: &AppState, name: &str, empty_text: Option<&str>) {
    if let Some(text) = empty_text {
        state.msg_empty.set_text(text);
    }
    if name == "loading" {
        state.msg_spinner.set_spinning(true);
    }
    state.msg_stack.set_visible_child_name(name);
}

pub(super) fn preview_body_ui(msg: &MessageInfo) -> String {
    let text = localize_preview(line_rust_core::message::preview(
        &msg.content_type,
        &msg.text,
    ));
    if text.chars().count() > 64 {
        format!("{}…", text.chars().take(63).collect::<String>())
    } else {
        text
    }
}

pub(super) fn update_chat_preview(state: &AppState, mid: &str, msg: &MessageInfo) {
    let mut chats = state.chats.borrow_mut();
    let Some(chat) = chats.iter_mut().find(|chat| chat.mid == mid) else {
        return;
    };
    // A cached page must not replace the preview of a newer live message.
    if msg.created_time < chat.last_activity && !chat.preview.trim().is_empty() {
        return;
    }
    chat.preview = preview_body_ui(msg);
    chat.last_activity = chat.last_activity.max(msg.created_time);
    if let Some(label) = state.chat_previews.borrow().get(mid) {
        label.set_text(&chat.preview);
    }
}

/// Rewrite cached English (or mismatched) preview prefixes into the active UI language.
pub(super) fn localize_preview(preview: &str) -> String {
    let mut s = preview.to_string();
    let pairs = [
        ("You: ", "you"),
        ("They: ", "they"),
        ("คุณ: ", "you"),
        ("เขา: ", "they"),
    ];
    for (prefix, key) in pairs {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = format!("{}: {}", crate::i18n::t(key), rest);
            break;
        }
    }
    // Localized media placeholders that may appear after the who-prefix.
    let replacements = [
        ("Photo", "preview_photo"),
        ("[Image]", "preview_photo"),
        ("Video", "preview_video"),
        ("[Video]", "preview_video"),
        ("Sticker", "preview_sticker"),
        ("[Sticker]", "preview_sticker"),
        ("Voice message", "voice_message"),
        ("File", "preview_file"),
        ("[File]", "preview_file"),
        ("Flex message", "preview_flex"),
        ("Message", "preview_message"),
        ("Tap to open", "tap_open"),
        ("Loading last message…", "tap_open"),
        ("Loading last message...", "tap_open"),
        ("No recent messages", "no_recent"),
        ("รูปภาพ", "preview_photo"),
        ("วิดีโอ", "preview_video"),
        ("สติกเกอร์", "preview_sticker"),
        ("ข้อความเสียง", "voice_message"),
        ("ไฟล์", "preview_file"),
        ("ข้อความ Flex", "preview_flex"),
        ("ข้อความ", "preview_message"),
        ("แตะเพื่อเปิด", "tap_open"),
        ("กำลังโหลดข้อความล่าสุด…", "tap_open"),
        ("ไม่มีข้อความล่าสุด", "no_recent"),
    ];
    if let Some((who, body)) = s.split_once(": ") {
        let mut body = body.to_string();
        for (from, key) in replacements {
            if body == from {
                body = crate::i18n::t(key);
                break;
            }
        }
        return format!("{who}: {body}");
    }
    for (from, key) in replacements {
        if s == from {
            return crate::i18n::t(key);
        }
    }
    s
}

pub(super) fn snap_adj_to_bottom(adj: &gtk::Adjustment) {
    let target = (adj.upper() - adj.page_size()).max(0.0);
    if (adj.value() - target).abs() > 0.5 {
        adj.set_value(target);
    }
}

pub(super) fn scroll_last_row_into_view(state: &AppState) {
    state.message_list.scroll_to_end();
    snap_adj_to_bottom(&state.message_scroll.vadjustment());
}

pub(super) fn scroll_messages_to_end(state: &AppState) {
    if !*state.stick_bottom.borrow() {
        return;
    }
    // Supersede any previous pin chain — overlapping clears were leaving stick=false
    // and stopping one message short of the latest.
    let pin_gen = {
        let mut g = state.scroll_pin_gen.borrow_mut();
        *g = g.saturating_add(1);
        *g
    };
    *state.scroll_pinning.borrow_mut() = true;
    state.message_list.queue_allocate();

    let state_c = state.clone();
    let step = Rc::new(RefCell::new(None::<Rc<dyn Fn(u32)>>));
    let step_fn = {
        let step = step.clone();
        let state_c = state_c.clone();
        Rc::new(move |attempt: u32| {
            if *state_c.scroll_pin_gen.borrow() != pin_gen {
                return;
            }
            if !*state_c.stick_bottom.borrow() {
                *state_c.scroll_pinning.borrow_mut() = false;
                return;
            }
            *state_c.scroll_pinning.borrow_mut() = true;
            scroll_last_row_into_view(&state_c);
            snap_adj_to_bottom(&state_c.message_scroll.vadjustment());

            let adj = state_c.message_scroll.vadjustment();
            let target = (adj.upper() - adj.page_size()).max(0.0);
            let at_bottom = adj.value() + 1.0 >= target;
            // Keep pinning across delayed ListBox allocation (~0.5–0.7s).
            // Require a few consecutive at-bottom frames so we don't stop one row short.
            if attempt >= 48 {
                *state_c.scroll_pinning.borrow_mut() = false;
                if *state_c.stick_bottom.borrow() {
                    *state_c.scroll_pinning.borrow_mut() = true;
                    scroll_last_row_into_view(&state_c);
                    snap_adj_to_bottom(&adj);
                    *state_c.scroll_pinning.borrow_mut() = false;
                }
                return;
            }
            if at_bottom && attempt >= 12 {
                // One more settle pass on the next ticks, then release.
                if attempt >= 18 {
                    *state_c.scroll_pinning.borrow_mut() = false;
                    return;
                }
            }
            let step = step.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(16), move || {
                if let Some(f) = step.borrow().as_ref() {
                    f(attempt + 1);
                }
            });
        }) as Rc<dyn Fn(u32)>
    };
    *step.borrow_mut() = Some(step_fn.clone());
    step_fn(0);
}

pub(super) fn pin_messages_to_latest(state: &AppState) {
    *state.stick_bottom.borrow_mut() = true;
    scroll_messages_to_end(state);
}

pub(super) fn state_chat_open(state: &AppState) -> bool {
    state.current_chat.borrow().is_some()
        && state.msg_stack.visible_child_name().as_deref() == Some("list")
}

pub(super) fn ensure_new_message_separator(state: &AppState) {
    if state.new_sep_row.borrow().is_some() {
        return;
    }
    let sep_row = gtk::ListBoxRow::builder()
        .selectable(false)
        .activatable(false)
        .css_classes(["line-new-sep-row"])
        .build();
    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .css_classes(["line-new-sep"])
        .build();
    let line = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .css_classes(["line-new-sep-line"])
        .build();
    line.set_height_request(1);
    let badge = gtk::Label::builder()
        .label(crate::i18n::t("new_messages_badge"))
        .css_classes(["line-new-sep-badge"])
        .valign(gtk::Align::Center)
        .build();
    row_box.append(&line);
    row_box.append(&badge);
    sep_row.set_child(Some(&row_box));
    state.message_list.append(&sep_row);
    *state.new_sep_row.borrow_mut() = Some(sep_row);
}

pub(super) fn dismiss_new_marker(state: &AppState) {
    if let Some(row) = state.new_sep_row.borrow_mut().take() {
        state.message_list.remove(&row);
    }
    *state.pending_new_below.borrow_mut() = 0;
    update_jump_banner(state);
}

pub(super) fn update_jump_banner(state: &AppState) {
    let n = *state.pending_new_below.borrow();
    let show = n > 0 && !*state.stick_bottom.borrow();
    if show {
        let label = if n == 1 {
            crate::i18n::t("jump_new_one")
        } else {
            crate::i18n::tf("jump_new_n", &[("n", &n.to_string())])
        };
        state.jump_banner_label.set_text(&label);
    }
    state.jump_banner.set_reveal_child(show);
}

pub(super) fn jump_to_latest(state: &AppState) {
    *state.pending_new_below.borrow_mut() = 0;
    update_jump_banner(state);
    pin_messages_to_latest(state);
    if let Some(mid) = state.current_chat.borrow().clone() {
        mark_chat_read(state, &mid);
        clear_unread_badge(state, &mid);
    }
}

#[cfg(test)]
mod receipt_tests {
    #[test]
    fn message_grouping_uses_adjacent_messages_with_an_inclusive_five_minute_limit() {
        use super::message_boundary;
        let time = 1_800_000_000_000;
        assert!(!message_boundary(None, "a", false, time));
        for delta in [0, 299_999, 300_000] {
            assert!(message_boundary(
                Some(("a", false, time)),
                "a",
                false,
                time + delta
            ));
        }
        for delta in [-1, 300_001, 1_200_000] {
            assert!(!message_boundary(
                Some(("a", false, time)),
                "a",
                false,
                time + delta
            ));
        }
        assert!(!message_boundary(
            Some(("a", false, time)),
            "b",
            false,
            time + 1000
        ));
        assert!(!message_boundary(
            Some(("a", false, time)),
            "a",
            true,
            time + 1000
        ));
        assert!(!message_boundary(
            Some(("", false, time)),
            "",
            false,
            time + 1000
        ));
        assert!(!message_boundary(Some(("a", false, 0)), "a", false, time));
        assert!(message_boundary(
            Some(("a", false, time / 1000)),
            "a",
            false,
            time / 1000 + 300
        ));
        // A long chain stays grouped: compare each pair, not against its first row.
        for index in 1..10 {
            assert!(message_boundary(
                Some(("a", false, time + (index - 1) * 240_000)),
                "a",
                false,
                time + index * 240_000
            ));
        }
    }

    #[test]
    #[ignore = "requires a GTK display; run under xvfb-run"]
    fn gtk_sender_layout_repairs_prepend_and_removed_message_boundaries() {
        use super::*;
        gtk::init().unwrap();
        let make = |id: &str, sender: &str, time| LayoutRow {
            id: id.into(),
            from: sender.into(),
            mine: false,
            time,
            day: gtk::Label::new(None),
            identity: Some((
                gtk::Box::new(gtk::Orientation::Horizontal, 0).upcast(),
                gtk::Label::new(None).upcast(),
            )),
        };
        let time = 1_800_000_000_000;
        let oldest = make("1", "a", time);
        let first = make("2", "a", time + 1000);
        update_layout_row(None, &first);
        assert!(first.identity.as_ref().unwrap().0.is_visible());
        update_layout_row(Some(&oldest), &first);
        assert!(!first.identity.as_ref().unwrap().0.is_visible());
        assert!(!first.identity.as_ref().unwrap().1.is_visible());
        assert!(!first.day.is_visible());
        let different = make("3", "b", time + 2000);
        let next = make("4", "a", time + 3000);
        update_layout_row(Some(&different), &next);
        assert!(next.identity.as_ref().unwrap().0.is_visible());
        update_layout_row(Some(&first), &next);
        assert!(!next.identity.as_ref().unwrap().0.is_visible());
        let later = make("5", "a", time + 303_001);
        update_layout_row(Some(&next), &later);
        assert!(later.identity.as_ref().unwrap().0.is_visible());
    }
    use super::*;

    #[test]
    fn outgoing_read_count_is_above_time_and_unread_shows_only_time() {
        let timestamp = 1_800_000_000_000;
        let time = format_msg_time(timestamp);
        assert_eq!(format_outgoing_status(None, timestamp), time);
        assert_eq!(format_outgoing_status(Some(0), timestamp), time);
        for count in [1, 3] {
            let read = crate::i18n::tf("status_read_count", &[("n", &count.to_string())]);
            assert_eq!(
                format_outgoing_status(Some(count), timestamp),
                format!("{read}\n{time}")
            );
        }
        assert_eq!(
            format_outgoing_status(Some(3), 0),
            crate::i18n::tf("status_read_count", &[("n", "3")])
        );
        assert_eq!(format_outgoing_status(None, 0), "");
    }
}
