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

pub(super) fn append(state: &AppState, msg: &MessageInfo, outer: &gtk::Box, content: &gtk::Box) {
    if msg.mine
        || msg.from.is_empty()
        || !state
            .current_chat
            .borrow()
            .as_deref()
            .is_some_and(|mid| mid.starts_with(['c', 'r']))
    {
        return;
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
    outer.append(&avatar);
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
    let mut ui = state.sender_ui.borrow_mut();
    if let Some(previous) = ui.profiles.get(mid) {
        if profile.name.is_empty() || profile.name == mid {
            profile.name.clone_from(&previous.name);
        }
        profile.avatar_path = profile.avatar_path.or_else(|| previous.avatar_path.clone());
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
    ui.profiles.insert(mid.to_owned(), profile);
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
