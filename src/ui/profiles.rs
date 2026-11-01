use super::*;
use serde_json::{Value, json};
use std::cell::Cell;

#[derive(Debug)]
pub(super) struct ProfileUi {
    window: gtk::Window,
    mid: String,
    account: String,
    own: bool,
    active: Cell<bool>,
    busy: Cell<bool>,
    friend: Cell<bool>,
    name: gtk::Label,
    bio: gtk::Label,
    status: gtk::Label,
    cover_status: gtk::Label,
    avatar: gtk::Picture,
    cover: gtk::Picture,
    controls: gtk::Box,
    add: gtk::Button,
    save: gtk::Button,
    cancel: gtk::Button,
    avatar_path: RefCell<Option<String>>,
    cover_path: RefCell<Option<String>>,
    avatar_epoch: Rc<Cell<u64>>,
    cover_epoch: Rc<Cell<u64>>,
    selected: RefCell<Option<(String, String)>>,
    chat: RefCell<ChatInfo>,
}

fn button(key: &str, icon: &str) -> gtk::Button {
    gtk::Button::builder()
        .child(&button_content(key, icon))
        .tooltip_text(crate::i18n::t(key))
        .build()
}

fn button_content(key: &str, icon: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_halign(gtk::Align::Center);
    row.append(&gtk::Image::from_icon_name(icon));
    row.append(&gtk::Label::new(Some(&crate::i18n::t(key))));
    row
}

pub(super) fn close(state: &AppState) {
    let ui = state.profile_ui.borrow_mut().take();
    if let Some(ui) = ui {
        ui.active.set(false);
        ui.window.close();
    }
}

pub(super) fn open_self(state: &AppState) {
    let mid = state.self_mid.borrow().clone();
    if let Some(mid) = mid {
        open(state, &mid);
    }
}

pub(super) fn open(state: &AppState, mid: &str) {
    if !*state.session_ready.borrow() {
        toast(state, &crate::i18n::t("still_restoring"));
        return;
    }
    close(state);
    let account = state.self_mid.borrow().clone().unwrap_or_default();
    let own = mid == account;
    let chat = state
        .chats
        .borrow()
        .iter()
        .find(|c| c.mid == mid)
        .cloned()
        .unwrap_or_else(|| {
            serde_json::from_value(json!({"mid":mid,"name":mid,"kind":"user"})).unwrap()
        });
    let window = gtk::Window::builder()
        .transient_for(&state.window)
        .modal(true)
        .title(crate::i18n::t(if own {
            "my_profile"
        } else {
            "view_profile"
        }))
        .default_width(420)
        .default_height(580)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&libadwaita::HeaderBar::new());
    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    let cover = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Cover)
        .build();
    let cover_frame = gtk::AspectFrame::builder()
        .ratio(2.2)
        .obey_child(false)
        .child(&cover)
        .css_classes(["line-profile-cover"])
        .build();
    body.append(&cover_frame);
    let avatar = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Cover)
        .width_request(96)
        .height_request(96)
        .build();
    let avatar_overlay = gtk::Overlay::builder()
        .child(&avatar)
        .halign(gtk::Align::Center)
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["line-profile-avatar"])
        .build();
    let placeholder = gtk::Image::builder()
        .icon_name("avatar-default-symbolic")
        .pixel_size(48)
        .build();
    avatar_overlay.add_overlay(&placeholder);
    avatar.connect_paintable_notify(move |picture| {
        placeholder.set_visible(picture.paintable().is_none())
    });
    body.append(&avatar_overlay);
    let name = gtk::Label::builder()
        .label(if own {
            state.self_display_name.borrow().clone()
        } else {
            chat.name.clone()
        })
        .wrap(true)
        .max_width_chars(32)
        .css_classes(["title-2"])
        .build();
    let bio = gtk::Label::builder()
        .wrap(true)
        .selectable(true)
        .max_width_chars(38)
        .margin_start(18)
        .margin_end(18)
        .build();
    body.append(&name);
    body.append(&bio);
    let cover_status = gtk::Label::builder()
        .wrap(true)
        .css_classes(["dim-label", "caption"])
        .label(crate::i18n::t("profile_cover_loading"))
        .build();
    body.append(&cover_status);
    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_start(18)
        .margin_end(18)
        .margin_bottom(18)
        .build();
    let actions = gtk::Box::builder().spacing(10).homogeneous(true).build();
    let add = button("add_friend", "list-add-symbolic");
    let chat_btn = button("profile_chat", "chat-symbolic");
    chat_btn.add_css_class("suggested-action");
    let avatar_edit = button("profile_change_photo", "camera-photo-symbolic");
    let cover_edit = button("profile_change_cover", "image-x-generic-symbolic");
    if own {
        controls.append(
            &gtk::Label::builder()
                .label(crate::i18n::t("profile_sync_hint"))
                .wrap(true)
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        actions.append(&avatar_edit);
        actions.append(&cover_edit);
    } else {
        actions.append(&add);
        actions.append(&chat_btn);
    }
    controls.append(&actions);
    let save = button("profile_save", "document-save-symbolic");
    save.add_css_class("suggested-action");
    save.set_visible(false);
    let cancel = button("cancel", "edit-undo-symbolic");
    cancel.set_visible(false);
    controls.append(&save);
    controls.append(&cancel);
    let refresh = button("profile_refresh", "view-refresh-symbolic");
    controls.append(&refresh);
    body.append(&controls);
    let status = gtk::Label::builder()
        .wrap(true)
        .max_width_chars(40)
        .margin_start(18)
        .margin_end(18)
        .margin_bottom(18)
        .build();
    body.append(&status);
    root.append(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&body)
            .build(),
    );
    window.set_child(Some(&root));
    let avatar_path = if own {
        state.self_avatar_path.borrow().clone()
    } else {
        chat.avatar_path.clone()
    };
    let ui = Rc::new(ProfileUi {
        window,
        mid: mid.into(),
        account,
        own,
        active: Cell::new(true),
        busy: Cell::new(false),
        friend: Cell::new(false),
        name,
        bio,
        status,
        cover_status,
        avatar,
        cover,
        controls,
        add,
        save,
        cancel,
        avatar_path: RefCell::new(avatar_path),
        cover_path: RefCell::new(None),
        avatar_epoch: Rc::new(Cell::new(0)),
        cover_epoch: Rc::new(Cell::new(0)),
        selected: RefCell::new(None),
        chat: RefCell::new(chat),
    });
    load_picture(&ui, false, ui.avatar_path.borrow().clone());
    {
        let weak = Rc::downgrade(&ui);
        ui.window.connect_close_request(move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.active.set(false);
            }
            glib::Propagation::Proceed
        });
        let window = ui.window.downgrade();
        let esc = gtk::EventControllerKey::new();
        esc.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        ui.window.add_controller(esc);
    }
    for (widget, action) in [(&avatar_edit, "avatar"), (&cover_edit, "background")] {
        let s = state.clone();
        let weak = Rc::downgrade(&ui);
        widget.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                choose_image(&s, &ui, action);
            }
        });
    }
    {
        let s = state.clone();
        let weak = Rc::downgrade(&ui);
        ui.save.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                let selected = ui.selected.borrow().clone();
                if let Some((action, path)) = selected {
                    request(&s, &ui, &action, Some(&path));
                }
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.cancel.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                discard_preview(&ui);
            }
        });
        let s = state.clone();
        let weak = Rc::downgrade(&ui);
        refresh.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                discard_preview(&ui);
                request(&s, &ui, "get", None);
            }
        });
        let s = state.clone();
        let weak = Rc::downgrade(&ui);
        ui.add.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                request(&s, &ui, "add", None);
            }
        });
        let s = state.clone();
        let weak = Rc::downgrade(&ui);
        chat_btn.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                if !valid(&s, &ui) {
                    return;
                }
                let chat = s
                    .chats
                    .borrow()
                    .iter()
                    .find(|c| c.mid == ui.mid)
                    .cloned()
                    .unwrap_or_else(|| ui.chat.borrow().clone());
                ui.window.close();
                open_chat(&s, &chat);
            }
        });
    }
    for (picture, is_cover) in [(&ui.avatar, false), (&ui.cover, true)] {
        let s = state.clone();
        let weak = Rc::downgrade(&ui);
        let click = gtk::GestureClick::new();
        click.set_button(1);
        picture.set_cursor_from_name(Some("pointer"));
        click.connect_released(move |_, _, _, _| {
            if let Some(ui) = weak.upgrade() {
                if !valid(&s, &ui) {
                    return;
                }
                let selected = ui
                    .selected
                    .borrow()
                    .clone()
                    .filter(|(action, _)| (action == "background") == is_cover);
                let path = if let Some((_, path)) = selected {
                    Some(path)
                } else if is_cover {
                    ui.cover_path.borrow().clone()
                } else {
                    ui.avatar_path.borrow().clone()
                };
                if let Some(path) = path {
                    open_media_viewer(&s, &path, "IMAGE", "profile.jpg");
                }
            }
        });
        picture.add_controller(click);
    }
    *state.profile_ui.borrow_mut() = Some(ui.clone());
    ui.window.present();
    request(state, &ui, "get", None);
}

fn valid(state: &AppState, ui: &ProfileUi) -> bool {
    ui.active.get()
        && *state.session_ready.borrow()
        && state.self_mid.borrow().as_deref() == Some(ui.account.as_str())
}

fn discard_preview(ui: &ProfileUi) {
    ui.selected.borrow_mut().take();
    ui.save.set_visible(false);
    ui.cancel.set_visible(false);
    load_picture(ui, false, ui.avatar_path.borrow().clone());
    load_picture(ui, true, ui.cover_path.borrow().clone());
}

// A cancelled or superseded decode must not overwrite the newest preview.
fn load_picture(ui: &ProfileUi, cover: bool, path: Option<String>) {
    let (picture, epoch, size) = if cover {
        (&ui.cover, &ui.cover_epoch, 640)
    } else {
        (&ui.avatar, &ui.avatar_epoch, 96)
    };
    let generation = epoch.get().wrapping_add(1);
    epoch.set(generation);
    picture.set_paintable(None::<&gdk::Texture>);
    let Some(path) = path else { return };
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(crate::sticker_anim::load_scaled(&path, size, false));
    });
    let picture = picture.downgrade();
    let epoch = epoch.clone();
    glib::spawn_future_local(async move {
        if let Ok(Some(frames)) = rx.recv().await
            && epoch.get() == generation
            && let Some(picture) = picture.upgrade()
        {
            super::media::apply_frames_to_picture(&picture, frames, false);
        }
    });
}

fn choose_image(state: &AppState, ui: &Rc<ProfileUi>, action: &'static str) {
    if !valid(state, ui) || ui.busy.get() {
        return;
    }
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(&crate::i18n::t("profile_image_files")));
    for mime in ["image/jpeg", "image/png", "image/webp"] {
        filter.add_mime_type(mime);
    }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder()
        .title(crate::i18n::t(if action == "avatar" {
            "profile_change_photo"
        } else {
            "profile_change_cover"
        }))
        .filters(&filters)
        .default_filter(&filter)
        .build();
    let s = state.clone();
    let weak = Rc::downgrade(ui);
    dialog.open(Some(&ui.window), None::<&gio::Cancellable>, move |result| {
        let Some(ui) = weak.upgrade() else { return };
        if !valid(&s, &ui) || ui.busy.get() {
            return;
        }
        let file = match result {
            Ok(file) => file,
            Err(error) if error.matches(gtk::DialogError::Dismissed) => return,
            Err(error) => {
                ui.status.set_text(&error.to_string());
                return;
            }
        };
        let Some(path) = file.path() else {
            ui.status.set_text(&crate::i18n::t("profile_local_file"));
            return;
        };
        discard_preview(&ui);
        let path = path.to_string_lossy().into_owned();
        load_picture(&ui, action == "background", Some(path.clone()));
        *ui.selected.borrow_mut() = Some((action.into(), path));
        ui.save.set_visible(true);
        ui.cancel.set_visible(true);
        ui.status.set_text(&crate::i18n::t("profile_preview_hint"));
    });
}

fn request(state: &AppState, ui: &Rc<ProfileUi>, action: &str, path: Option<&str>) {
    if !valid(state, ui) || ui.busy.replace(true) {
        return;
    }
    ui.controls.set_sensitive(false);
    ui.status.set_text(&crate::i18n::t("profile_loading"));
    match state.sidecar.request(
        "profile",
        Some(json!({"mid":ui.mid,"action":action,"filePath":path})),
    ) {
        Ok(id) => {
            state.pending.borrow_mut().insert(
                id,
                Pending::Profile {
                    ui: Rc::downgrade(ui),
                    action: action.into(),
                },
            );
        }
        Err(error) => {
            ui.busy.set(false);
            ui.controls.set_sensitive(true);
            ui.status.set_text(&error.to_string());
        }
    }
}

pub(super) fn response(
    state: &AppState,
    ui: &Rc<ProfileUi>,
    action: &str,
    ok: bool,
    result: &Value,
    error: Option<&str>,
) {
    if !valid(state, ui) {
        return;
    }
    ui.busy.set(false);
    ui.controls.set_sensitive(true);
    if !ok {
        let message = crate::i18n::tf(
            "profile_failed",
            &[("error", error.unwrap_or("request failed"))],
        );
        if action == "cover" {
            ui.cover_status.set_text(&message);
            ui.status.set_text("");
        } else {
            ui.status.set_text(&message);
        }
        if action == "get" {
            ui.add.set_sensitive(false);
        }
        return;
    }
    ui.status.set_text("");
    match action {
        "get" => {
            if let Ok(mut chat) = serde_json::from_value::<ChatInfo>(result.clone()) {
                if chat.mid != ui.mid {
                    return;
                }
                senders::apply_profile(state, &mut chat);
                ui.name.set_text(&chat.name);
                *ui.avatar_path.borrow_mut() = chat.avatar_path.clone();
                load_picture(ui, false, chat.avatar_path.clone());
                if ui.own {
                    apply_self_profile(
                        state,
                        &chat.mid,
                        &chat.name,
                        chat.avatar_path.as_deref(),
                        result["picturePath"].as_str(),
                    );
                }
                *ui.chat.borrow_mut() = chat;
            }
            ui.bio.set_text(
                result["statusMessage"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&crate::i18n::t("profile_no_status")),
            );
            ui.friend.set(result["isFriend"] == true);
            ui.add.set_sensitive(!ui.friend.get());
            ui.add.set_child(Some(&button_content(
                if ui.friend.get() {
                    "profile_already_friend"
                } else {
                    "add_friend"
                },
                if ui.friend.get() {
                    "emblem-ok-symbolic"
                } else {
                    "list-add-symbolic"
                },
            )));
            request(state, ui, "cover", None);
        }
        "cover" => {
            *ui.cover_path.borrow_mut() = result["coverPath"].as_str().map(str::to_owned);
            load_picture(ui, true, ui.cover_path.borrow().clone());
            if ui.cover_path.borrow().is_some() {
                ui.cover_status.set_text("");
            } else {
                ui.cover_status
                    .set_text(&crate::i18n::t("profile_no_cover"));
            }
        }
        "add" => {
            ui.friend.set(true);
            ui.add.set_sensitive(false);
            ui.add.set_child(Some(&button_content(
                "profile_already_friend",
                "emblem-ok-symbolic",
            )));
            ui.status.set_text(&crate::i18n::t("friend_ok"));
            request_friends_list(state);
        }
        "avatar" | "background" => {
            if let Some(path) = result["path"].as_str() {
                if action == "avatar" {
                    *ui.avatar_path.borrow_mut() = Some(path.into());
                } else {
                    *ui.cover_path.borrow_mut() = Some(path.into());
                    ui.cover_status.set_text("");
                }
            }
            discard_preview(ui);
            ui.status.set_text(&crate::i18n::t("profile_saved"));
        }
        _ => {}
    }
}
