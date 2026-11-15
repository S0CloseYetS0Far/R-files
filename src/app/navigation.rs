use super::*;

impl App {
    // ================= navigation =================
    pub fn navigate(self: &Rc<Self>, loc: Loc) {
        self.navigate_with(loc, Nav::Fresh);
    }

    fn navigate_with(self: &Rc<Self>, loc: Loc, nav: Nav) {
        let saved = self.capture_view();
        {
            let mut tabs = self.tabs.borrow_mut();
            let t = &mut tabs[self.current.get()];
            let view = arrival_view(nav, None, &t.loc, &loc);
            t.view = view;
            if t.loc.key() == loc.key() && !matches!(loc, Loc::Search { .. }) {
                drop(tabs);
                self.load_current();
                return;
            }
            let old = std::mem::replace(&mut t.loc, loc);
            t.back.push((old, Some(saved)));
            t.fwd.clear();
        }
        self.load_current();
    }

    pub(super) fn go_back(self: &Rc<Self>) {
        let saved = self.capture_view();
        {
            let mut tabs = self.tabs.borrow_mut();
            let t = &mut tabs[self.current.get()];
            let Some((prev, view)) = t.back.pop() else { return };
            let cur = std::mem::replace(&mut t.loc, prev);
            t.view = arrival_view(Nav::Back, view, &cur, &t.loc);
            t.fwd.push((cur, Some(saved)));
        }
        self.load_current();
    }

    pub(super) fn go_forward(self: &Rc<Self>) {
        let saved = self.capture_view();
        {
            let mut tabs = self.tabs.borrow_mut();
            let t = &mut tabs[self.current.get()];
            let Some((next, view)) = t.fwd.pop() else { return };
            let cur = std::mem::replace(&mut t.loc, next);
            t.view = arrival_view(Nav::Forward, view, &cur, &t.loc);
            t.back.push((cur, Some(saved)));
        }
        self.load_current();
    }

    pub(super) fn go_up(self: &Rc<Self>) {
        match self.current_loc() {
            Loc::Dir(f) => {
                if let Some(p) = f.parent() {
                    self.navigate_with(Loc::Dir(p), Nav::Up);
                }
            }
            Loc::Search { root, .. } => self.navigate_with(Loc::Dir(root), Nav::Up),
            Loc::Starred => {}
        }
    }

    pub(super) fn load_current(self: &Rc<Self>) {
        // Any older restore is for a place being replaced
        self.drop_restore();
        let view = self.tabs.borrow_mut()[self.current.get()].view.take();
        let (loc, can_back, can_fwd) = {
            let tabs = self.tabs.borrow();
            let t = &tabs[self.current.get()];
            set_tab_title(&t.widget, &t.label, &t.loc.title());
            t.icon.set_icon_name(Some(t.loc.icon()));
            t.widget.set_tooltip_text(Some(&match &t.loc {
                Loc::Dir(f) => display_path(f),
                other => other.title(),
            }));
            (t.loc.clone(), !t.back.is_empty(), !t.fwd.is_empty())
        };
        self.refresh_tab_titles();
        self.search_gen.set(self.search_gen.get() + 1);
        self.cancel_search();
        self.back_btn.set_sensitive(can_back);
        self.fwd_btn.set_sensitive(can_fwd);
        *self.previewed.borrow_mut() = None;
        self.preview.clear();

        let new_query = match &loc {
            Loc::Dir(f) => {
                self.filter_model.set_model(Some(&self.dir_list));
                self.dir_list.set_file(None::<&gio::File>);
                self.dir_list.set_file(Some(f));
                self.up_btn.set_sensitive(f.parent().is_some());
                String::new()
            }
            Loc::Starred => {
                let files: Vec<_> = self.config.borrow().starred.iter().map(|u| gio::File::for_uri(u)).collect();
                let store = infos_store(files.into_iter());
                self.filter_model.set_model(Some(&store));
                self.drop_dead_stars();
                self.up_btn.set_sensitive(false);
                String::new()
            }
            Loc::Search { query, results, .. } => {
                let store = infos_store(results.iter().map(gio::File::for_path));
                self.filter_model.set_model(Some(&store));
                self.up_btn.set_sensitive(true);
                query.clone()
            }
        };
        self.path_entry.set_text(&loc_path_text(&loc));
        *self.query.borrow_mut() = new_query.to_lowercase();
        if self.search.text() != new_query {
            self.search.set_text(&new_query);
        }
        self.filter.changed(gtk::FilterChange::Different);
        self.selection.unselect_all();
        self.grid_scroll.vadjustment().set_value(0.0);
        self.list_scroll.vadjustment().set_value(0.0);
        self.window.set_title(Some(&format!("{} — Files", loc.title())));
        self.highlight_sidebar(&loc);
        self.update_pin_button();
        self.update_status();

        *self.restore.borrow_mut() = view.map(Restore::from);
        // Starred and search results are already there; a folder restores when loaded
        if !matches!(loc, Loc::Dir(_)) || !self.dir_list.is_loading() {
            self.apply_restore();
        }
    }

    /// Selects and scrolls to `name` in the current folder as soon as it
    /// has finished loading (used when RFiles is asked to show a file).
    pub fn select_when_loaded(self: &Rc<Self>, name: String) {
        *self.pending_select.borrow_mut() = Some(name);
        if !self.dir_list.is_loading() {
            self.apply_pending_select();
        }
    }

    pub(super) fn apply_pending_select(&self) {
        let Some(name) = self.pending_select.borrow_mut().take() else { return };
        // Showing this file replaces any remembered view
        self.drop_restore();
        // Look in the unfiltered folder, so a file the filter hides is
        // made visible instead of silently not selected.
        let Some(all) = self.filter_model.model() else { return };
        let Some(target) = (0..all.n_items())
            .filter_map(|i| all.item(i).and_downcast::<gio::FileInfo>())
            .find(|info| info.name().to_string_lossy() == name)
        else {
            return;
        };
        let (show_hidden, clear_query) = reveal_changes(
            self.show_hidden.get(),
            target.is_hidden() || target.is_backup(),
            &self.query.borrow(),
            &target.display_name(),
        );
        if clear_query {
            // Clear the query first so the entry's search-changed is a no-op
            self.query.borrow_mut().clear();
            self.search.set_text("");
            self.filter.changed(gtk::FilterChange::LessStrict);
        }
        if show_hidden {
            // Through the button, so it, the filter and the saved setting stay in step
            self.hidden_btn.set_active(true);
        }
        for pos in 0..self.selection.n_items() {
            let Some(info) = self.selection.item(pos).and_downcast::<gio::FileInfo>() else { continue };
            if info.name().to_string_lossy() == name {
                // Select and focus in the visible view; the other only scrolls
                let flags = gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT;
                let none = gtk::ListScrollFlags::NONE;
                let (grid_flags, list_flags) = if self.view_btn.is_active() { (none, flags) } else { (flags, none) };
                self.scroll_views(pos, grid_flags, list_flags);
                break;
            }
        }
    }

    /// Stops a closing window's work right away: handlers on shared
    /// objects (the volume monitor) go, folder monitoring and any playing
    /// preview stop. The rest is freed when the window is destroyed.
    pub(super) fn teardown(&self) {
        for (obj, id) in self.model_handlers.borrow_mut().drain(..) {
            obj.disconnect(id);
        }
        self.dir_list.set_file(None::<&gio::File>);
        self.preview.clear();
        self.cancel_search();
    }

    pub(crate) fn reload(self: &Rc<Self>) {
        self.save_view();
        self.load_current();
    }
}

/// What "Show in folder" must change so its target passes the filter:
/// (turn hidden files on, clear the search). `query` is lowercase.
fn reveal_changes(show_hidden: bool, target_hidden: bool, query: &str, display_name: &str) -> (bool, bool) {
    let need_hidden = target_hidden && !show_hidden;
    let need_clear = !query.is_empty() && !display_name.to_lowercase().contains(query);
    (need_hidden, need_clear)
}

#[cfg(test)]
mod reveal_tests {
    use super::reveal_changes;

    #[test]
    fn hidden_target_turns_hidden_files_on() {
        assert_eq!(reveal_changes(false, true, "", ".bashrc"), (true, false));
        assert_eq!(reveal_changes(true, true, "", ".bashrc"), (false, false));
        assert_eq!(reveal_changes(false, false, "", "notes.txt"), (false, false));
    }

    #[test]
    fn search_that_hides_target_is_cleared() {
        assert_eq!(reveal_changes(false, false, "zzz", "notes.txt"), (false, true));
        assert_eq!(reveal_changes(false, false, "note", "Notes.txt"), (false, false));
        assert_eq!(reveal_changes(false, true, "zzz", "file~"), (true, true));
    }
}
