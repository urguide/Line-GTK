use super::*;
use line_rust_core::mentions::{self as model, Member, Mention};
use std::cell::Cell;

pub(super) struct MentionUi {
    panel: gtk::Box,
    list: gtk::ListBox,
    scroll: gtk::ScrolledWindow,
    status: gtk::Label,
    retry: gtk::Button,
    members: RefCell<Option<Vec<Member>>>,
    filtered: RefCell<Vec<Member>>,
    pub selected: RefCell<Vec<Mention>>,
    generation: Cell<u64>,
    loading: Cell<bool>,
    failed: Cell<bool>,
    queued: Cell<bool>,
    preediting: Cell<bool>,
    dismissed: RefCell<Option<(String, i32)>>,
}

impl MentionUi {
    pub fn new() -> Rc<Self> {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .activate_on_single_click(true)
            .focusable(false)
            .css_classes(["boxed-list"])
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(220)
            .propagate_natural_height(true)
            .child(&list)
            .build();
        let panel = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .visible(false)
            .build();
        let status = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let retry = gtk::Button::builder().halign(gtk::Align::Start).build();
        panel.append(&status);
        panel.append(&scroll);
        panel.append(&retry);
        Rc::new(Self {
            panel,
            list,
            scroll,
            status,
            retry,
            members: RefCell::new(None),
            filtered: RefCell::new(Vec::new()),
            selected: RefCell::new(Vec::new()),
            generation: Cell::new(0),
            loading: Cell::new(false),
            failed: Cell::new(false),
            queued: Cell::new(false),
            preediting: Cell::new(false),
            dismissed: RefCell::new(None),
        })
    }
}

pub(super) fn wire(state: &AppState) {
    let ui = &state.mention_ui;
    state
        .conversation
        .insert_child_after(&ui.panel, Some(&state.reply_bar));
    track_edits(ui, &state.composer);
    let s = state.clone();
    state.composer.connect_changed(move |_| schedule(&s));
    let s = state.clone();
    state
        .composer
        .connect_cursor_position_notify(move |_| schedule(&s));
    let s = state.clone();
    state
        .composer
        .connect_selection_bound_notify(move |_| schedule(&s));
    wire_selection(state);
}

fn track_edits(ui: &Rc<MentionUi>, composer: &gtk::Entry) {
    // EntryBuffer emits exact edits, including paste, replacement and IME commits.
    let selected = ui.clone();
    composer
        .buffer()
        .connect_local("inserted-text", false, move |args| {
            let position = args[1].get::<u32>().unwrap() as usize;
            let count = args[3].get::<u32>().unwrap() as usize;
            model::inserted(&mut selected.selected.borrow_mut(), position, count);
            None
        });
    let selected = ui.clone();
    composer
        .buffer()
        .connect_local("deleted-text", false, move |args| {
            let position = args[1].get::<u32>().unwrap() as usize;
            let count = args[2].get::<u32>().unwrap() as usize;
            model::deleted(&mut selected.selected.borrow_mut(), position, count);
            None
        });
    if let Some(text) = composer.delegate().and_downcast::<gtk::Text>() {
        let ui = ui.clone();
        text.connect_preedit_changed(move |_, preedit| ui.preediting.set(!preedit.is_empty()));
    }
}

fn wire_selection(state: &AppState) {
    let ui = &state.mention_ui;
    let s = state.clone();
    ui.list
        .connect_row_activated(move |_, row| choose(&s, row.index() as usize));
    let s = state.clone();
    ui.retry.connect_clicked(move |_| {
        s.mention_ui.failed.set(false);
        refresh(&s);
        s.composer.grab_focus();
    });
    let s = state.clone();
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        // Do not steal candidate navigation / confirmation from Chinese IMEs.
        if s.mention_ui.preediting.get() {
            return glib::Propagation::Proceed;
        }
        if matches!(
            key,
            gdk::Key::Up | gdk::Key::Down | gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Tab
        ) {
            refresh(&s);
        }
        if !s.mention_ui.panel.is_visible()
            || modifiers.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            )
        {
            return glib::Propagation::Proceed;
        }
        match key {
            gdk::Key::Up | gdk::Key::Down => {
                let ui = &s.mention_ui;
                let count = ui.filtered.borrow().len() as i32;
                if count > 0 {
                    let index = ui.list.selected_row().map_or(0, |r| r.index());
                    let next = (index + if key == gdk::Key::Up { -1 } else { 1 }).rem_euclid(count);
                    if let Some(row) = ui.list.row_at_index(next) {
                        ui.list.select_row(Some(&row));
                        if let Some(point) =
                            row.compute_point(&ui.list, &gtk::graphene::Point::new(0.0, 0.0))
                        {
                            ui.scroll.vadjustment().clamp_page(
                                point.y() as f64,
                                (point.y() + row.height() as f32) as f64,
                            );
                        }
                    }
                }
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Tab
                if !modifiers.contains(gdk::ModifierType::SHIFT_MASK) =>
            {
                if let Some(row) = s.mention_ui.list.selected_row() {
                    choose(&s, row.index() as usize);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
            _ => glib::Propagation::Proceed,
        }
    });
    state.composer.add_controller(keys);
}

fn schedule(state: &AppState) {
    if state.mention_ui.queued.replace(true) {
        return;
    }
    let s = state.clone();
    glib::idle_add_local_once(move || {
        s.mention_ui.queued.set(false);
        refresh(&s);
    });
}

pub(super) fn reset(state: &AppState) {
    let ui = &state.mention_ui;
    ui.generation.set(ui.generation.get() + 1);
    ui.loading.set(false);
    ui.failed.set(false);
    *ui.members.borrow_mut() = None;
    ui.selected.borrow_mut().clear();
    ui.filtered.borrow_mut().clear();
    ui.panel.set_visible(false);
    *ui.dismissed.borrow_mut() = None;
}

pub(super) fn dismiss(state: &AppState) -> bool {
    let ui = &state.mention_ui;
    if ui.preediting.get() || !ui.panel.is_visible() {
        return false;
    }
    ui.panel.set_visible(false);
    *ui.dismissed.borrow_mut() =
        Some((state.composer.text().to_string(), state.composer.position()));
    true
}

fn refresh(state: &AppState) {
    let ui = &state.mention_ui;
    let text = state.composer.text();
    let cursor = state.composer.position();
    let query = model::query(&text, cursor.max(0) as usize, &ui.selected.borrow());
    if !*state.session_ready.borrow()
        || state.current_chat.borrow().is_none()
        || state.composer.selection_bounds().is_some()
        || query.is_none()
        || ui
            .dismissed
            .borrow()
            .as_ref()
            .is_some_and(|(t, p)| t == text.as_str() && *p == cursor)
    {
        ui.panel.set_visible(false);
        return;
    }
    *ui.dismissed.borrow_mut() = None;
    let (_, query) = query.unwrap();
    ui.panel.set_visible(true);
    ui.retry.set_label(&crate::i18n::t("mention_retry"));
    if ui.members.borrow().is_none() {
        ui.list.remove_all();
        ui.filtered.borrow_mut().clear();
        ui.scroll.set_visible(false);
        ui.status.set_visible(true);
        if !ui.loading.get() && !ui.failed.get() {
            let chat_mid = state.current_chat.borrow().clone().unwrap();
            match state.sidecar.request(
                "list_chat_members",
                Some(serde_json::json!({"chatMid":chat_mid})),
            ) {
                Ok(id) => {
                    ui.loading.set(true);
                    state.pending.borrow_mut().insert(
                        id,
                        Pending::ChatMembers {
                            generation: ui.generation.get(),
                        },
                    );
                }
                Err(_) => ui.failed.set(true),
            }
        }
        ui.status.set_text(&crate::i18n::t(if ui.failed.get() {
            "mention_failed"
        } else {
            "mention_loading"
        }));
        ui.retry.set_visible(ui.failed.get());
        return;
    }
    ui.retry.set_visible(false);
    let filtered: Vec<_> = ui
        .members
        .borrow()
        .as_ref()
        .unwrap()
        .iter()
        .filter(|m| model::matches(m, &query))
        .cloned()
        .collect();
    ui.status.set_text(&crate::i18n::t(if filtered.is_empty() {
        "mention_empty"
    } else {
        "mention_hint"
    }));
    ui.status.set_visible(true);
    ui.scroll.set_visible(!filtered.is_empty());
    if *ui.filtered.borrow() == filtered {
        return;
    }
    ui.list.remove_all();
    for member in &filtered {
        let label = gtk::Label::builder()
            .label(&member.name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(40)
            .margin_start(12)
            .margin_end(12)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        let row = gtk::ListBoxRow::builder()
            .focusable(false)
            .child(&label)
            .build();
        ui.list.append(&row);
    }
    *ui.filtered.borrow_mut() = filtered;
    ui.list.select_row(ui.list.row_at_index(0).as_ref());
    ui.scroll.vadjustment().set_value(0.0);
}

fn choose(state: &AppState, index: usize) {
    let ui = &state.mention_ui;
    let Some(member) = ui.filtered.borrow().get(index).cloned() else {
        return;
    };
    insert_member(ui, &state.composer, member);
}

fn insert_member(ui: &MentionUi, composer: &gtk::Entry, member: Member) {
    let text = composer.text();
    let cursor = composer.position().max(0) as usize;
    let Some((start, _)) = model::query(&text, cursor, &ui.selected.borrow()) else {
        return;
    };
    let label = format!("@{}", member.name);
    composer.delete_text(start as i32, cursor as i32);
    let mut position = start as i32;
    composer.insert_text(&format!("{label} "), &mut position);
    // A max-length limit may truncate the insert; only keep complete names.
    let new_text = composer.text();
    let end = start + label.chars().count();
    if model::byte_offset(&new_text, end).is_some_and(|end_byte| {
        let start_byte = model::byte_offset(&new_text, start).unwrap();
        new_text[start_byte..end_byte] == label
    }) {
        ui.selected.borrow_mut().push(Mention {
            start,
            end,
            mid: member.mid,
            label,
        });
    }
    composer.set_position(position);
    ui.panel.set_visible(false);
    composer.grab_focus();
}

pub(super) fn response(state: &AppState, generation: u64, ok: bool, result: &serde_json::Value) {
    let ui = &state.mention_ui;
    if generation != ui.generation.get() {
        return;
    }
    ui.loading.set(false);
    let members = ok
        .then(|| serde_json::from_value::<Vec<Member>>(result["members"].clone()).ok())
        .flatten();
    ui.failed.set(members.is_none());
    *ui.members.borrow_mut() = members;
    refresh(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a GTK display; run under xvfb-run"]
    fn gtk_entry_preserves_selected_identity_through_real_buffer_signals() {
        gtk::init().unwrap();
        let ui = MentionUi::new();
        let composer = gtk::Entry::new();
        track_edits(&ui, &composer);
        composer.set_text("😀 @j suffix");
        composer.set_position(4);
        insert_member(
            &ui,
            &composer,
            Member {
                mid: "u1".into(),
                name: "Johnny".into(),
            },
        );
        assert_eq!(composer.text(), "😀 @Johnny  suffix");
        assert_eq!(composer.position(), 10);
        assert_eq!(ui.selected.borrow()[0].start, 2);
        composer.insert_text("你好 ", &mut 0);
        assert_eq!(ui.selected.borrow()[0].start, 5);
        composer.delete_text(0, 3);
        assert_eq!(ui.selected.borrow()[0].start, 2);
        let (_, metadata) = model::compose(
            &composer.text(),
            &serde_json::json!({}),
            &ui.selected.borrow(),
        );
        let wire: serde_json::Value =
            serde_json::from_str(metadata["MENTION"].as_str().unwrap()).unwrap();
        assert_eq!(
            wire["MENTIONEES"][0],
            serde_json::json!({"S":"3","E":"10","M":"u1"})
        );
        composer.delete_text(4, 5);
        assert!(ui.selected.borrow().is_empty());
        let text = composer.delegate().and_downcast::<gtk::Text>().unwrap();
        text.emit_preedit_changed("中");
        assert!(ui.preediting.get());
        text.emit_preedit_changed("");
        assert!(!ui.preediting.get());
        composer.set_text("@j");
        composer.set_position(-1);
        insert_member(
            &ui,
            &composer,
            Member {
                mid: "u2".into(),
                name: "Johnny".into(),
            },
        );
        assert_eq!(ui.selected.borrow()[0].mid, "u2");
        composer.set_text("");
        assert!(ui.selected.borrow().is_empty());
    }
}
