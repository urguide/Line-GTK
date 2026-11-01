use super::*;

#[derive(Default)]
pub(super) struct SenderUi {
    profiles: HashMap<String, ChatInfo>,
    requested: HashSet<String>,
    slots: HashMap<String, Vec<SenderSlot>>,
}

struct SenderSlot {
    name: glib::WeakRef<gtk::Label>,
    picture: glib::WeakRef<gtk::Picture>,
}

pub(super) fn reset(state: &AppState) {
    *state.sender_ui.borrow_mut() = SenderUi::default();
}

pub(super) fn clear_slots(state: &AppState) {
    state.sender_ui.borrow_mut().slots.clear();
}

pub(super) fn append(
    state: &AppState,
    msg: &MessageInfo,
    outer: &gtk::Box,
    content: &gtk::Box,
) -> Option<(gtk::Widget, gtk::Widget)> {
    if msg.mine || msg.from.is_empty() {
        return None;
    }

    let profile = state
        .sender_ui
        .borrow()
        .profiles
        .get(&msg.from)
        .cloned()
        .or_else(|| {
            state
                .chats
                .borrow()
                .iter()
                .find(|c| c.mid == msg.from)
                .cloned()
        })
        .or_else(|| {
            state.friends_ui.borrow().as_ref().and_then(|ui| {
                ui.friends
                    .borrow()
                    .iter()
                    .find(|c| c.mid == msg.from)
                    .cloned()
            })
        });
    let name = profile.as_ref().map_or(msg.from.as_str(), |p| &p.name);
    let label = gtk::Label::builder()
        .label(name)
        .tooltip_text(name)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(32)
        .css_classes(["line-msg-sender", "caption"])
        .build();
    let picture = gtk::Picture::builder()
        .width_request(36)
        .height_request(36)
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Cover)
        .tooltip_text(name)
        .build();
    let placeholder = gtk::Image::builder()
        .icon_name("avatar-default-symbolic")
        .pixel_size(24)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    let avatar = gtk::Overlay::builder()
        .width_request(36)
        .height_request(36)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["line-msg-avatar"])
        .child(&picture)
        .build();
    avatar.add_overlay(&placeholder);
    picture.connect_paintable_notify(move |picture| {
        placeholder.set_visible(picture.paintable().is_none());
    });
    if let Some(path) = profile.as_ref().and_then(|p| p.avatar_path.as_deref())
        && std::path::Path::new(path).is_file()
    {
        attach_texture_async(picture.clone(), path.to_owned(), 36);
    }
    if let Some(profile) = profile {
        state
            .sender_ui
            .borrow_mut()
            .profiles
            .insert(msg.from.clone(), profile);
    }
    let gutter = avatar_gutter(&avatar);
    outer.append(&gutter);
    content.append(&label);
    state
        .sender_ui
        .borrow_mut()
        .slots
        .entry(msg.from.clone())
        .or_default()
        .push(SenderSlot {
            name: label.downgrade(),
            picture: picture.downgrade(),
        });

    // Resolve actual senders, including non-friends and former group members.
    // One request per sender per conversation, shared by history and live rows.
    if *state.session_ready.borrow()
        && state
            .sender_ui
            .borrow_mut()
            .requested
            .insert(msg.from.clone())
        && let Ok(id) = state
            .sidecar
            .request("sender_profile", Some(serde_json::json!({"mid":msg.from})))
    {
        state.pending.borrow_mut().insert(
            id,
            Pending::SenderProfile {
                mid: msg.from.clone(),
                generation: *state.history_generation.borrow(),
            },
        );
    }
    Some((avatar.upcast(), label.upcast()))
}

fn avatar_gutter(avatar: &gtk::Overlay) -> gtk::Box {
    // A horizontal gutter forwards the bubble height into GtkPicture's
    // width-for-height measurement, widening this column for taller messages.
    // A vertical, top-aligned gutter measures the avatar at its natural height.
    let gutter = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(36)
        .valign(gtk::Align::Start)
        .hexpand(false)
        .vexpand(false)
        .build();
    gutter.append(avatar);
    gutter
}

pub(super) fn response(
    state: &AppState,
    mid: &str,
    generation: u64,
    ok: bool,
    result: &serde_json::Value,
) {
    if generation != *state.history_generation.borrow() || !ok {
        return;
    }
    let Ok(mut profile) = serde_json::from_value::<ChatInfo>(result.clone()) else {
        return;
    };
    if profile.mid != mid {
        return;
    }
    apply_profile(state, &mut profile);
}

pub(super) fn apply_profile(state: &AppState, profile: &mut ChatInfo) {
    let mid = profile.mid.as_str();
    if let Some(alias) = profile.alias.as_deref().filter(|s| !s.trim().is_empty()) {
        profile.name = alias.to_owned();
    }
    let mut ui = state.sender_ui.borrow_mut();
    if let Some(previous) = ui.profiles.get(mid) {
        if profile.name.is_empty() || profile.name == mid {
            profile.name.clone_from(&previous.name);
        }
        profile.avatar_path = profile
            .avatar_path
            .clone()
            .or_else(|| previous.avatar_path.clone());
    }
    if let Some(slots) = ui.slots.get_mut(mid) {
        slots.retain(|slot| {
            let (Some(label), Some(picture)) = (slot.name.upgrade(), slot.picture.upgrade()) else {
                return false;
            };
            label.set_text(&profile.name);
            label.set_tooltip_text(Some(&profile.name));
            picture.set_tooltip_text(Some(&profile.name));
            if let Some(path) = profile.avatar_path.as_deref()
                && std::path::Path::new(path).is_file()
            {
                attach_texture_async(picture, path.to_owned(), 36);
            }
            true
        });
    }
    ui.profiles.insert(mid.to_owned(), profile.clone());
}

pub(super) fn avatar_ready(state: &AppState, mid: &str, path: &str) {
    let mut ui = state.sender_ui.borrow_mut();
    if let Some(profile) = ui.profiles.get_mut(mid) {
        profile.avatar_path = Some(path.to_owned());
    }
    if let Some(slots) = ui.slots.get_mut(mid) {
        slots.retain(|slot| {
            let Some(picture) = slot.picture.upgrade() else {
                return false;
            };
            if std::path::Path::new(path).is_file() {
                attach_texture_async(picture, path.to_owned(), 36);
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a GTK display; run under xvfb-run"]
    fn gtk_avatar_column_keeps_text_media_and_continuations_aligned() {
        gtk::init().unwrap();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mut rows = Vec::new();
        for (width, height, show_avatar) in [
            (140, 40, true),
            (400, 85, true),
            (128, 150, true),
            (360, 100, true),
            (140, 40, false),
        ] {
            let outer = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .hexpand(true)
                .valign(gtk::Align::End)
                .build();
            let picture = gtk::Picture::builder()
                .width_request(36)
                .height_request(36)
                .can_shrink(true)
                .content_fit(gtk::ContentFit::Cover)
                .build();
            let pixbuf = Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 36, 36).unwrap();
            pixbuf.fill(0xff0000ff);
            picture.set_paintable(Some(&gdk::Texture::for_pixbuf(&pixbuf)));
            let avatar = gtk::Overlay::builder()
                .width_request(36)
                .height_request(36)
                .halign(gtk::Align::Start)
                .valign(gtk::Align::Start)
                .child(&picture)
                .build();
            let gutter = avatar_gutter(&avatar);
            avatar.set_visible(show_avatar);
            outer.append(&gutter);
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(4)
                .halign(gtk::Align::Start)
                .build();
            content.append(
                &gtk::Box::builder()
                    .width_request(width)
                    .height_request(height)
                    .build(),
            );
            outer.append(&content);
            outer.append(&gtk::Label::new(Some("3:20 PM")));
            outer.append(&gtk::Box::builder().hexpand(true).build());
            root.append(&outer);
            rows.push((gutter, content, height));
        }
        let (_, height, _, _) = root.measure(gtk::Orientation::Vertical, 800);
        root.allocate(800, height, -1, None);
        let mut expected_y = 0.0;
        for (gutter, content, height) in rows {
            let bounds = content.compute_bounds(&root).unwrap();
            assert_eq!(
                gutter.width(),
                36,
                "avatar column stretched for content height {height}"
            );
            assert_eq!(bounds.x(), 44.0, "content left edge for height {height}");
            assert_eq!(bounds.y(), expected_y, "unexpected vertical gap");
            assert_eq!(bounds.height(), height as f32);
            expected_y += height as f32;
        }
    }
}
