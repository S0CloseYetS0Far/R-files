use super::*;

/// What the path bar shows for a location. Used when loading it and when the typed text is abandoned.
pub(super) fn loc_path_text(loc: &Loc) -> String {
    match loc {
        Loc::Dir(f) => display_path(f),
        Loc::Starred => "Starred".into(),
        Loc::RecentDownloads => "Recent Downloads".into(),
        Loc::Search { root, query, end, .. } => {
            let mut text = format!("Search “{query}” in {}", display_path(root));
            if let Some(note) = end.note() {
                text.push_str(&format!(" ({note})"));
            }
            text
        }
    }
}

impl App {
    /// Path bar: Enter goes there, Tab completes folder names, Escape or leaving the entry
    /// puts the current location back.
    pub(super) fn setup_path_bar(self: &Rc<Self>) {
        let entry = &self.path_entry;
        // Enter: check what the text points at off the UI thread
        let a = Rc::downgrade(self);
        entry.connect_activate(move |e| {
            let Some(a) = a.upgrade() else { return };
            let t = crate::pathbar::expand_tilde(e.text().trim(), &glib::home_dir());
            let f = gio::File::for_commandline_arg(&t);
            let from = a.current_loc().key();
            a.grid.grab_focus();
            let a = Rc::downgrade(&a);
            glib::spawn_future_local(async move {
                let info = f.query_info_future("standard::type", gio::FileQueryInfoFlags::NONE, glib::Priority::DEFAULT);
                let ty = info.await.map(|i| i.file_type()).unwrap_or(gio::FileType::Unknown);
                let Some(a) = a.upgrade() else { return };
                // The user went somewhere else while we were asking
                if a.current_loc().key() != from {
                    return;
                }
                match ty {
                    gio::FileType::Directory | gio::FileType::Mountable => a.navigate(Loc::Dir(f)),
                    gio::FileType::Unknown if f.uri_scheme().map(|s| s != "file").unwrap_or(false) => {
                        a.navigate(Loc::Dir(f))
                    }
                    gio::FileType::Unknown => a.status.set_text(&format!("“{t}” doesn't exist")),
                    _ => a.launch(&f, false),
                }
            });
        });

        // Tab completes, Escape cancels. Capture, so Tab never reaches the focus movement.
        let k = gtk::EventControllerKey::new();
        k.set_propagation_phase(gtk::PropagationPhase::Capture);
        let a = Rc::downgrade(self);
        k.connect_key_pressed(move |_, key, _, mods| {
            let Some(a) = a.upgrade() else { return glib::Propagation::Proceed };
            match key {
                gdk::Key::Escape => {
                    a.restore_path_text();
                    a.grid.grab_focus();
                    glib::Propagation::Stop
                }
                gdk::Key::Tab
                    if !mods.intersects(
                        gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK,
                    ) =>
                {
                    let text = a.path_entry.text().to_string();
                    if !crate::pathbar::looks_like_path(&text) {
                        return glib::Propagation::Proceed;
                    }
                    a.complete_path(text);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        entry.add_controller(k);

        // Leaving the entry without Enter drops what was typed
        let f = gtk::EventControllerFocus::new();
        let a = Rc::downgrade(self);
        f.connect_leave(move |_| {
            let Some(a) = a.upgrade() else { return };
            // Focus moving into the candidate list, or the whole window losing focus, is not "leaving"
            let active = a.window.is_active();
            if active && !a.path_pop.is_visible() {
                a.restore_path_text();
            }
        });
        entry.add_controller(f);

        // Focus goes back to the entry when the list closes
        let e = entry.downgrade();
        self.path_pop.connect_closed(move |_| {
            if let Some(e) = e.upgrade() {
                e.grab_focus_without_selecting();
                e.set_position(-1);
            }
        });
        // The popover is a child of the entry, so let go of it when the entry goes
        let p = self.path_pop.downgrade();
        entry.connect_destroy(move |_| {
            if let Some(p) = p.upgrade() {
                p.unparent();
            }
        });
    }

    fn restore_path_text(&self) {
        let text = loc_path_text(&self.current_loc());
        if self.path_entry.text() != text {
            self.path_entry.set_text(&text);
        }
    }

    /// Tab in the path bar. The folder listing runs off the UI thread; its answer is dropped
    /// when the text has changed meanwhile.
    fn complete_path(self: &Rc<Self>, text: String) {
        use crate::pathbar as pb;
        let Some((parent_typed, segment)) = pb::split_typed(&text) else { return };
        if text == "~" {
            self.path_entry.set_text("~/");
            self.path_entry.set_position(-1);
            return;
        }
        let dir = pb::parent_dir(&parent_typed, &glib::home_dir());
        let a = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let names = gio::spawn_blocking(move || pb::list_folders(&dir)).await.unwrap_or_default();
            let Some(a) = a.upgrade() else { return };
            if a.path_entry.text().as_str() != text {
                return;
            }
            let matches = pb::choose_matches(&names, &segment);
            let Some(new) = pb::completed_text(&parent_typed, &segment, &matches) else { return };
            if new != text {
                a.path_entry.set_text(&new);
                a.path_entry.set_position(-1);
            } else if matches.len() > 1 {
                a.show_path_candidates(&parent_typed, &matches);
            }
        });
    }

    /// A small list of folders under the entry; picking one (click or Enter) completes the text.
    fn show_path_candidates(self: &Rc<Self>, parent_typed: &str, matches: &[String]) {
        let list = gtk::ListBox::new();
        let shown = &matches[..matches.len().min(crate::pathbar::MAX_SHOWN)];
        for m in shown {
            let l = gtk::Label::new(Some(m));
            l.set_xalign(0.0);
            list.append(&l);
        }
        if matches.len() > shown.len() {
            let l = gtk::Label::new(Some("…"));
            l.set_xalign(0.0);
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&l));
            row.set_activatable(false);
            row.set_selectable(false);
            list.append(&row);
        }
        let texts: Vec<String> = shown.iter().map(|m| format!("{parent_typed}{m}/")).collect();
        let a = Rc::downgrade(self);
        list.connect_row_activated(move |_, row| {
            let Some(a) = a.upgrade() else { return };
            let Some(t) = texts.get(row.index() as usize) else { return };
            a.path_entry.set_text(t);
            a.path_entry.set_position(-1);
            a.path_pop.popdown();
        });
        self.path_pop.set_child(Some(&list));
        self.path_pop.popup();
        if let Some(first) = list.row_at_index(0) {
            list.select_row(Some(&first));
            first.grab_focus();
        }
    }
}
