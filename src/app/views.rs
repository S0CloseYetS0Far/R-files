use super::*;

impl App {
    // ================= model / filtering =================
    pub(super) fn setup_filter(self: &Rc<Self>) {
        let a = Rc::downgrade(self);
        self.filter.set_filter_func(move |obj| {
            let Some(a) = a.upgrade() else { return false };
            let Some(info) = obj.downcast_ref::<gio::FileInfo>() else { return false };
            if !a.show_hidden.get() && (info.is_hidden() || info.is_backup()) {
                return false;
            }
            let q = a.query.borrow();
            // Results of a search inside files needn't have the query in their names
            if a.content_query.borrow().as_deref() == Some(q.as_str()) && matches!(a.current_loc(), Loc::Search { .. }) {
                return true;
            }
            q.is_empty() || info.display_name().to_lowercase().contains(q.as_str())
        });
    }

    // ================= views =================
    pub(super) fn setup_views(self: &Rc<Self>) {
        // Grid view tiles
        let f = gtk::SignalListItemFactory::new();
        let a = Rc::downgrade(self);
        f.connect_setup(move |_, obj| {
            let Some(a) = a.upgrade() else { return };
            let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let bx = gtk::Box::new(gtk::Orientation::Vertical, 4);
            bx.add_css_class("tile");
            let img = gtk::Image::new();
            let lbl = gtk::Label::builder()
                .wrap(true)
                .wrap_mode(pango::WrapMode::WordChar)
                .lines(2)
                .ellipsize(pango::EllipsizeMode::Middle)
                .justify(gtk::Justification::Center)
                .build();
            bx.append(&img);
            bx.append(&lbl);
            li.set_child(Some(&bx));
            a.attach_item_gesture(&bx, li);
        });
        f.connect_bind(|_, obj| {
            let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(info) = li.item().and_downcast::<gio::FileInfo>() else { return };
            let bx = li.child().and_downcast::<gtk::Box>().unwrap();
            let img = bx.first_child().and_downcast::<gtk::Image>().unwrap();
            let lbl = img.next_sibling().and_downcast::<gtk::Label>().unwrap();
            let size = ICON_SIZE.with(|s| s.get());
            img.set_pixel_size(size);
            let chars = (size / 5 + 3).clamp(10, 16);
            lbl.set_width_chars(chars);
            lbl.set_max_width_chars(chars);
            set_icon(&img, &info, true);
            crate::thumbs::bind(li, &img, &info);
            lbl.set_text(&info.display_name());
            bx.set_opacity(if info.is_hidden() { 0.55 } else { 1.0 });
        });
        self.grid.set_factory(Some(&f));
        crate::thumbs::watch_scroll(&self.grid_scroll);
        crate::thumbs::watch_scroll(&self.list_scroll);

        // Keep the column limit just above what fits in the current width
        let grid = self.grid.downgrade();
        self.grid_scroll.hadjustment().connect_page_size_notify(move |adj| {
            let Some(g) = grid.upgrade() else { return };
            let cols = grid_columns_for_width(adj.page_size());
            if cols != g.max_columns() {
                // Not during the size allocation that told us the width
                let g = g.downgrade();
                glib::idle_add_local_once(move || {
                    if let Some(g) = g.upgrade() {
                        g.set_max_columns(cols);
                    }
                });
            }
        });

        // List view columns
        self.add_column("Name", 0, true);
        self.add_column("Size", 1, false);
        self.add_column("Modified", 2, false);
        self.add_column("Type", 3, false);
        let (key, desc) = self.sort.get();
        self.set_sort(key, desc);
        if let Some(s) = self.list.sorter() {
            let a = Rc::downgrade(self);
            s.connect_changed(move |s, _| {
                if let Some(a) = a.upgrade() {
                    a.header_sort_changed(s);
                }
            });
        }

        for scroll in [&self.grid_scroll, &self.list_scroll] {
            // Right-click on empty space
            let g = gtk::GestureClick::new();
            g.set_button(3);
            let a = Rc::downgrade(self);
            let s = scroll.downgrade();
            g.connect_pressed(move |_, _, x, y| {
                let Some(a) = a.upgrade() else { return };
                let Some(s) = s.upgrade() else { return };
                a.selection.unselect_all();
                a.popup_menu(&s, x, y, a.background_menu());
            });
            scroll.add_controller(g);

            // Keys on the file views
            let k = gtk::EventControllerKey::new();
            let a = Rc::downgrade(self);
            let s = scroll.downgrade();
            k.connect_key_pressed(move |_, key, _, mods| {
                let Some(a) = a.upgrade() else { return glib::Propagation::Proceed };
                let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
                let alt = mods.contains(gdk::ModifierType::ALT_MASK);
                if is_menu_key(key, mods) {
                    if let Some(s) = s.upgrade() {
                        a.keyboard_menu(&s);
                    }
                    return glib::Propagation::Stop;
                }
                // Escape first dismisses a result banner, then clears the selection
                if key == gdk::Key::Escape && a.banner.dismiss() {
                    return glib::Propagation::Stop;
                }
                let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
                let action = match key {
                    gdk::Key::Delete if shift && !ctrl && !alt => "delete-permanently",
                    gdk::Key::Delete if !ctrl && !alt => "trash",
                    gdk::Key::F2 => "rename",
                    gdk::Key::BackSpace if !ctrl && !alt => "back",
                    gdk::Key::c | gdk::Key::C if ctrl => "copy",
                    gdk::Key::x | gdk::Key::X if ctrl => "cut",
                    gdk::Key::v | gdk::Key::V if ctrl => "paste",
                    gdk::Key::a | gdk::Key::A if ctrl => "select-all",
                    // Here and not an application shortcut, so text entries keep Ctrl+Z for their own undo
                    gdk::Key::z | gdk::Key::Z if ctrl && !alt && shift => "redo",
                    gdk::Key::z | gdk::Key::Z if ctrl && !alt => "undo",
                    gdk::Key::Escape => "select-none",
                    _ => return glib::Propagation::Proceed,
                };
                a.activate(action);
                glib::Propagation::Stop
            });
            scroll.add_controller(k);
            crate::dnd::attach_view(self, scroll);
        }

        let a = Rc::downgrade(self);
        self.grid.connect_activate(move |_, pos| {
            if let Some(a) = a.upgrade() {
                a.open_position(pos)
            }
        });
        let a = Rc::downgrade(self);
        self.list.connect_activate(move |_, pos| {
            if let Some(a) = a.upgrade() {
                a.open_position(pos)
            }
        });
    }

    fn add_column(self: &Rc<Self>, title: &str, kind: u8, expand: bool) {
        let f = gtk::SignalListItemFactory::new();
        let a = Rc::downgrade(self);
        f.connect_setup(move |_, obj| {
            let Some(a) = a.upgrade() else { return };
            let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let bx = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            if kind == 0 {
                let img = gtk::Image::new();
                img.set_pixel_size(32);
                bx.append(&img);
            }
            let lbl = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(pango::EllipsizeMode::End)
                .hexpand(true)
                .build();
            if kind != 0 {
                lbl.add_css_class("dim");
            }
            bx.append(&lbl);
            li.set_child(Some(&bx));
            a.attach_item_gesture(&bx, li);
        });
        f.connect_bind(move |_, obj| {
            let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(info) = li.item().and_downcast::<gio::FileInfo>() else { return };
            let bx = li.child().and_downcast::<gtk::Box>().unwrap();
            let lbl = bx.last_child().and_downcast::<gtk::Label>().unwrap();
            match kind {
                0 => {
                    let img = bx.first_child().and_downcast::<gtk::Image>().unwrap();
                    set_icon(&img, &info, true);
                    crate::thumbs::bind(li, &img, &info);
                    lbl.set_text(&info.display_name());
                }
                1 => lbl.set_text(&format_size(&info)),
                2 => lbl.set_text(&format_time(&info)),
                _ => lbl.set_text(&type_description(&info)),
            }
        });
        let col = gtk::ColumnViewColumn::new(Some(title), Some(f));
        col.set_expand(expand);
        col.set_resizable(true);
        if !expand {
            col.set_fixed_width(if kind == 3 { 170 } else if kind == 2 { 190 } else { 140 });
        }
        col.set_id(Some(SortKey::ALL[kind as usize].as_str()));
        // Only makes the header clickable and show the arrow: the model is sorted by
        // `App::sorter`, because a reversed column sorter would put folders last.
        col.set_sorter(Some(&gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal)));
        self.list.append_column(&col);
    }

    /// Sorts both views. Goes through the list view's header sorter so its arrow
    /// always matches; `header_sort_changed` does the rest.
    pub(super) fn set_sort(&self, key: SortKey, desc: bool) {
        let cols = self.list.columns();
        let col = (0..cols.n_items())
            .filter_map(|i| cols.item(i).and_downcast::<gtk::ColumnViewColumn>())
            .find(|c| c.id().as_deref() == Some(key.as_str()));
        let order = if desc { gtk::SortType::Descending } else { gtk::SortType::Ascending };
        self.list.sort_by_column(col.as_ref(), order);
    }

    /// Shows one view and gives only that one the model: a view in a hidden stack page
    /// still builds a few hundred rows or tiles, which cost about 4 MB (#55).
    pub(super) fn show_view(&self, list: bool) {
        self.view_stack.set_visible_child_name(if list { "list" } else { "grid" });
        let model = Some(&self.selection);
        let (grid_model, list_model) = if list { (None, model) } else { (model, None) };
        self.grid.set_model(grid_model);
        self.list.set_model(list_model);
        // The view that was hidden did not follow the selection: bring it into view
        let sel = self.selection.selection();
        if !sel.is_empty() {
            self.scroll_views(sel.minimum(), gtk::ListScrollFlags::NONE, gtk::ListScrollFlags::NONE);
        }
    }

    /// Scrolls the view that has the model (the hidden one has none).
    pub(super) fn scroll_views(&self, pos: u32, grid_flags: gtk::ListScrollFlags, list_flags: gtk::ListScrollFlags) {
        if self.grid.model().is_some() {
            self.grid.scroll_to(pos, grid_flags, None);
        }
        if self.list.model().is_some() {
            self.list.scroll_to(pos, None::<&gtk::ColumnViewColumn>, list_flags, None);
        }
    }

    /// A header click or `set_sort`: re-sorts the model in place (no reload; the
    /// selection follows its items) and remembers the choice.
    fn header_sort_changed(&self, s: &gtk::Sorter) {
        let Some(s) = s.downcast_ref::<gtk::ColumnViewSorter>() else { return };
        let key = s.primary_sort_column().and_then(|c| c.id()).and_then(|id| SortKey::parse(&id));
        let desc = key.is_some() && s.primary_sort_order() == gtk::SortType::Descending;
        let state = (key.unwrap_or(SortKey::Name), desc);
        if self.sort.replace(state) == state {
            return;
        }
        self.sorter.changed(gtk::SorterChange::Different);
        {
            let mut c = self.config.borrow_mut();
            (c.sort, c.sort_desc) = state;
        }
        self.save_config();
    }

    /// Right-click (menu) and middle-click (open in new tab) on a file.
    fn attach_item_gesture(self: &Rc<Self>, widget: &gtk::Box, li: &gtk::ListItem) {
        let g = gtk::GestureClick::new();
        g.set_button(0);
        let a = Rc::downgrade(self);
        let weak = li.downgrade();
        let w = widget.downgrade();
        g.connect_pressed(move |g, _, x, y| {
            let Some(a) = a.upgrade() else { return };
            let Some(w) = w.upgrade() else { return };
            let Some(li) = weak.upgrade() else { return };
            let Some(info) = li.item().and_downcast::<gio::FileInfo>() else { return };
            match g.current_button() {
                3 => {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    // Keep a multi-selection if you right-click inside it
                    if !a.selection.is_selected(li.position()) {
                        a.selection.select_item(li.position(), true);
                    }
                    // Attach the menu to the view, not the tile: the theme's
                    // grid-tile spacing would otherwise stretch the menu.
                    let p = w
                        .compute_point(&a.view_stack, &gtk::graphene::Point::new(x as f32, y as f32))
                        .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
                    a.popup_menu(&a.view_stack, p.x() as f64, p.y() as f64, a.item_menu(&info));
                }
                2 => {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    if is_dir_like(&info) {
                        a.new_tab(Loc::Dir(item_file(&info)));
                    }
                }
                _ => {}
            }
        });
        widget.add_controller(g);
        crate::dnd::attach_item(self, widget, li);
    }

    pub(super) fn on_selection_changed(self: &Rc<Self>) {
        // A hidden preview pane does no work; showing it previews the selection
        if !self.preview.root.get_visible() {
            self.update_status();
            return;
        }
        let infos = self.selected_infos();
        match infos.len() {
            0 => {
                if self.previewed.borrow_mut().take().is_some() {
                    self.preview.clear();
                }
            }
            1 => {
                let key = item_file(&infos[0]).uri().to_string();
                // Don't restart the preview (e.g. a playing video) if the
                // same single file is still selected
                if self.previewed.borrow().as_deref() != Some(key.as_str()) {
                    *self.previewed.borrow_mut() = Some(key);
                    self.preview.show(&infos[0], self.show_hidden.get());
                }
            }
            n => {
                *self.previewed.borrow_mut() = Some(format!("multi:{n}"));
                let size: u64 = infos
                    .iter()
                    .filter(|i| !is_dir_like(i))
                    .map(|i| i.size().max(0) as u64)
                    .sum();
                self.preview.show_summary(n, size);
            }
        }
        self.update_status();
    }

    pub(super) fn update_status(&self) {
        let n = self.selection.n_items();
        let mut s = format!("{n} item{}", if n == 1 { "" } else { "s" });
        if matches!(self.current_loc(), Loc::Dir(_)) && self.dir_list.is_loading() {
            s.push_str("  ·  loading…");
        }
        if let Loc::Search { end, .. } = self.current_loc() {
            if let Some(note) = end.note() {
                s.push_str(&format!("  ·  {note}"));
            }
        }
        let sel = self.selected_infos();
        if sel.len() == 1 {
            s.push_str(&format!("  ·  “{}” selected", sel[0].display_name()));
            let size = format_size(&sel[0]);
            if !size.is_empty() {
                s.push_str(&format!(" ({size})"));
            }
        } else if sel.len() > 1 {
            s.push_str(&format!("  ·  {} selected", sel.len()));
        }
        self.status.set_text(&s);
        self.update_empty_view();
    }

    /// Shows or hides the "nothing here" message over the file view.
    pub(super) fn update_empty_view(&self) {
        let loc = self.current_loc();
        let place = match &loc {
            Loc::Dir(f) if f.uri().trim_end_matches('/') == "trash:" => Place::Trash,
            Loc::Dir(_) => Place::Dir,
            Loc::Starred => Place::Starred,
            Loc::RecentDownloads => Place::Dir,
            Loc::Search { .. } => Place::Search,
        };
        let dir = matches!(loc, Loc::Dir(_));
        let error = if dir { self.dir_list.error().map(|e| e.message().to_string()) } else { None };
        let total = self.filter_model.model().map_or(0, |m| m.n_items());
        let msg = empty_message(
            place,
            dir && self.dir_list.is_loading(),
            error.as_deref(),
            total,
            self.filter_model.n_items(),
            &self.query.borrow(),
        );
        match msg {
            Some((icon, text)) => {
                self.empty_icon.set_icon_name(Some(icon));
                self.empty_label.set_text(&text);
                self.empty_box.set_visible(true);
            }
            None => self.empty_box.set_visible(false),
        }
    }
}

/// What kind of place the file view shows, for the empty-view message.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Place {
    Dir,
    Trash,
    Starred,
    Search,
}

/// The icon and text to show when the view has nothing in it, or None
/// while it has items or is still loading. `total` counts the items before
/// filtering, `shown` after it, `query` is the filter text.
fn empty_message(
    place: Place,
    loading: bool,
    error: Option<&str>,
    total: u32,
    shown: u32,
    query: &str,
) -> Option<(&'static str, String)> {
    if shown > 0 || loading {
        return None;
    }
    if let Some(e) = error {
        // GIO says "Error opening directory “/x”: Permission denied"; the
        // path is already in the path bar, so keep just the reason.
        let e = e.rsplit_once(": ").map_or(e, |(_, why)| why);
        return Some(("action-unavailable-symbolic", format!("Can't open this folder: {e}")));
    }
    if total > 0 {
        return Some(if query.is_empty() {
            ("view-conceal-symbolic", "Only hidden files here (Ctrl+H shows them)".to_string())
        } else {
            ("system-search-symbolic", format!("No files match “{query}”"))
        });
    }
    Some(match place {
        Place::Dir => ("folder-symbolic", "Folder is empty".to_string()),
        Place::Trash => ("user-trash-symbolic", "Trash is empty".to_string()),
        Place::Starred => ("starred-symbolic", "Star files to see them here".to_string()),
        Place::Search => ("system-search-symbolic", format!("No results for “{query}”")),
    })
}

#[cfg(test)]
mod empty_view_tests {
    use super::{empty_message, Place};

    #[test]
    fn empty_view_messages() {
        let text = |p, loading, err, total, shown, q| empty_message(p, loading, err, total, shown, q).map(|m| m.1);
        // Nothing while loading or while anything is shown
        assert_eq!(text(Place::Dir, true, None, 0, 0, ""), None);
        assert_eq!(text(Place::Dir, false, None, 3, 3, ""), None);
        assert_eq!(text(Place::Dir, false, None, 3, 1, "a"), None);
        assert_eq!(text(Place::Dir, false, None, 0, 0, "").as_deref(), Some("Folder is empty"));
        assert_eq!(text(Place::Trash, false, None, 0, 0, "").as_deref(), Some("Trash is empty"));
        assert_eq!(text(Place::Starred, false, None, 0, 0, "").as_deref(), Some("Star files to see them here"));
        assert_eq!(text(Place::Search, false, None, 0, 0, "rep").as_deref(), Some("No results for “rep”"));
        assert_eq!(text(Place::Dir, false, None, 4, 0, "rep").as_deref(), Some("No files match “rep”"));
        assert_eq!(text(Place::Search, false, None, 4, 0, "x").as_deref(), Some("No files match “x”"));
        assert!(text(Place::Dir, false, None, 2, 0, "").unwrap().contains("hidden"));
        assert_eq!(
            text(Place::Dir, false, Some("Permission denied"), 0, 0, "").as_deref(),
            Some("Can't open this folder: Permission denied")
        );
        assert_eq!(
            text(Place::Dir, false, Some("Error opening directory “/a: b”: Permission denied"), 0, 0, "").as_deref(),
            Some("Can't open this folder: Permission denied")
        );
    }
}
