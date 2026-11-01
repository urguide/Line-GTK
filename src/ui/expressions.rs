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
        busy: RefCell::new(false),
        icons: RefCell::new(VecDeque::new()),
    });
    for (button, all) in [(&ui.selected, false), (&ui.all, true)] {
        let weak = Rc::downgrade(&ui);
        let s = state.clone();
        button.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let packages: Vec<_> = ui
                    .checks
                    .borrow()
                    .iter()
                    .filter(|(_, check, _)| check.is_sensitive() && (all || check.is_active()))
                    .map(|(id, _, _)| id.clone())
                    .collect();
                if packages.is_empty() {
                    return;
                }
                queue_request(
                    &s,
                    "enqueue_expressions",
                    json!({"kind":ui.kind,"packages":packages}),
                );
            }
        });
    }
    let weak = Rc::downgrade(&ui);
    let s = state.clone();
    ui.stop.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            queue_request(&s, "stop_expressions", json!({"kind":ui.kind}));
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
    state.expression_catalogs.borrow_mut().clear();
    *state.expression_downloads.borrow_mut() = Value::Null;
    let epoch = state.expression_thumbs.borrow().epoch.wrapping_add(1);
    *state.expression_thumbs.borrow_mut() = ThumbCache {
        epoch,
        ..Default::default()
    };
    let ui = state.expression_ui.borrow_mut().take();
    if let Some(ui) = ui {
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
    let cached = state.expression_catalogs.borrow().get(&ui.kind).cloned();
    if let Some(cached) = cached {
        fill_library(state, ui, &cached);
    }
    busy(ui, true);
    if ui.checks.borrow().is_empty() {
        ui.status.set_text(&crate::i18n::t("loading_stickers"));
    }
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
    cache_catalog(state, &ui.kind, result);
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
    update_progress(state, ui);
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

fn queue_request(state: &AppState, method: &str, params: Value) {
    match state.sidecar.request(method, Some(params)) {
        Ok(id) => {
            state
                .pending
                .borrow_mut()
                .insert(id, Pending::ExpressionQueue);
        }
        Err(error) => toast(state, &error.to_string()),
    }
}

pub(super) fn preload(state: &AppState) {
    for kind in ["sticker", "emoji"] {
        request_local(state, kind);
    }
    queue_request(state, "expression_queue", json!({}));
}

pub(super) fn request_local(state: &AppState, kind: &str) {
    if let Ok(id) = state
        .sidecar
        .request("expression_local", Some(json!({"kind":kind})))
    {
        state
            .pending
            .borrow_mut()
            .insert(id, Pending::ListStickers { kind: kind.into() });
    }
}

pub(super) fn cache_catalog(state: &AppState, kind: &str, result: &Value) {
    let changed = state.expression_catalogs.borrow().get(kind) != Some(result);
    state
        .expression_catalogs
        .borrow_mut()
        .insert(kind.into(), result.clone());
    if kind == "emoji" {
        *state.emoji_catalog.borrow_mut() = result.clone();
    }
    if !changed {
        return;
    }
    let mut paths = Vec::new();
    for pack in result["packs"].as_array().into_iter().flatten().take(80) {
        if let Some(path) = pack["iconPath"].as_str() {
            paths.push(path.to_owned());
        }
    }
    for item in result["packs"][0]["stickers"]
        .as_array()
        .into_iter()
        .flatten()
        .take(48)
    {
        if let Some(path) = item["imagePath"].as_str() {
            paths.push(path.to_owned());
        }
    }
    for path in paths {
        load_thumb(state, None, path);
    }
}

pub(super) fn catalog_ready(state: &AppState, kind: &str, result: &Value) {
    if result["account"].is_string()
        && result["account"].as_str() != state.self_mid.borrow().as_deref()
    {
        return;
    }
    let changed = state.expression_catalogs.borrow().get(kind) != Some(result);
    cache_catalog(state, kind, result);
    if changed
        && state.sticker_popover.is_visible()
        && *state.expression_picker_kind.borrow() == kind
    {
        fill_sticker_popover(state, result, kind);
    }
}

pub(super) fn queue_state(state: &AppState, result: &Value) {
    if result["account"].as_str() != state.self_mid.borrow().as_deref() {
        return;
    }
    *state.expression_downloads.borrow_mut() = result.clone();
    if let Some(kind) = result["kind"].as_str()
        && result["catalog"].is_object()
    {
        catalog_ready(state, kind, &result["catalog"]);
    }
    if let Some(ui) = state.expression_ui.borrow().as_ref() {
        update_progress(state, ui);
    }
}

fn update_progress(state: &AppState, ui: &LibraryUi) {
    let snapshot = state.expression_downloads.borrow();
    let jobs: Vec<_> = snapshot["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|j| j["kind"] == ui.kind)
        .collect();
    let pending = jobs
        .iter()
        .filter(|j| matches!(j["status"].as_str(), Some("queued" | "downloading")))
        .count();
    let done = jobs.iter().filter(|j| j["status"] == "done").count();
    let failed = jobs.iter().filter(|j| j["status"] == "failed").count();
    for (id, check, label) in ui.checks.borrow().iter() {
        if let Some(job) = jobs.iter().find(|j| j["package"] == *id) {
            let status = job["status"].as_str().unwrap_or_default();
            // Completed jobs are historical; disk validation is authoritative after cache clearing.
            if status == "done" {
                let installed = state
                    .expression_catalogs
                    .borrow()
                    .get(&ui.kind)
                    .is_some_and(|c| {
                        c["products"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|p| p["id"] == *id && p["downloaded"] == true)
                    });
                check.set_sensitive(!installed);
                label.set_text(&crate::i18n::t(if installed {
                    "expressions_downloaded"
                } else {
                    "expressions_not_downloaded"
                }));
            } else {
                check.set_sensitive(!matches!(status, "queued" | "downloading"));
                label.set_text(&crate::i18n::t(match status {
                    "queued" => "expressions_queued",
                    "downloading" => "expressions_background_active",
                    "failed" => "expressions_failed",
                    _ => "expressions_not_downloaded",
                }));
                if status == "downloading" {
                    label.set_text(&format!(
                        "{} {}/{}",
                        crate::i18n::t("expressions_background_active"),
                        job["done"],
                        job["total"]
                    ));
                }
            }
            if !check.is_sensitive() {
                check.set_active(false);
            }
            label.set_tooltip_text(job["error"].as_str());
        }
    }
    ui.stop.set_sensitive(pending > 0);
    if !jobs.is_empty() {
        ui.progress
            .set_fraction((done + failed) as f64 / jobs.len() as f64);
        ui.status.set_text(&crate::i18n::tf(
            "expressions_background_status",
            &[
                ("pending", &pending.to_string()),
                ("done", &done.to_string()),
                ("failed", &failed.to_string()),
            ],
        ));
    }
}

#[derive(Default)]
pub(super) struct ThumbCache {
    epoch: u64,
    active: usize,
    decode_queue: VecDeque<String>,
    textures: HashMap<String, gdk::Texture>,
    order: VecDeque<String>,
    waiting: HashMap<String, Vec<glib::WeakRef<gtk::Picture>>>,
}

pub(super) fn attach_thumb(state: &AppState, picture: gtk::Picture, path: String) {
    load_thumb(state, Some(picture), path);
}

fn load_thumb(state: &AppState, picture: Option<gtk::Picture>, path: String) {
    if path.is_empty() {
        return;
    }
    let cache = state.expression_thumbs.clone();
    {
        let mut c = cache.borrow_mut();
        if let Some(texture) = c.textures.get(&path) {
            if let Some(picture) = picture {
                picture.set_paintable(Some(texture));
            }
            c.order.retain(|key| key != &path);
            c.order.push_back(path);
            return;
        }
        let waiter = picture.map(|p| p.downgrade());
        if let Some(waiting) = c.waiting.get_mut(&path) {
            waiting.extend(waiter);
            return;
        }
        c.waiting.insert(path.clone(), waiter.into_iter().collect());
        c.decode_queue.push_back(path);
    }
    pump_thumbs(cache);
}

fn pump_thumbs(cache: Rc<RefCell<ThumbCache>>) {
    let (path, epoch) = {
        let mut c = cache.borrow_mut();
        if c.active >= 2 {
            return;
        }
        let Some(path) = c.decode_queue.pop_front() else {
            return;
        };
        c.active += 1;
        (path, c.epoch)
    };
    let (tx, rx) = async_channel::bounded(1);
    let file = path.clone();
    std::thread::spawn(move || {
        let _ = tx.send_blocking(crate::sticker_anim::load_scaled(&file, 96, false));
    });
    let next = cache.clone();
    glib::spawn_future_local(async move {
        let texture = rx
            .recv()
            .await
            .ok()
            .flatten()
            .and_then(|frames| frames.frames.first().and_then(raw_frame_to_pixbuf))
            .map(|pb| gdk::Texture::for_pixbuf(&pb));
        let mut c = cache.borrow_mut();
        if c.epoch != epoch {
            return;
        }
        c.active -= 1;
        let Some(waiting) = c.waiting.remove(&path) else {
            return;
        };
        if let Some(texture) = texture {
            for picture in waiting {
                if let Some(picture) = picture.upgrade() {
                    picture.set_paintable(Some(&texture));
                }
            }
            c.textures.insert(path.clone(), texture);
            c.order.push_back(path);
            while c.order.len() > 256 {
                if let Some(old) = c.order.pop_front() {
                    c.textures.remove(&old);
                }
            }
        }
        drop(c);
        pump_thumbs(cache);
    });
    pump_thumbs(next);
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
