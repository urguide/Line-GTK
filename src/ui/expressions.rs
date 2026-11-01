use super::*;
use serde_json::{Value, json};

#[derive(Debug)]
pub(super) struct LibraryUi {
    window: libadwaita::Window,
    kind: String,
    rows: gtk::Box,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    selected: gtk::Button,
    all: gtk::Button,
    refresh: gtk::Button,
    stop: gtk::Button,
    checks: RefCell<Vec<(String, gtk::CheckButton, gtk::Label)>>,
    queue: RefCell<VecDeque<String>>,
    total: RefCell<usize>,
    done: RefCell<usize>,
    failed: RefCell<usize>,
    busy: RefCell<bool>,
    icons: RefCell<VecDeque<(String, gtk::Picture, gtk::Image)>>,
}

pub(super) fn open_library(state: &AppState, kind: &str) {
    if !*state.session_ready.borrow() {
        return;
    }
    if let Some(ui) = state.expression_ui.borrow().as_ref() {
        ui.window.present();
        return;
    }
    let window = libadwaita::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title(crate::i18n::t(if kind == "emoji" {
            "emojis"
        } else {
            "expressions_manage"
        }))
        .default_width(560)
        .default_height(620)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.append(&libadwaita::HeaderBar::new());
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_start(16)
        .margin_end(16)
        .margin_bottom(16)
        .vexpand(true)
        .build();
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let selected = gtk::Button::with_label(&crate::i18n::t("expressions_download_selected"));
    selected.add_css_class("suggested-action");
    let all = gtk::Button::with_label(&crate::i18n::t("expressions_download_all"));
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text(crate::i18n::t("expressions_refresh"))
        .build();
    let stop = gtk::Button::with_label(&crate::i18n::t("expressions_stop"));
    stop.set_sensitive(false);
    for b in [&selected, &all, &refresh, &stop] {
        controls.append(b);
    }
    body.append(&controls);
    let status = gtk::Label::builder().xalign(0.0).wrap(true).build();
    body.append(&status);
    let progress = gtk::ProgressBar::new();
    body.append(&progress);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&rows)
        .build();
    body.append(&scroll);
    root.append(&body);
    window.set_content(Some(&root));
    let ui = Rc::new(LibraryUi {
        window,
        kind: kind.into(),
        rows,
        status,
        progress,
        selected,
        all,
        refresh,
        stop,
        checks: RefCell::new(Vec::new()),
        queue: RefCell::new(VecDeque::new()),
        total: RefCell::new(0),
        done: RefCell::new(0),
        failed: RefCell::new(0),
        busy: RefCell::new(false),
        icons: RefCell::new(VecDeque::new()),
    });
    for (button, all) in [(&ui.selected, false), (&ui.all, true)] {
        let weak = Rc::downgrade(&ui);
        let s = state.clone();
        button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                *ui.queue.borrow_mut() = ui
                    .checks
                    .borrow()
                    .iter()
                    .filter(|(_, check, _)| check.is_sensitive() && (all || check.is_active()))
                    .map(|(id, _, _)| id.clone())
                    .collect();
                *ui.total.borrow_mut() = ui.queue.borrow().len();
                *ui.done.borrow_mut() = 0;
                *ui.failed.borrow_mut() = 0;
                ui.progress.set_fraction(0.0);
                next_download(&s, &ui);
            }
        });
    }
    let weak = Rc::downgrade(&ui);
    ui.stop.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.queue.borrow_mut().clear();
            ui.stop.set_sensitive(false);
            ui.status.set_text(&crate::i18n::t("expressions_stopping"));
        }
    });
    let weak = Rc::downgrade(&ui);
    let s = state.clone();
    ui.refresh.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            request_library(&s, &ui, true);
        }
    });
    let s = state.clone();
    ui.window.connect_close_request(move |_| {
        s.expression_ui.borrow_mut().take();
        glib::Propagation::Proceed
    });
    *state.expression_ui.borrow_mut() = Some(ui.clone());
    ui.window.present();
    request_library(state, &ui, false);
}

pub(super) fn close(state: &AppState) {
    *state.emoji_catalog.borrow_mut() = Value::Null;
    let ui = state.expression_ui.borrow_mut().take();
    if let Some(ui) = ui {
        ui.queue.borrow_mut().clear();
        ui.window.close();
    }
    state.sticker_popover.popdown();
}

pub(super) fn wire_composer_preview(state: &AppState) {
    let preview = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_start(16)
        .margin_end(16)
        .visible(false)
        .build();
    state
        .conversation
        .insert_child_after(&preview, Some(&state.reply_bar));
    let s = state.clone();
    state.composer.connect_changed(move |entry| {
        while let Some(child) = preview.first_child() {
            preview.remove(&child);
        }
        let (text, metadata) =
            line_rust_core::expressions::compose(entry.text().as_str(), &s.emoji_catalog.borrow());
        let resources = line_rust_core::expressions::resources(&metadata);
        let visible = resources.as_array().is_some_and(|r| !r.is_empty());
        preview.set_visible(visible);
        if visible {
            let msg: MessageInfo =
                serde_json::from_value(json!({"text":text,"emojis":resources})).unwrap();
            append_inline(&s, &preview, &msg);
        }
    });
}

fn busy(ui: &LibraryUi, value: bool) {
    *ui.busy.borrow_mut() = value;
    for button in [&ui.selected, &ui.all, &ui.refresh] {
        button.set_sensitive(!value);
    }
}

fn request_library(state: &AppState, ui: &Rc<LibraryUi>, force: bool) {
    busy(ui, true);
    ui.status.set_text(&crate::i18n::t("loading_stickers"));
    match state.sidecar.request(
        "expression_library",
        Some(json!({"kind":ui.kind,"force":force})),
    ) {
        Ok(id) => {
            state.pending.borrow_mut().insert(
                id,
                Pending::ExpressionLibrary {
                    ui: Rc::downgrade(ui),
                },
            );
        }
        Err(error) => load_failed(ui, &error.to_string()),
    }
}

pub(super) fn load_failed(ui: &LibraryUi, error: &str) {
    busy(ui, false);
    ui.status.set_text(error);
}

pub(super) fn fill_library(state: &AppState, ui: &Rc<LibraryUi>, result: &Value) {
    ui.icons.borrow_mut().clear();
    while let Some(child) = ui.rows.first_child() {
        ui.rows.remove(&child);
    }
    ui.checks.borrow_mut().clear();
    for product in result["products"].as_array().into_iter().flatten() {
        let id = line_rust_core::message::string(&product["id"]);
        let name = product["name"].as_str().unwrap_or(&id);
        let downloaded = product["downloaded"] == true;
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let check = gtk::CheckButton::new();
        check.set_sensitive(!downloaded);
        let icon = gtk::Overlay::new();
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .tooltip_text(name)
            .build();
        picture.set_size_request(64, 64);
        icon.set_child(Some(&picture));
        let placeholder = gtk::Image::from_icon_name("image-x-generic-symbolic");
        placeholder.set_pixel_size(32);
        placeholder.add_css_class("dim-label");
        icon.add_overlay(&placeholder);
        if let Some(path) = product["iconPath"]
            .as_str()
            .filter(|p| std::path::Path::new(p).is_file())
        {
            attach_texture_async(picture.clone(), path.into(), 96);
            placeholder.set_visible(false);
        } else {
            ui.icons
                .borrow_mut()
                .push_back((id.clone(), picture, placeholder));
        }
        let label = gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .build();
        let status = gtk::Label::new(Some(&crate::i18n::t(if downloaded {
            "expressions_downloaded"
        } else {
            "expressions_not_downloaded"
        })));
        status.add_css_class("dim-label");
        row.append(&check);
        row.append(&icon);
        row.append(&label);
        row.append(&status);
        ui.rows.append(&row);
        ui.checks.borrow_mut().push((id, check, status));
    }
    busy(ui, false);
    ui.status.set_text(&if result["stale"] == true {
        crate::i18n::t("expressions_stale")
    } else {
        crate::i18n::tf(
            "expressions_count",
            &[("count", &ui.checks.borrow().len().to_string())],
        )
    });
    next_icon(state, ui);
}

fn next_icon(state: &AppState, ui: &Rc<LibraryUi>) {
    // One small request at a time: the list is usable immediately, and downloads
    // or chat operations can run between cover images. Closing drops the queue.
    loop {
        let Some((package, picture, placeholder)) = ui.icons.borrow_mut().pop_front() else {
            return;
        };
        if let Ok(id) = state.sidecar.request(
            "expression_icon",
            Some(json!({"kind":ui.kind,"packageId":package})),
        ) {
            state.pending.borrow_mut().insert(
                id,
                Pending::ExpressionIcon {
                    ui: Rc::downgrade(ui),
                    picture: picture.downgrade(),
                    placeholder: placeholder.downgrade(),
                },
            );
            return;
        }
    }
}

pub(super) fn icon_done(
    state: &AppState,
    ui: &Rc<LibraryUi>,
    picture: &glib::WeakRef<gtk::Picture>,
    placeholder: &glib::WeakRef<gtk::Image>,
    result: &Value,
) {
    // A refreshed list or closed window must not be updated by an older request.
    let Some(picture) = picture.upgrade() else {
        return;
    };
    if let Some(path) = result["iconPath"].as_str() {
        attach_texture_async(picture, path.into(), 96);
        if let Some(placeholder) = placeholder.upgrade() {
            placeholder.set_visible(false);
        }
    }
    next_icon(state, ui);
}

fn next_download(state: &AppState, ui: &Rc<LibraryUi>) {
    let next = ui.queue.borrow_mut().pop_front();
    let Some(package) = next else {
        busy(ui, false);
        ui.stop.set_sensitive(false);
        ui.status.set_text(&crate::i18n::tf(
            "expressions_finished",
            &[
                ("done", &ui.done.borrow().to_string()),
                ("failed", &ui.failed.borrow().to_string()),
            ],
        ));
        return;
    };
    busy(ui, true);
    ui.stop.set_sensitive(true);
    ui.status.set_text(&crate::i18n::tf(
        "expressions_downloading",
        &[
            (
                "current",
                &(*ui.done.borrow() + *ui.failed.borrow() + 1).to_string(),
            ),
            ("total", &ui.total.borrow().to_string()),
        ],
    ));
    if let Some((_, _, status)) = ui.checks.borrow().iter().find(|(id, _, _)| id == &package) {
        status.set_text(&crate::i18n::t("loading_stickers"));
    }
    match state.sidecar.request(
        "download_expression",
        Some(json!({"kind":ui.kind,"packageId":package})),
    ) {
        Ok(id) => {
            state.pending.borrow_mut().insert(
                id,
                Pending::ExpressionDownload {
                    ui: Rc::downgrade(ui),
                    package,
                },
            );
        }
        Err(error) => download_done(state, ui, &package, Err(&error.to_string())),
    }
}

pub(super) fn download_done(
    state: &AppState,
    ui: &Rc<LibraryUi>,
    package: &str,
    result: Result<&Value, &str>,
) {
    if result.is_ok() {
        *ui.done.borrow_mut() += 1;
    } else {
        *ui.failed.borrow_mut() += 1;
    }
    if let Some((_, check, status)) = ui.checks.borrow().iter().find(|(id, _, _)| id == package) {
        check.set_sensitive(result.is_err());
        check.set_active(result.is_err());
        status.set_text(&crate::i18n::t(if result.is_ok() {
            "expressions_downloaded"
        } else {
            "expressions_failed"
        }));
        status.set_tooltip_text(result.err());
    }
    ui.progress.set_fraction(
        (*ui.done.borrow() + *ui.failed.borrow()) as f64 / (*ui.total.borrow()).max(1) as f64,
    );
    next_download(state, ui);
}

pub(super) fn append_inline(state: &AppState, bubble: &gtk::Box, msg: &MessageInfo) {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .hexpand(true)
        .css_classes(["line-inline-emoji"])
        .build();
    view.set_size_request(220, -1);
    let mut resources = msg.emojis.clone();
    let mut missing = false;
    for resource in &mut resources {
        let package = line_rust_core::message::string(&resource["productId"]);
        let id = line_rust_core::message::string(&resource["sticonId"]);
        if !line_rust_core::expressions::valid_id(&package)
            || !line_rust_core::expressions::valid_id(&id)
        {
            continue;
        }
        let path = state
            .data_dir
            .join("cache/stickers/emoji")
            .join(package)
            .join(format!("{id}.png"));
        if path.is_file() {
            resource["imagePath"] = json!(path);
        } else {
            missing = true;
        }
    }
    render_inline(&view, &msg.text, &json!(resources));
    if missing
        && let Ok(id) = state
            .sidecar
            .request("expression_images", Some(json!({"resources":resources})))
    {
        state.pending.borrow_mut().insert(
            id,
            Pending::EmojiImages {
                view: view.downgrade(),
                text: msg.text.clone(),
            },
        );
    }
    bubble.append(&view);
}

pub(super) fn render_inline(view: &gtk::TextView, text: &str, resources: &Value) {
    let buffer = view.buffer();
    buffer.set_text(text);
    let mut spans = Vec::new();
    for resource in resources.as_array().into_iter().flatten().take(100) {
        let Some((start, end)) =
            resource["S"]
                .as_u64()
                .zip(resource["E"].as_u64())
                .and_then(|(s, e)| {
                    Some((
                        line_rust_core::expressions::char_offset(text, s)?,
                        line_rust_core::expressions::char_offset(text, e)?,
                    ))
                })
        else {
            continue;
        };
        let Some(path) = resource["imagePath"]
            .as_str()
            .filter(|p| std::path::Path::new(p).is_file())
        else {
            continue;
        };
        if start < end {
            spans.push((start, end, path));
        }
    }
    spans.sort_by_key(|(start, _, _)| *start);
    let mut previous_end = 0;
    spans.retain(|(start, end, _)| {
        if *start < previous_end {
            false
        } else {
            previous_end = *end;
            true
        }
    });
    for (start, end, path) in spans.into_iter().rev() {
        let mut begin = buffer.iter_at_offset(start);
        let mut finish = buffer.iter_at_offset(end);
        buffer.delete(&mut begin, &mut finish);
        let anchor = buffer.create_child_anchor(&mut begin);
        let pic = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .build();
        pic.set_size_request(24, 24);
        attach_texture_async(pic.clone(), path.into(), 48);
        view.add_child_at_anchor(&pic, &anchor);
    }
}
