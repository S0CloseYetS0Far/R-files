use super::*;

impl App {
    // ================= copy / cut / paste =================
    pub(super) fn copy_selected(&self, cut: bool) {
        let files: Vec<gio::File> = self.selected_infos().iter().map(item_file).collect();
        if files.is_empty() {
            return;
        }
        fileops::set_clipboard(&self.window.clipboard(), &files, cut);
        let n = files.len();
        let what = if n == 1 { "1 item".to_string() } else { format!("{n} items") };
        self.status.set_text(&if cut {
            format!("Cut {what} — paste (Ctrl+V) in another folder to move")
        } else {
            format!("Copied {what}")
        });
    }

    pub(super) fn paste_into(self: &Rc<Self>, dest: gio::File) {
        let Some(dest_path) = dest.path() else {
            self.status.set_text("Can only paste into local folders");
            return;
        };
        let cb = self.window.clipboard();
        let a = self.clone();
        glib::spawn_future_local(async move {
            let Some((cut, files)) = fileops::read_clipboard(&cb).await else {
                a.status.set_text("Nothing to paste — copy or cut some files first");
                return;
            };
            // Cleared after a complete move, so pasting again doesn't look for files that are gone. Kept when some
            // were skipped or already there, since those are still where they were.
            fileops::start_transfer(&a, files, dest_path, cut, move |o, cancelled| {
                if cut && o.errors.is_empty() && !cancelled && o.skipped == 0 && o.already == 0 {
                    let _ = cb.set_content(None::<&gdk::ContentProvider>);
                }
            });
        });
    }

    // ================= other file operations =================
    pub(super) fn prompt(self: &Rc<Self>, title: &str, initial: &str, select: usize, ok: &str, f: impl Fn(String) -> Result<(), String> + 'static) {
        let win = gtk::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title(title)
            .default_width(400)
            .resizable(false)
            .build();
        win.add_css_class("rfiles");
        let entry = gtk::Entry::builder().text(initial).build();
        let err = gtk::Label::new(None);
        err.add_css_class("error");
        err.set_xalign(0.0);
        err.set_visible(false);
        let cancel = gtk::Button::with_label("Cancel");
        let okb = gtk::Button::with_label(ok);
        okb.add_css_class("suggested-action");
        let btns = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        btns.set_halign(gtk::Align::End);
        btns.append(&cancel);
        btns.append(&okb);
        let bx = gtk::Box::new(gtk::Orientation::Vertical, 12);
        bx.set_margin_top(18);
        bx.set_margin_bottom(18);
        bx.set_margin_start(18);
        bx.set_margin_end(18);
        bx.append(&entry);
        bx.append(&err);
        bx.append(&btns);
        win.set_child(Some(&bx));
        // Escape cancels like the Cancel button; bubble phase so the entry's own popups get it first
        let keys = gtk::ShortcutController::new();
        keys.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::NamedAction::new("window.close")),
        ));
        win.add_controller(keys);

        let f = Rc::new(f);
        let submit = {
            let (win, entry, err) = (win.downgrade(), entry.downgrade(), err.downgrade());
            Rc::new(move || {
                let (Some(win), Some(entry), Some(err)) = (win.upgrade(), entry.upgrade(), err.upgrade()) else { return };
                let t = entry.text().trim().to_string();
                if t.is_empty() {
                    return;
                }
                match f(t) {
                    Ok(()) => win.close(),
                    Err(e) => {
                        err.set_text(&e);
                        err.set_visible(true);
                    }
                }
            })
        };
        let s = submit.clone();
        okb.connect_clicked(move |_| s());
        entry.connect_activate(move |_| submit());
        let w = win.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(w) = w.upgrade() {
                w.close()
            }
        });
        win.present();
        entry.grab_focus();
        entry.select_region(0, select as i32);
    }

    pub(super) fn in_recent(&self) -> bool {
        self.current_dir().is_some_and(|d| d.uri().starts_with("recent:"))
    }

    /// Moves the selection to the trash, or deletes it permanently after asking (always so in the trash).
    pub(super) fn delete_selected(self: &Rc<Self>, permanent: bool) {
        let recent = self.in_recent();
        let items: Vec<(gio::File, String)> = self
            .selected_infos()
            .iter()
            .map(|i| {
                // In Recent the real file, not its recent:/// entry. Elsewhere the item itself: in the trash
                // that is the trash:/// item, so GIO also drops its trash record.
                let f = if recent {
                    item_file(i)
                } else {
                    i.attribute_object("standard::file").and_downcast::<gio::File>().unwrap_or_else(|| item_file(i))
                };
                (f, i.display_name().to_string())
            })
            .collect();
        if permanent || self.in_trash() {
            crate::trash::confirm_delete(self, items);
        } else {
            crate::trash::move_to_trash(self, items);
        }
    }

    /// Removes the selected entries from Recent, leaving the files alone.
    pub(super) fn forget_recent_selected(self: &Rc<Self>) {
        if !self.in_recent() {
            return;
        }
        let items: Vec<(gio::File, String)> = self
            .selected_infos()
            .iter()
            .filter_map(|i| {
                let f = i.attribute_object("standard::file").and_downcast::<gio::File>()?;
                Some((f, i.display_name().to_string()))
            })
            .collect();
        crate::trash::forget_recent(self, items);
    }

    /// Moves the selected trash items back to where they were deleted from.
    pub(super) fn restore_selected(self: &Rc<Self>) {
        if !self.in_trash() {
            return;
        }
        let infos = self.selected_infos();
        let origs: Vec<Option<PathBuf>> = infos.iter().map(crate::trash::orig_path).collect();
        if !crate::trash::can_restore(&origs) {
            if !infos.is_empty() {
                self.status.set_text("Only items at the top of the trash can be restored");
            }
            return;
        }
        // The trash:/// item itself, so GIO also removes its trash record
        let items: Vec<(gio::File, PathBuf, bool)> = infos
            .iter()
            .zip(origs.into_iter().flatten())
            .map(|(i, orig)| {
                let f = i
                    .attribute_object("standard::file")
                    .and_downcast::<gio::File>()
                    .unwrap_or_else(|| item_file(i));
                (f, orig, i.file_type() == gio::FileType::Directory)
            })
            .collect();
        let busy = ntr(items.len(), "Restoring {n} item…", "Restoring {n} items…");
        self.status.set_text(&busy);
        // Runs on after the window closes; the result then goes elsewhere
        let job = crate::jobs::start(&busy, None);
        let a = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let n = match gio::spawn_blocking(move || crate::trash::restore(items)).await {
                Ok(o) => crate::banner::Notice { text: crate::trash::restore_message(&o), errors: o.errors },
                Err(_) => crate::banner::Notice::stopped("Restore failed"),
            };
            crate::jobs::deliver(&a, n);
            drop(job);
        });
    }

    pub(super) fn toggle_star(self: &Rc<Self>) {
        let infos = self.selected_infos();
        if infos.is_empty() {
            return;
        }
        let mut skipped = false;
        {
            let mut c = self.config.borrow_mut();
            for info in &infos {
                let uri = item_file(info).uri().to_string();
                if c.starred.contains(&uri) {
                    c.starred.retain(|u| u != &uri);
                } else if crate::config::storable(&uri) {
                    c.starred.push(uri);
                } else {
                    skipped = true;
                }
            }
        }
        self.save_config();
        Self::stars_changed();
        if skipped {
            self.status.set_text("Items with a line break in their address can't be starred");
        }
    }

    /// Forgets starred files that are gone for good, so the saved list doesn't keep dead
    /// entries forever. The view already leaves them out; this only cleans the config.
    pub(super) fn drop_dead_stars(self: &Rc<Self>) {
        let uris = self.config.borrow().starred.clone();
        if uris.is_empty() {
            return;
        }
        let a = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(dead) = gio::spawn_blocking(move || {
                uris.into_iter().filter(|u| crate::config::star_is_dead(u)).collect::<Vec<_>>()
            })
            .await
            else {
                return;
            };
            let Some(a) = a.upgrade() else { return };
            if dead.is_empty() {
                return;
            }
            a.config.borrow_mut().starred.retain(|u| !dead.contains(u));
            a.save_config();
        });
    }
}

/// Why `name` can't be a new folder's name, if it can't. `child()` would read "a/b" or ".." as a path and
/// create the folder somewhere else, so these are refused before it is called.
pub(super) fn folder_name_problem(name: &str) -> Option<&'static str> {
    if name == "." || name == ".." {
        // Same wording as batch rename
        Some("“.” and “..” are not allowed")
    } else if name.contains(['/', '\0']) {
        // A NUL can't come from a GTK entry; refused anyway since it would cut the name short
        Some("The name can't contain “/”")
    } else {
        None
    }
}

#[cfg(test)]
mod folder_name_tests {
    use super::folder_name_problem;

    #[test]
    fn folder_names() {
        for ok in ["New Folder", "a.b", "...", ".hidden", "x~"] {
            assert_eq!(folder_name_problem(ok), None, "{ok}");
        }
        for bad in ["a/b", "../x", "/", "x/", "./", "a\0b"] {
            assert_eq!(folder_name_problem(bad), Some("The name can't contain “/”"), "{bad:?}");
        }
        for bad in [".", ".."] {
            assert_eq!(folder_name_problem(bad), Some("“.” and “..” are not allowed"), "{bad:?}");
        }
    }

    #[test]
    fn accepted_names_stay_in_the_folder() {
        use gtk::gio;
        use gtk::prelude::*;
        let dir = gio::File::for_path(std::env::temp_dir().join("rfiles-test-dir"));
        for name in ["New Folder", "...", ".hidden", "a b"] {
            assert_eq!(folder_name_problem(name), None);
            let child = dir.child(name);
            assert_eq!(child.parent().map(|p| p.equal(&dir)), Some(true), "{name}");
            assert_eq!(child.basename().as_deref(), Some(std::path::Path::new(name)));
        }
    }
}
