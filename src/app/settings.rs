use super::*;

const CUSTOM_STYLE: u32 = 2;

/// Drop-down index for a "folder_icon" setting.
fn folder_style_index(setting: &str) -> u32 {
    if setting == "system" {
        1
    } else if setting.starts_with("custom:") {
        CUSTOM_STYLE
    } else {
        0
    }
}

const ICON_SIZES: [(i32, &str); 3] = [(40, "Small"), (56, "Medium"), (72, "Large")];

impl App {
    // ================= settings (icons) =================
    pub(super) fn setup_settings_popover(self: &Rc<Self>) {
        let bx = gtk::Box::new(gtk::Orientation::Vertical, 8);
        bx.set_margin_top(10);
        bx.set_margin_bottom(10);
        bx.set_margin_start(10);
        bx.set_margin_end(10);

        let h1 = gtk::Label::new(Some("Folder icon"));
        h1.set_xalign(0.0);
        h1.add_css_class("heading");
        let style = gtk::DropDown::from_strings(&["Light blue", "System theme", "Custom image…"]);
        let cur = self.config.borrow().folder_icon.clone();
        style.set_selected(folder_style_index(&cur));
        // Re-picks the image, since selecting "Custom image…" again does nothing once it is selected
        let choose = gtk::Button::with_label("Choose image…");
        choose.set_halign(gtk::Align::Start);
        choose.set_visible(style.selected() == CUSTOM_STYLE);

        let h2 = gtk::Label::new(Some("Icon size"));
        h2.set_xalign(0.0);
        h2.add_css_class("heading");
        let names: Vec<&str> = ICON_SIZES.iter().map(|s| s.1).collect();
        let size = gtk::DropDown::from_strings(&names);
        let cur_size = self.config.borrow().icon_size;
        size.set_selected(ICON_SIZES.iter().position(|s| s.0 == cur_size).unwrap_or(0) as u32);

        let h3 = gtk::Label::new(Some("Theme"));
        h3.set_xalign(0.0);
        h3.add_css_class("heading");
        let theme = gtk::DropDown::from_strings(&crate::theme::THEME_NAMES);
        theme.set_selected(self.config.borrow().theme.index());

        let hint = gtk::Label::new(Some("Tip: right-click any file or folder\n→ Change Icon… to give just that one\nits own icon."));
        hint.set_xalign(0.0);
        hint.add_css_class("dim");
        hint.add_css_class("small");

        bx.append(&h1);
        bx.append(&style);
        bx.append(&choose);
        bx.append(&h2);
        bx.append(&size);
        bx.append(&h3);
        bx.append(&theme);
        bx.append(&hint);
        bx.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let pop = gtk::Popover::new();
        for (label, action) in [("Keyboard shortcuts", "win.shortcuts"), ("About RFiles", "win.about")] {
            let b = gtk::Button::with_label(label);
            b.add_css_class("flat");
            if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.0);
            }
            b.set_action_name(Some(action));
            let pw = pop.downgrade();
            b.connect_clicked(move |_| {
                if let Some(p) = pw.upgrade() {
                    p.popdown();
                }
            });
            bx.append(&b);
        }
        pop.set_child(Some(&bx));
        self.settings_btn.set_popover(Some(&pop));

        let a = Rc::downgrade(self);
        let busy = Rc::new(Cell::new(false));
        let choose_w = choose.downgrade();
        style.connect_selected_notify(move |dd| {
            let Some(a) = a.upgrade() else { return };
            // Also runs for the cancel path below, which is what hides the button again
            if let Some(b) = choose_w.upgrade() {
                b.set_visible(dd.selected() == CUSTOM_STYLE);
            }
            if busy.get() {
                return;
            }
            match dd.selected() {
                0 => a.set_folder_icon("blue".into()),
                1 => a.set_folder_icon("system".into()),
                _ => {
                    let prev = a.config.borrow().folder_icon.clone();
                    let (a2, dd2, busy2) = (a.clone(), dd.clone(), busy.clone());
                    a.pick_image("Choose a folder icon", move |res| match res.and_then(|f| f.path()) {
                        Some(p) => a2.set_folder_icon(format!("custom:{}", p.display())),
                        None => {
                            // Cancelled: put the dropdown back
                            busy2.set(true);
                            dd2.set_selected(folder_style_index(&prev));
                            busy2.set(false);
                        }
                    });
                }
            }
        });
        let a = Rc::downgrade(self);
        choose.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            let a2 = a.clone();
            a.pick_image("Choose a folder icon", move |res| {
                if let Some(p) = res.and_then(|f| f.path()) {
                    a2.set_folder_icon(format!("custom:{}", p.display()));
                }
            });
        });
        let a = Rc::downgrade(self);
        size.connect_selected_notify(move |dd| {
            let Some(a) = a.upgrade() else { return };
            let px = ICON_SIZES.get(dd.selected() as usize).map(|s| s.0).unwrap_or(40);
            ICON_SIZE.with(|s| s.set(px));
            a.config.borrow_mut().icon_size = px;
            a.save_config();
            a.icons_changed();
        });
        let a = Rc::downgrade(self);
        theme.connect_selected_notify(move |dd| {
            let Some(a) = a.upgrade() else { return };
            a.config.borrow_mut().theme = crate::theme::Theme::from_index(dd.selected());
            a.save_config();
            // One style sheet is shared, so every open window changes at once
            crate::theme::apply();
        });
    }

    fn set_folder_icon(self: &Rc<Self>, setting: String) {
        if !crate::config::storable(&setting) {
            self.status.set_text("Images with a line break in their path can't be used as the folder icon");
            return;
        }
        apply_folder_icon_setting(&setting);
        self.config.borrow_mut().folder_icon = setting;
        self.save_config();
        self.icons_changed();
    }

    pub(super) fn pick_image(&self, title: &str, f: impl FnOnce(Option<gio::File>) + 'static) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_mime_type("image/*");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dlg = gtk::FileDialog::builder().title(title).modal(true).filters(&filters).build();
        dlg.open(Some(&self.window), None::<&gio::Cancellable>, move |r| f(r.ok()));
    }

    pub(super) fn set_custom_icon(self: &Rc<Self>, target: &gio::File, icon_uri: &str) {
        match target.set_attribute(
            "metadata::custom-icon",
            icon_uri,
            gio::FileQueryInfoFlags::NONE,
            None::<&gio::Cancellable>,
        ) {
            Ok(()) => self.reload(),
            Err(e) => self.status.set_text(&format!("Couldn't set icon: {}", e.message())),
        }
    }
}

#[cfg(test)]
mod folder_style_tests {
    use super::folder_style_index;

    #[test]
    fn folder_style_indexes() {
        assert_eq!(folder_style_index("blue"), 0);
        assert_eq!(folder_style_index(""), 0);
        assert_eq!(folder_style_index("system"), 1);
        assert_eq!(folder_style_index("custom:/a/b.png"), 2);
    }
}
