use super::*;

/// Screen-reader name for an icon-only button: the tooltip without its shortcut, "Back (Alt+Left)" -> "Back".
fn accessible_name(tip: &str) -> &str {
    tip.split(" (").next().unwrap_or(tip)
}

#[cfg(test)]
mod accessible_name_tests {
    use super::accessible_name;

    #[test]
    fn strips_the_shortcut() {
        assert_eq!(accessible_name("Back (Alt+Left)"), "Back");
        assert_eq!(accessible_name("List view (Ctrl+2) / grid view (Ctrl+1)"), "List view");
        assert_eq!(accessible_name("Pin this folder"), "Pin this folder");
    }
}

fn icon_button(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tip));
    b.update_property(&[gtk::accessible::Property::Label(accessible_name(tip))]);
    b.add_css_class("flat");
    b
}

fn icon_toggle(icon: &str, tip: &str) -> gtk::ToggleButton {
    let b = gtk::ToggleButton::new();
    b.set_icon_name(icon);
    b.set_tooltip_text(Some(tip));
    b.update_property(&[gtk::accessible::Property::Label(accessible_name(tip))]);
    b.add_css_class("flat");
    b
}

pub fn build(gapp: &gtk::Application, start: gio::File) -> Rc<App> {
    let shared_config = crate::config::shared();
    let config = shared_config.borrow();
    apply_folder_icon_setting(&config.folder_icon);
    ICON_SIZE.with(|s| s.set(config.icon_size));

    let window = gtk::ApplicationWindow::builder()
        .application(gapp)
        .title("Files")
        .icon_name("io.github.rfiles.Files")
        .default_width(config.width)
        .default_height(config.height)
        .build();
    window.add_css_class("rfiles");

    // ---------- title bar: window buttons on top, then tabs [+] ... search ----------
    let controls = gtk::WindowControls::new(gtk::PackType::End);
    controls.set_decoration_layout(Some(":minimize,maximize,close"));
    let app_icon = gtk::Image::from_icon_name("io.github.rfiles.Files");
    app_icon.set_pixel_size(18);
    let title = gtk::Label::new(Some("Files"));
    title.add_css_class("app-title");
    let row1 = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row1.add_css_class("top-row");
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    row1.append(&app_icon);
    row1.append(&title);
    row1.append(&spacer);
    row1.append(&controls);

    // The + sits right after the last tab, like in a browser
    let plus = gtk::Button::from_icon_name("list-add-symbolic");
    plus.add_css_class("flat");
    plus.add_css_class("new-tab");
    plus.set_tooltip_text(Some("New tab (Ctrl+T)"));
    plus.update_property(&[gtk::accessible::Property::Label("New tab")]);
    plus.set_valign(gtk::Align::Center);
    plus.set_action_name(Some("win.new-tab"));
    let tab_strip = gtk::Box::builder().spacing(4).accessible_role(gtk::AccessibleRole::TabList).build();
    tab_strip.append(&plus);
    let tab_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hexpand(true)
        .child(&tab_strip)
        .build();
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search  (Enter = include subfolders)")
        .width_chars(30)
        .valign(gtk::Align::Center)
        .build();
    search.add_css_class("search");
    let row2 = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row2.add_css_class("tab-row");
    row2.append(&tab_scroll);
    row2.append(&search);
    // Opt-in content search lives in a small popover so the toolbar stays as it was
    let search_content = gtk::CheckButton::with_label("Also search inside text files");
    let search_opts = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Search options")
        .valign(gtk::Align::Center)
        .popover(&gtk::Popover::builder().child(&search_content).build())
        .build();
    search_opts.add_css_class("flat");
    search_opts.update_property(&[gtk::accessible::Property::Label("Search options")]);
    row2.append(&search_opts);

    let tb = gtk::Box::new(gtk::Orientation::Vertical, 2);
    tb.add_css_class("titlebar-box");
    tb.append(&row1);
    tb.append(&row2);
    let handle = gtk::WindowHandle::new();
    handle.set_child(Some(&tb));
    window.set_titlebar(Some(&handle));

    // ---------- navigation bar ----------
    let back_btn = icon_button("go-previous-symbolic", "Back (Alt+Left)");
    back_btn.set_action_name(Some("win.back"));
    let fwd_btn = icon_button("go-next-symbolic", "Forward (Alt+Right)");
    fwd_btn.set_action_name(Some("win.forward"));
    let up_btn = icon_button("go-up-symbolic", "Parent folder (Alt+Up)");
    up_btn.set_action_name(Some("win.up"));
    let refresh_btn = icon_button("view-refresh-symbolic", "Reload (F5)");
    refresh_btn.set_action_name(Some("win.refresh"));
    let path_entry = gtk::Entry::builder().hexpand(true).build();
    path_entry.add_css_class("path");
    let path_pop = gtk::Popover::new();
    path_pop.set_has_arrow(false);
    path_pop.set_position(gtk::PositionType::Bottom);
    path_pop.set_parent(&path_entry);
    let term_btn = icon_button("utilities-terminal-symbolic", "Open in Terminal (Ctrl+Alt+T)");
    term_btn.set_action_name(Some("win.terminal-here"));
    let pin_btn = icon_button("view-pin-symbolic", "Pin this folder to the sidebar (Ctrl+D)");
    pin_btn.set_action_name(Some("win.pin-current"));
    let newdir_btn = icon_button("folder-new-symbolic", "New folder (Ctrl+Shift+N)");
    newdir_btn.set_action_name(Some("win.new-folder"));
    let hidden_btn = icon_toggle("view-reveal-symbolic", "Show hidden files (Ctrl+H)");
    let view_btn = icon_toggle("view-list-symbolic", "List view (Ctrl+2) / grid view (Ctrl+1)");
    let sidebar_btn = icon_toggle("sidebar-show-symbolic", "Show sidebar (F9)");
    let preview_btn = icon_toggle("sidebar-show-right-symbolic", "Show preview pane (F4)");
    let settings_btn = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Icons & appearance")
        .build();
    settings_btn.add_css_class("flat");

    let nav = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    nav.add_css_class("navbar");
    for w in [
        sidebar_btn.upcast_ref::<gtk::Widget>(),
        back_btn.upcast_ref(),
        fwd_btn.upcast_ref(),
        up_btn.upcast_ref(),
        refresh_btn.upcast_ref(),
        path_entry.upcast_ref(),
        newdir_btn.upcast_ref(),
        term_btn.upcast_ref(),
        pin_btn.upcast_ref(),
        hidden_btn.upcast_ref(),
        view_btn.upcast_ref(),
        preview_btn.upcast_ref(),
        settings_btn.upcast_ref(),
    ] {
        nav.append(w);
    }

    // ---------- the shared file model (one per window, reused by every tab) ----------
    let dir_list = gtk::DirectoryList::new(Some(ATTRS), None::<&gio::File>);
    let filter = gtk::CustomFilter::new(|_| true);
    let filter_model = gtk::FilterListModel::new(Some(dir_list.clone()), Some(filter.clone()));
    let sort = Rc::new(Cell::new((config.sort, config.sort_desc)));
    let s = sort.clone();
    let keep_order = Rc::new(Cell::new(false));
    let k = keep_order.clone();
    let sorter = gtk::CustomSorter::new(move |a, b| {
        let (Some(a), Some(b)) = (a.downcast_ref::<gio::FileInfo>(), b.downcast_ref::<gio::FileInfo>()) else {
            return gtk::Ordering::Equal;
        };
        // GTK's sort is stable, so Equal keeps the model's own order
        if k.get() {
            return gtk::Ordering::Equal;
        }
        let (key, desc) = s.get();
        compare_infos(a, b, key, desc).into()
    });
    let sort_model = gtk::SortListModel::new(Some(filter_model.clone()), Some(sorter.clone()));
    let selection = gtk::MultiSelection::new(Some(sort_model));

    let grid = gtk::GridView::new(Some(selection.clone()), None::<gtk::ListItemFactory>);
    // GTK keeps about 30 rows x max_columns tiles alive, however many
    // columns really fit, so the limit follows the view's width (see
    // `fit_grid_columns`) instead of being one big number.
    grid.set_max_columns(GRID_START_COLUMNS);
    grid.set_min_columns(1);
    grid.set_enable_rubberband(true);
    grid.add_css_class("files");
    let list = gtk::ColumnView::new(Some(selection.clone()));
    list.add_css_class("files");
    list.set_reorderable(false);
    list.set_enable_rubberband(true);

    let grid_scroll = gtk::ScrolledWindow::builder()
        .child(&grid)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    grid_scroll.add_css_class("files");
    let list_scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).build();
    list_scroll.add_css_class("files");
    let view_stack = gtk::Stack::new();
    view_stack.add_named(&grid_scroll, Some("grid"));
    view_stack.add_named(&list_scroll, Some("list"));
    view_stack.set_vexpand(true);

    // Laid over the views rather than being a stack page, so the grid/list
    // choice is untouched and clicks and drops reach the view underneath
    // (its background menu and drop target).
    let empty_icon = gtk::Image::new();
    empty_icon.set_pixel_size(64);
    let empty_label = gtk::Label::new(None);
    empty_label.set_wrap(true);
    empty_label.set_justify(gtk::Justification::Center);
    empty_label.set_max_width_chars(50);
    let empty_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    empty_box.add_css_class("empty-view");
    empty_box.set_halign(gtk::Align::Center);
    empty_box.set_valign(gtk::Align::Center);
    empty_box.set_can_target(false);
    empty_box.set_visible(false);
    empty_box.append(&empty_icon);
    empty_box.append(&empty_label);
    let view_overlay = gtk::Overlay::new();
    view_overlay.set_child(Some(&view_stack));
    view_overlay.add_overlay(&empty_box);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_ellipsize(pango::EllipsizeMode::End);
    status.add_css_class("statusbar");

    let files_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    files_box.add_css_class("files-area");
    let transfers = gtk::Box::new(gtk::Orientation::Vertical, 0);
    files_box.append(&view_overlay);
    files_box.append(&transfers);
    let banner = crate::banner::Banner::new();
    files_box.append(&banner.row);
    files_box.append(&status);

    // ---------- sidebar ----------
    let sidebar = gtk::ListBox::new();
    sidebar.add_css_class("navigation-sidebar");
    sidebar.set_selection_mode(gtk::SelectionMode::Single);
    let side_scroll = gtk::ScrolledWindow::builder()
        .child(&sidebar)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    side_scroll.add_css_class("sidebar");
    side_scroll.set_size_request(150, -1);

    // ---------- preview ----------
    let preview = Preview::new();
    preview.root.set_size_request(260, -1);

    // ---------- panes (drag the dividers to resize) ----------
    let inner = gtk::Paned::new(gtk::Orientation::Horizontal);
    inner.set_start_child(Some(&files_box));
    inner.set_end_child(Some(&preview.root));
    inner.set_resize_start_child(true);
    inner.set_resize_end_child(false);
    inner.set_shrink_start_child(false);
    inner.set_shrink_end_child(false);

    let outer = gtk::Paned::new(gtk::Orientation::Horizontal);
    outer.set_start_child(Some(&side_scroll));
    outer.set_end_child(Some(&inner));
    outer.set_resize_start_child(false);
    outer.set_shrink_start_child(false);
    outer.set_shrink_end_child(false);
    outer.set_position(config.sidebar_pos);
    outer.set_vexpand(true);

    let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main.append(&nav);
    main.append(&outer);
    window.set_child(Some(&main));

    search.set_key_capture_widget(Some(&files_box));

    let show_hidden = config.show_hidden;
    let list_view = config.list_view;
    let preview_width = config.preview_width;
    let (show_sidebar, show_preview) = (config.show_sidebar, config.show_preview);
    if config.maximized {
        window.maximize();
    }
    drop(config);

    let app = Rc::new(App {
        window,
        tabs: RefCell::new(Vec::new()),
        current: Cell::new(0),
        closed_tabs: RefCell::new(ClosedStack::default()),
        tab_strip,
        tab_scroll,
        plus,
        dir_list,
        filter_model,
        filter,
        selection,
        query: RefCell::new(String::new()),
        show_hidden: Cell::new(show_hidden),
        path_entry,
        path_pop,
        back_btn,
        fwd_btn,
        up_btn,
        hidden_btn,
        view_btn,
        sidebar_btn,
        preview_btn,
        settings_btn,
        pin_btn,
        search,
        search_content,
        content_query: RefCell::new(None),
        status,
        transfers,
        banner,
        view_stack,
        grid,
        list,
        grid_scroll,
        list_scroll,
        empty_box,
        empty_icon,
        empty_label,
        preview,
        previewed: RefCell::new(None),
        sidebar,
        side_scroll,
        preview_hidden_at: Cell::new(0),
        sidebar_targets: RefCell::new(Vec::new()),
        outer,
        inner,
        volumes: gio::VolumeMonitor::get(),
        config: shared_config,
        search_gen: Cell::new(0),
        search_cancel: RefCell::new(None),
        layout_ready: Cell::new(false),
        pending_select: RefCell::new(None),
        restore: RefCell::new(None),
        restore_gen: Cell::new(0),
        model_handlers: RefCell::new(Vec::new()),
        save_pending: Cell::new(false),
        sort,
        sorter,
        keep_order,
        downloads_monitor: RefCell::new(None),
    });

    OPEN.with(|o| o.borrow_mut().push(app.clone()));
    app.setup_filter();
    app.setup_views();
    app.setup_view_restore();
    app.setup_actions(gapp);
    app.setup_signals();
    app.setup_settings_popover();
    app.rebuild_sidebar();

    app.hidden_btn.set_active(show_hidden);
    app.view_btn.set_active(list_view);
    app.show_view(list_view);
    // Applied before the buttons, so their toggled handlers find nothing to change or save
    app.side_scroll.set_visible(show_sidebar);
    app.preview.root.set_visible(show_preview);
    app.sidebar_btn.set_active(show_sidebar);
    app.preview_btn.set_active(show_preview);

    app.new_tab(Loc::Dir(start));
    app.restore_layout(preview_width);
    app.window.present();
    app.grid.grab_focus();
    // Told once, in the first window
    let problems = std::mem::take(&mut app.config.borrow_mut().load_problems);
    if !problems.is_empty() {
        let text = "Couldn't read some settings files; changes to them won't be saved until RFiles is restarted";
        app.notify(crate::banner::Notice { text: text.into(), errors: problems });
    }
    app
}

impl App {
    fn setup_signals(self: &Rc<Self>) {
        let track = |obj: &glib::Object, id: glib::SignalHandlerId| {
            self.model_handlers.borrow_mut().push((obj.clone(), id));
        };
        let a = Rc::downgrade(self);
        track(self.selection.upcast_ref(), self.selection.connect_selection_changed(move |_, _, _| {
            if let Some(a) = a.upgrade() {
                a.on_selection_changed()
            }
        }));
        let a = Rc::downgrade(self);
        track(self.selection.upcast_ref(), self.selection.connect_items_changed(move |_, _, _, _| {
            if let Some(a) = a.upgrade() {
                a.update_status()
            }
        }));
        let a = Rc::downgrade(self);
        let id = self.dir_list.connect_loading_notify(move |dl| {
            let Some(a) = a.upgrade() else { return };
            if !dl.is_loading() {
                a.apply_pending_select();
                a.apply_restore();
            }
            a.update_status();
        });
        track(self.dir_list.upcast_ref(), id);
        let a = Rc::downgrade(self);
        let id = self.dir_list.connect_error_notify(move |dl| {
            let Some(a) = a.upgrade() else { return };
            let Some(err) = dl.error() else { return };
            if err.matches(gio::IOErrorEnum::NotMounted) {
                if let Some(f) = dl.file() {
                    let op = gtk::MountOperation::new(Some(&a.window));
                    let a2 = a.clone();
                    a.status.set_text("Mounting…");
                    f.mount_enclosing_volume(gio::MountMountFlags::NONE, Some(&op), None::<&gio::Cancellable>, move |r| match r {
                        Ok(()) => a2.reload(),
                        Err(e) => a2.status.set_text(&format!("Couldn't mount: {}", e.message())),
                    });
                }
            } else {
                a.status.set_text(&format!("Can't open folder: {}", err.message()));
            }
            a.update_empty_view();
        });
        track(self.dir_list.upcast_ref(), id);

        // Preview "Open" button and double-clicks inside the folder preview
        let a = Rc::downgrade(self);
        *self.preview.on_open.borrow_mut() = Some(Rc::new(move |info: &gio::FileInfo| {
            if let Some(a) = a.upgrade() {
                a.open_info(info)
            }
        }));

        // Sidebar
        let a = Rc::downgrade(self);
        self.sidebar.connect_row_activated(move |_, row| {
            let Some(a) = a.upgrade() else { return };
            if a.refuse_missing_pin(row.index() as usize) {
                // Keep the current place selected, not the dead row
                a.highlight_sidebar(&a.current_loc());
                return;
            }
            if let Some(v) = a.sidebar_volume(row.index() as usize) {
                // Keep the current place selected until the volume is mounted and opened
                a.highlight_sidebar(&a.current_loc());
                a.mount_volume(v, false);
            } else if let Some(loc) = a.sidebar_loc(row.index() as usize) {
                a.navigate(loc);
            }
        });
        for sig in [
            "mount-added",
            "mount-removed",
            "mount-changed",
            "volume-added",
            "volume-removed",
            "volume-changed",
            "drive-connected",
            "drive-disconnected",
        ] {
            let a = Rc::downgrade(self);
            let id = self.volumes.connect_local(sig, false, move |_| {
                let a = a.upgrade()?;
                a.rebuild_sidebar();
                None
            });
            track(self.volumes.upcast_ref(), id);
        }

        self.setup_path_bar();

        // Search box (top right)
        let a = Rc::downgrade(self);
        self.search.connect_search_changed(move |e| {
            let Some(a) = a.upgrade() else { return };
            let q = e.text().to_lowercase();
            if q.is_empty() {
                a.stop_search_by_user();
            }
            if *a.query.borrow() != q {
                *a.query.borrow_mut() = q;
                a.filter.changed(gtk::FilterChange::Different);
            }
        });
        let a = Rc::downgrade(self);
        self.search.connect_activate(move |e| {
            let Some(a) = a.upgrade() else { return };
            let q = e.text().trim().to_string();
            if q.is_empty() {
                return;
            }
            let root = match a.current_loc() {
                Loc::Dir(f) => f,
                Loc::Search { root, .. } => root,
                Loc::RecentDownloads => match crate::util::downloads_dir() {
                    Some(d) => d,
                    None => return,
                },
                Loc::Starred => return,
            };
            a.run_search(root, q);
        });
        let a = Rc::downgrade(self);
        self.search.connect_stop_search(move |e| {
            let Some(a) = a.upgrade() else { return };
            a.stop_search_by_user();
            e.set_text("");
            a.grid.grab_focus();
        });

        // Toggles
        let a = Rc::downgrade(self);
        self.hidden_btn.connect_toggled(move |b| {
            let Some(a) = a.upgrade() else { return };
            a.show_hidden.set(b.is_active());
            a.filter.changed(gtk::FilterChange::Different);
            // The last window to change it sets what new windows start with
            a.config.borrow_mut().show_hidden = b.is_active();
            a.save_config();
        });
        let a = Rc::downgrade(self);
        self.view_btn.connect_toggled(move |b| {
            let Some(a) = a.upgrade() else { return };
            a.show_view(b.is_active());
            a.config.borrow_mut().list_view = b.is_active();
            a.save_config();
        });
        let a = Rc::downgrade(self);
        self.sidebar_btn.connect_toggled(move |b| {
            if let Some(a) = a.upgrade() {
                a.set_sidebar_shown(b.is_active())
            }
        });
        let a = Rc::downgrade(self);
        self.preview_btn.connect_toggled(move |b| {
            if let Some(a) = a.upgrade() {
                a.set_preview_shown(b.is_active())
            }
        });

        let a = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            let Some(a) = a.upgrade() else { return glib::Propagation::Proceed };
            a.save_layout();
            a.teardown();
            // Release the window's data once GTK has finished closing it
            let w = a.window.clone();
            glib::idle_add_local_once(move || {
                OPEN.with(|o| o.borrow_mut().retain(|a| a.window != w));
            });
            glib::Propagation::Proceed
        });
    }
}
