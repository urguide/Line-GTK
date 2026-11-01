use super::*;

/// Multiline message input with a bounded height and an empty-state hint.
#[derive(Clone)]
pub(super) struct Composer {
    view: gtk::TextView,
    container: gtk::Overlay,
    placeholder: gtk::Label,
}

impl std::ops::Deref for Composer {
    type Target = gtk::TextView;

    fn deref(&self) -> &Self::Target {
        &self.view
    }
}

impl Composer {
    pub fn new() -> Self {
        let view = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .left_margin(12)
            .right_margin(12)
            .top_margin(8)
            .bottom_margin(8)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .min_content_height(34)
            .max_content_height(160)
            .propagate_natural_height(true)
            .child(&view)
            .build();
        let placeholder = gtk::Label::builder()
            .label(crate::i18n::t("type_message"))
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_start(12)
            .margin_top(8)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .can_target(false)
            .css_classes(["dim-label"])
            .build();
        let container = gtk::Overlay::builder()
            .hexpand(true)
            .valign(gtk::Align::Center)
            .css_classes(["line-composer-entry"])
            .child(&scroll)
            .build();
        container.add_overlay(&placeholder);
        let hint = placeholder.clone();
        view.buffer().connect_changed(move |buffer| {
            hint.set_visible(buffer.char_count() == 0);
        });
        Self {
            view,
            container,
            placeholder,
        }
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.container
    }

    pub fn set_placeholder_text(&self, text: Option<&str>) {
        self.placeholder.set_text(text.unwrap_or_default());
    }

    pub fn text(&self) -> glib::GString {
        let buffer = self.buffer();
        buffer.text(&buffer.start_iter(), &buffer.end_iter(), true)
    }

    pub fn set_text(&self, text: &str) {
        self.buffer().set_text(text);
    }

    pub fn position(&self) -> i32 {
        self.buffer().cursor_position()
    }

    pub fn set_position(&self, position: i32) {
        let buffer = self.buffer();
        buffer.place_cursor(&buffer.iter_at_offset(position));
        self.scroll_mark_onscreen(&buffer.get_insert());
    }

    pub fn selection_bounds(&self) -> Option<(i32, i32)> {
        self.buffer()
            .selection_bounds()
            .map(|(start, end)| (start.offset(), end.offset()))
    }

    pub fn delete_text(&self, start: i32, end: i32) {
        let buffer = self.buffer();
        buffer.delete(
            &mut buffer.iter_at_offset(start),
            &mut buffer.iter_at_offset(end),
        );
    }

    pub fn insert_text(&self, text: &str, position: &mut i32) {
        let buffer = self.buffer();
        let mut iter = buffer.iter_at_offset(*position);
        buffer.insert(&mut iter, text);
        *position = iter.offset();
    }

    pub fn connect_changed<F: Fn(&Self) + 'static>(&self, f: F) {
        let composer = self.clone();
        self.buffer().connect_changed(move |_| f(&composer));
    }
}
