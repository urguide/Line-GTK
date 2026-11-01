use super::*;
use serde_json::{Value, json};
use std::cell::Cell;

#[derive(Debug)]
pub(super) struct NotesUi {
    window: gtk::Window,
    chat: String,
    account: String,
    active: Cell<bool>,
    busy: Cell<bool>,
    controls: gtk::Box,
    status: gtk::Label,
    stack: gtk::Stack,
    list: gtk::Box,
    detail: gtk::Box,
    editor: gtk::TextView,
    comment: gtk::Entry,
    more: gtk::Button,
    cursor: RefCell<Value>,
    seen: RefCell<HashSet<String>>,
    selected: RefCell<Value>,
    editing: Cell<bool>,
    attachments: RefCell<Vec<String>>,
    attach: gtk::Button,
    attached: gtk::Label,
}

fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .build()
}

fn button(key: &str) -> gtk::Button {
    gtk::Button::with_label(&crate::i18n::t(key))
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

pub(super) fn close(state: &AppState) {
    let ui = state.notes_ui.borrow_mut().take();
    if let Some(ui) = ui {
        ui.active.set(false);
        ui.window.close();
    }
}

pub(super) fn open(state: &AppState) {
    if !*state.session_ready.borrow() {
        toast(state, &crate::i18n::t("still_restoring"));
        return;
    }
    let Some(chat) = state.current_chat.borrow().clone() else {
        return;
    };
    close(state);
    let window = gtk::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title(format!(
            "{} — {}",
            crate::i18n::t("notes"),
            state.chat_title.text()
        ))
        .default_width(680)
        .default_height(720)
        .build();
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_start(18)
        .margin_end(18)
        .margin_top(16)
        .margin_bottom(16)
        .build();
    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .vexpand(true)
        .build();
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let back = button("notes_back");
    let refresh = button("notes_refresh");
    let create = button("notes_new");
    create.add_css_class("suggested-action");
    bar.append(&back);
    bar.append(&refresh);
    bar.append(&create);
    controls.append(&bar);
    let stack = gtk::Stack::builder().vexpand(true).build();
    let list = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let more = button("notes_more");
    more.set_visible(false);
    let list_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    list_page.append(&list);
    list_page.append(&more);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list_page)
        .build();
    stack.add_named(&scroll, Some("list"));
    let detail = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let detail_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let detail_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&detail)
        .build();
    detail_page.append(&detail_scroll);
    let comment_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let comment = gtk::Entry::builder()
        .placeholder_text(crate::i18n::t("notes_comment_hint"))
        .max_length(10000)
        .hexpand(true)
        .build();
    let send_comment = button("notes_comment");
    comment_row.append(&comment);
    comment_row.append(&send_comment);
    detail_page.append(&comment_row);
    stack.add_named(&detail_page, Some("detail"));
    let editor_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    editor_page.append(&label(&crate::i18n::t("notes_shared_hint")));
    let editor = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .vexpand(true)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    let editor_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&editor)
        .build();
    let save = button("notes_save");
    let attach = button("notes_attach");
    let attached = label("");
    save.add_css_class("suggested-action");
    editor_page.append(&editor_scroll);
    editor_page.append(&attach);
    editor_page.append(&attached);
    editor_page.append(&save);
    stack.add_named(&editor_page, Some("editor"));
    stack.set_visible_child_name("list");
    controls.append(&stack);
    let status = label("");
    root.append(&controls);
    root.append(&status);
    window.set_child(Some(&root));
    let ui = Rc::new(NotesUi {
        window,
        chat,
        account: state.self_mid.borrow().clone().unwrap_or_default(),
        active: Cell::new(true),
        busy: Cell::new(false),
        controls,
        status,
        stack,
        list,
        detail,
        editor,
        comment,
        more,
        cursor: RefCell::new(Value::Null),
        seen: RefCell::new(HashSet::new()),
        selected: RefCell::new(Value::Null),
        editing: Cell::new(false),
        attachments: RefCell::new(Vec::new()),
        attach,
        attached,
    });
    let weak = Rc::downgrade(&ui);
    ui.window.connect_close_request(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.active.set(false);
        }
        glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(&ui);
    back.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.stack.set_visible_child_name("list");
        }
    });
    let weak = Rc::downgrade(&ui);
    let s = state.clone();
    refresh.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            request(&s, &ui, "list", json!({}));
        }
    });
    let weak = Rc::downgrade(&ui);
    create.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.editing.set(false);
            ui.attachments.borrow_mut().clear();
            ui.attached.set_text("");
            ui.attach.set_visible(true);
            ui.editor.buffer().set_text("");
            ui.stack.set_visible_child_name("editor");
            ui.editor.grab_focus();
        }
    });
    let weak = Rc::downgrade(&ui);
    let s = state.clone();
    save.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            let buffer = ui.editor.buffer();
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string();
            if (text.trim().is_empty() && ui.attachments.borrow().is_empty())
                || text.encode_utf16().count() > 10000
            {
                ui.status.set_text(&crate::i18n::t("notes_text_required"));
                return;
            }
            let action = if ui.editing.get() { "update" } else { "create" };
            let post = ui.selected.borrow()["id"].clone();
            let paths = ui.attachments.borrow().clone();
            request(
                &s,
                &ui,
                action,
                json!({"text":text,"postId":post,"filePaths":paths}),
            );
        }
    });
    let weak = Rc::downgrade(&ui);
    let s = state.clone();
    send_comment.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            let text = ui.comment.text().to_string();
            if text.trim().is_empty() {
                return;
            }
            let post = ui.selected.borrow()["id"].clone();
            request(&s, &ui, "comment", json!({"text":text,"postId":post}));
        }
    });
    let weak = Rc::downgrade(&ui);
    let s = state.clone();
    ui.more.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            let cursor = ui.cursor.borrow().clone();
            request(&s, &ui, "list", json!({"cursor":cursor}));
        }
    });
    *state.notes_ui.borrow_mut() = Some(ui.clone());
    let weak = Rc::downgrade(&ui);
    ui.attach.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            let filter = gtk::FileFilter::new();
            for suffix in [
                "jpg", "jpeg", "png", "gif", "webp", "mp4", "mov", "m4v", "webm",
            ] {
                filter.add_suffix(suffix);
            }
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let dialog = gtk::FileDialog::builder()
                .title(crate::i18n::t("notes_attach"))
                .filters(&filters)
                .build();
            let weak = Rc::downgrade(&ui);
            dialog.open_multiple(Some(&ui.window), None::<&gio::Cancellable>, move |result| {
                let Some(ui) = weak.upgrade().filter(|ui| ui.active.get()) else {
                    return;
                };
                let Ok(files) = result else {
                    return;
                };
                if files.n_items() > 20 {
                    ui.status.set_text(&crate::i18n::t("notes_media_limit"));
                    return;
                }
                let mut paths = Vec::new();
                for index in 0..files.n_items() {
                    if let Some(path) = files
                        .item(index)
                        .and_then(|f| f.downcast::<gio::File>().ok())
                        .and_then(|f| f.path())
                    {
                        paths.push(path.to_string_lossy().into_owned());
                    }
                }
                ui.attached.set_text(
                    &paths
                        .iter()
                        .map(|p| {
                            PathBuf::from(p)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                *ui.attachments.borrow_mut() = paths;
            });
        }
    });
    ui.window.present();
    request(state, &ui, "list", json!({}));
}

fn request(state: &AppState, ui: &Rc<NotesUi>, action: &str, mut params: Value) {
    if !ui.active.get() || ui.busy.get() {
        return;
    }
    if !*state.session_ready.borrow() || state.self_mid.borrow().as_deref() != Some(&ui.account) {
        ui.status.set_text(&crate::i18n::t("still_restoring"));
        return;
    }
    let append = !params["cursor"].is_null() || !params["scrollId"].is_null();
    params["chatMid"] = json!(ui.chat);
    params["action"] = json!(action);
    match state.sidecar.request("notes", Some(params)) {
        Ok(id) => {
            ui.busy.set(true);
            ui.controls.set_sensitive(false);
            ui.status.set_text(&crate::i18n::t("notes_loading"));
            state.pending.borrow_mut().insert(
                id,
                Pending::Notes {
                    ui: Rc::downgrade(ui),
                    action: action.into(),
                    append,
                },
            );
        }
        Err(error) => ui.status.set_text(&crate::i18n::tf(
            "notes_failed",
            &[("error", &error.to_string())],
        )),
    }
}

fn date(time: &Value) -> String {
    time.as_i64()
        .filter(|n| *n > 0)
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn post_body(post: &Value) -> gtk::Box {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let heading = label(&format!(
        "{}  {}",
        post["authorName"].as_str().unwrap_or_default(),
        date(&post["createdTime"])
    ));
    heading.add_css_class("heading");
    body.append(&heading);
    body.append(&label(post["text"].as_str().unwrap_or_default()));
    for (field, key) in [
        ("media", "notes_media"),
        ("stickers", "notes_stickers"),
        ("locations", "notes_locations"),
    ] {
        if let Some(items) = post[field].as_array().filter(|items| !items.is_empty()) {
            body.append(&label(&crate::i18n::tf(
                key,
                &[("n", &items.len().to_string())],
            )));
        }
    }
    body.append(&label(&crate::i18n::tf(
        "notes_counts",
        &[
            ("likes", &post["likes"].to_string()),
            ("comments", &post["comments"].to_string()),
        ],
    )));
    body
}

fn render_detail(state: &AppState, ui: &Rc<NotesUi>, result: &Value) {
    clear(&ui.detail);
    let post = &result["post"];
    *ui.selected.borrow_mut() = post.clone();
    ui.detail.append(&post_body(post));
    if let Some(media) = post["media"].as_array() {
        for item in media {
            if !matches!(item["type"].as_str(), Some("PHOTO" | "VIDEO")) {
                continue;
            }
            let open = button(if item["type"] == "VIDEO" {
                "notes_open_video"
            } else {
                "notes_open_image"
            });
            ui.detail.append(&open);
            let weak = Rc::downgrade(ui);
            let s = state.clone();
            let oid = item["objectId"].clone();
            let id = post["id"].clone();
            open.connect_clicked(move |_| {
                if let Some(ui) = weak.upgrade() {
                    request(&s, &ui, "media", json!({"postId":id,"objectId":oid}));
                }
            });
        }
    }
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let like = button(if post["liked"] == true {
        "notes_unlike"
    } else {
        "notes_like"
    });
    actions.append(&like);
    let weak = Rc::downgrade(ui);
    let s = state.clone();
    like.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            let post = ui.selected.borrow().clone();
            request(
                &s,
                &ui,
                "like",
                json!({"postId":post["id"],"liked":post["liked"] != true}),
            );
        }
    });
    if post["mine"] == true {
        let edit = button("notes_edit");
        let delete = button("notes_delete");
        delete.add_css_class("destructive-action");
        actions.append(&edit);
        actions.append(&delete);
        let weak = Rc::downgrade(ui);
        edit.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.editing.set(true);
                ui.attachments.borrow_mut().clear();
                ui.attached.set_text("");
                ui.attach.set_visible(false);
                ui.editor
                    .buffer()
                    .set_text(ui.selected.borrow()["text"].as_str().unwrap_or_default());
                ui.stack.set_visible_child_name("editor");
            }
        });
        let weak = Rc::downgrade(ui);
        let s = state.clone();
        delete.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let dialog = libadwaita::MessageDialog::builder()
                    .transient_for(&ui.window)
                    .modal(true)
                    .heading(crate::i18n::t("notes_delete"))
                    .body(crate::i18n::t("notes_delete_confirm"))
                    .build();
                dialog.add_response("cancel", &crate::i18n::t("notes_cancel"));
                dialog.add_response("delete", &crate::i18n::t("notes_delete"));
                dialog
                    .set_response_appearance("delete", libadwaita::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                let weak = Rc::downgrade(&ui);
                let s = s.clone();
                dialog.connect_response(None, move |_, response| {
                    if response == "delete"
                        && let Some(ui) = weak.upgrade()
                    {
                        let id = ui.selected.borrow()["id"].clone();
                        request(&s, &ui, "delete", json!({"postId":id}));
                    }
                });
                dialog.present();
            }
        });
    }
    ui.detail.append(&actions);
    ui.detail
        .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    render_comments(state, ui, result);
    ui.stack.set_visible_child_name("detail");
}

fn render_comments(state: &AppState, ui: &Rc<NotesUi>, result: &Value) {
    let mut child = ui.detail.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if widget.widget_name() == "notes-more-comments" {
            ui.detail.remove(&widget);
        }
    }
    if result["commentsError"] == true {
        ui.detail
            .append(&label(&crate::i18n::t("notes_comments_failed")));
    }
    if let Some(comments) = result["comments"].as_array() {
        for comment in comments {
            ui.detail.append(&label(&format!(
                "{}  {}\n{}",
                comment["authorName"].as_str().unwrap_or_default(),
                date(&comment["createdTime"]),
                comment["text"].as_str().unwrap_or_default()
            )));
            if comment["attachments"].as_u64().unwrap_or(0) > 0 {
                ui.detail.append(&label(&crate::i18n::tf(
                    "notes_media",
                    &[("n", &comment["attachments"].to_string())],
                )));
            }
        }
    }
    if let Some(scroll) = result["scrollId"].as_str().filter(|s| !s.is_empty()) {
        let more = button("notes_more_comments");
        more.set_widget_name("notes-more-comments");
        ui.detail.append(&more);
        let scroll = scroll.to_owned();
        let weak = Rc::downgrade(ui);
        let s = state.clone();
        more.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let id = ui.selected.borrow()["id"].clone();
                request(&s, &ui, "comments", json!({"postId":id,"scrollId":scroll}));
            }
        });
    }
}

pub(super) fn response(
    state: &AppState,
    ui: &Rc<NotesUi>,
    action: &str,
    append: bool,
    ok: bool,
    result: &Value,
    error: Option<&str>,
) {
    if !ui.active.get() || state.self_mid.borrow().as_deref() != Some(&ui.account) {
        return;
    }
    ui.busy.set(false);
    ui.controls.set_sensitive(true);
    ui.status.set_text("");
    if !ok {
        ui.status.set_text(&crate::i18n::tf(
            "notes_failed",
            &[("error", error.unwrap_or("request failed"))],
        ));
        return;
    }
    match action {
        "list" => {
            if !append {
                clear(&ui.list);
                ui.seen.borrow_mut().clear();
            }
            let mut added = 0;
            if let Some(posts) = result["posts"].as_array() {
                for post in posts {
                    let id = post["id"].as_str().unwrap_or_default().to_owned();
                    if !ui.seen.borrow_mut().insert(id.clone()) {
                        continue;
                    }
                    added += 1;
                    let card = post_body(post);
                    card.add_css_class("card");
                    let open = button("notes_open");
                    card.append(&open);
                    ui.list.append(&card);
                    let weak = Rc::downgrade(ui);
                    let s = state.clone();
                    open.connect_clicked(move |_| {
                        if let Some(ui) = weak.upgrade() {
                            ui.comment.set_text("");
                            request(&s, &ui, "get", json!({"postId":id}));
                        }
                    });
                }
            }
            if ui.seen.borrow().is_empty() {
                ui.list.append(&label(&crate::i18n::t("notes_empty")));
            }
            let next = result["cursor"].clone();
            ui.more.set_visible(
                added > 0 && !next.is_null() && (!append || next != *ui.cursor.borrow()),
            );
            *ui.cursor.borrow_mut() = next;
            ui.stack.set_visible_child_name("list");
        }
        "get" => render_detail(state, ui, result),
        "media" => {
            if let Some(path) = result["path"].as_str() {
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("note-media");
                open_media_viewer(
                    state,
                    path,
                    result["contentType"].as_str().unwrap_or("IMAGE"),
                    name,
                );
            }
        }
        "comments" => render_comments(state, ui, result),
        "comment" | "like" | "update" => {
            if action == "comment" {
                ui.comment.set_text("");
            }
            let id = ui.selected.borrow()["id"].clone();
            request(state, ui, "get", json!({"postId":id}));
        }
        "create" | "delete" => {
            ui.editor.buffer().set_text("");
            ui.attachments.borrow_mut().clear();
            ui.attached.set_text("");
            ui.stack.set_visible_child_name("list");
            request(state, ui, "list", json!({}));
        }
        _ => {}
    }
}
