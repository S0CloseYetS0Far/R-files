use crate::config::Config;
use crate::fileops;
use crate::preview::Preview;
use crate::util::*;
use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;

const SEARCH_MAX_RESULTS: usize = 2000;
const SEARCH_MAX_VISITED: usize = 400_000;
/// Column limit of the grid before its width is known.
const GRID_START_COLUMNS: u32 = 16;
/// No tile is narrower than this (the smallest icon size with its label),
/// so `width / GRID_MIN_TILE_PX` columns is always at least what fits.
const GRID_MIN_TILE_PX: f64 = 64.0;

/// The most columns the grid could show in `width` pixels. Used as the
/// grid's column limit: GTK keeps about 30 rows x that many tiles alive.
fn grid_columns_for_width(width: f64) -> u32 {
    ((width / GRID_MIN_TILE_PX).ceil() as u32).clamp(4, 64)
}

const ICON_SIZES: [(i32, &str); 3] = [(40, "Small"), (56, "Medium"), (72, "Large")];

#[derive(Clone)]
pub enum Loc {
    Dir(gio::File),
    Starred,
    Search { root: gio::File, query: String, results: Rc<Vec<PathBuf>> },
}

impl Loc {
    fn title(&self) -> String {
        match self {
            Loc::Dir(f) => file_title(f),
            Loc::Starred => "Starred".into(),
            Loc::Search { query, .. } => format!("“{query}”"),
        }
    }
    fn icon(&self) -> &'static str {
        match self {
            Loc::Dir(f) => icon_for_file(f),
            Loc::Starred => "starred-symbolic",
            Loc::Search { .. } => "system-search-symbolic",
        }
    }
    fn key(&self) -> String {
        match self {
            Loc::Dir(f) => f.uri().trim_end_matches('/').to_string(),
            Loc::Starred => "starred:".into(),
            Loc::Search { query, .. } => format!("search:{query}"),
        }
    }
}

/// One entry of a right-click menu; `None` in a menu list is a separator.
struct MItem {
    label: String,
    action: &'static str,
    target: Option<glib::Variant>,
    accel: &'static str,
}

fn mi(label: impl Into<String>, action: &'static str, accel: &'static str) -> Option<MItem> {
    Some(MItem { label: label.into(), action, target: None, accel })
}

struct Tab {
    loc: Loc,
    back: Vec<Loc>,
    fwd: Vec<Loc>,
    widget: gtk::Box,
    label: gtk::Label,
    icon: gtk::Image,
    close: gtk::Button,
}

/// Where a sidebar row goes.
struct SideTarget {
    loc: Loc,
}

pub struct App {
    pub window: gtk::ApplicationWindow,
    tabs: RefCell<Vec<Tab>>,
    current: Cell<usize>,
    tab_strip: gtk::Box,
    tab_scroll: gtk::ScrolledWindow,
    plus: gtk::Button,
    dir_list: gtk::DirectoryList,
    filter_model: gtk::FilterListModel,
    filter: gtk::CustomFilter,
    pub(crate) selection: gtk::MultiSelection,
    query: RefCell<String>,
    show_hidden: Cell<bool>,
    path_entry: gtk::Entry,
    back_btn: gtk::Button,
    fwd_btn: gtk::Button,
    up_btn: gtk::Button,
    hidden_btn: gtk::ToggleButton,
    view_btn: gtk::ToggleButton,
    settings_btn: gtk::MenuButton,
    pin_btn: gtk::Button,
    search: gtk::SearchEntry,
    pub(crate) status: gtk::Label,
    view_stack: gtk::Stack,
    grid: gtk::GridView,
    list: gtk::ColumnView,
    grid_scroll: gtk::ScrolledWindow,
    list_scroll: gtk::ScrolledWindow,
    preview: Rc<Preview>,
    previewed: RefCell<Option<String>>,
    sidebar: gtk::ListBox,
    sidebar_targets: RefCell<Vec<Option<SideTarget>>>,
    outer: gtk::Paned,
    inner: gtk::Paned,
    volumes: gio::VolumeMonitor,
    config: RefCell<Config>,
    search_gen: Cell<u64>,
    layout_ready: Cell<bool>,
    /// File name to select once the folder has loaded ("Show in folder")
    pending_select: RefCell<Option<String>>,
    /// Handlers on objects that outlive the window (models, the volume
    /// monitor); disconnected on close so a closed window is freed.
    model_handlers: RefCell<Vec<(glib::Object, glib::SignalHandlerId)>>,
    save_pending: Cell<bool>,
}

thread_local! {
    /// Owns every open window's `App`. Signal handlers only hold weak
    /// references, so removing an entry when its window is destroyed frees
    /// the whole window (the process is shared by all windows).
    static OPEN: RefCell<Vec<Rc<App>>> = const { RefCell::new(Vec::new()) };
}

fn icon_button(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tip));
    b.add_css_class("flat");
    b
}

fn icon_toggle(icon: &str, tip: &str) -> gtk::ToggleButton {
    let b = gtk::ToggleButton::new();
    b.set_icon_name(icon);
    b.set_tooltip_text(Some(tip));
    b.add_css_class("flat");
    b
}

pub fn build(gapp: &gtk::Application, start: gio::File) -> Rc<App> {
    let config = Config::load();
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
    plus.set_valign(gtk::Align::Center);
    plus.set_action_name(Some("win.new-tab"));
    let tab_strip = gtk::Box::new(gtk::Orientation::Horizontal, 4);
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
    let term_btn = icon_button("utilities-terminal-symbolic", "Open in Terminal (Ctrl+Alt+T)");
    term_btn.set_action_name(Some("win.terminal-here"));
    let pin_btn = icon_button("view-pin-symbolic", "Pin this folder to the sidebar (Ctrl+D)");
    pin_btn.set_action_name(Some("win.pin-current"));
    let newdir_btn = icon_button("folder-new-symbolic", "New folder (Ctrl+Shift+N)");
    newdir_btn.set_action_name(Some("win.new-folder"));
    let hidden_btn = icon_toggle("view-reveal-symbolic", "Show hidden files (Ctrl+H)");
    let view_btn = icon_toggle("view-list-symbolic", "List view (Ctrl+2) / grid view (Ctrl+1)");
    let settings_btn = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Icons & appearance")
        .build();
    settings_btn.add_css_class("flat");

    let nav = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    nav.add_css_class("navbar");
    for w in [
        back_btn.upcast_ref::<gtk::Widget>(),
        fwd_btn.upcast_ref(),
        up_btn.upcast_ref(),
        refresh_btn.upcast_ref(),
        path_entry.upcast_ref(),
        newdir_btn.upcast_ref(),
        term_btn.upcast_ref(),
        pin_btn.upcast_ref(),
        hidden_btn.upcast_ref(),
        view_btn.upcast_ref(),
        settings_btn.upcast_ref(),
    ] {
        nav.append(w);
    }

    // ---------- the shared file model (one per window, reused by every tab) ----------
    let dir_list = gtk::DirectoryList::new(Some(ATTRS), None::<&gio::File>);
    let filter = gtk::CustomFilter::new(|_| true);
    let filter_model = gtk::FilterListModel::new(Some(dir_list.clone()), Some(filter.clone()));
    let sort_model =
        gtk::SortListModel::new(Some(filter_model.clone()), Some(gtk::CustomSorter::new(sort_infos)));
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

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_ellipsize(pango::EllipsizeMode::End);
    status.add_css_class("statusbar");

    let files_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    files_box.add_css_class("files-area");
    files_box.append(&view_stack);
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
    if config.maximized {
        window.maximize();
    }

    let app = Rc::new(App {
        window,
        tabs: RefCell::new(Vec::new()),
        current: Cell::new(0),
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
        back_btn,
        fwd_btn,
        up_btn,
        hidden_btn,
        view_btn,
        settings_btn,
        pin_btn,
        search,
        status,
        view_stack,
        grid,
        list,
        grid_scroll,
        list_scroll,
        preview,
        previewed: RefCell::new(None),
        sidebar,
        sidebar_targets: RefCell::new(Vec::new()),
        outer,
        inner,
        volumes: gio::VolumeMonitor::get(),
        config: RefCell::new(config),
        search_gen: Cell::new(0),
        layout_ready: Cell::new(false),
        pending_select: RefCell::new(None),
        model_handlers: RefCell::new(Vec::new()),
        save_pending: Cell::new(false),
    });

    OPEN.with(|o| o.borrow_mut().push(app.clone()));
    app.setup_filter();
    app.setup_views();
    app.setup_actions(gapp);
    app.setup_signals();
    app.setup_settings_popover();
    app.rebuild_sidebar();

    app.hidden_btn.set_active(show_hidden);
    app.view_btn.set_active(list_view);
    app.view_stack.set_visible_child_name(if list_view { "list" } else { "grid" });

    app.new_tab(Loc::Dir(start));
    app.restore_layout(preview_width);
    app.window.present();
    app.grid.grab_focus();
    app
}

impl App {
    // ================= model / filtering =================
    fn setup_filter(self: &Rc<Self>) {
        let a = Rc::downgrade(self);
        self.filter.set_filter_func(move |obj| {
            let Some(a) = a.upgrade() else { return false };
            let Some(info) = obj.downcast_ref::<gio::FileInfo>() else { return false };
            if !a.show_hidden.get() && (info.is_hidden() || info.is_backup()) {
                return false;
            }
            let q = a.query.borrow();
            q.is_empty() || info.display_name().to_lowercase().contains(q.as_str())
        });
    }

    pub(crate) fn selected_infos(&self) -> Vec<gio::FileInfo> {
        let bs = self.selection.selection();
        (0..bs.size())
            .filter_map(|i| self.selection.item(bs.nth(i as u32)).and_downcast::<gio::FileInfo>())
            .collect()
    }

    /// The selected item, when exactly one is selected.
    fn selected_info(&self) -> Option<gio::FileInfo> {
        let mut v = self.selected_infos();
        if v.len() == 1 {
            v.pop()
        } else {
            None
        }
    }

    pub(crate) fn current_loc(&self) -> Loc {
        self.tabs.borrow()[self.current.get()].loc.clone()
    }

    fn current_dir(&self) -> Option<gio::File> {
        match self.current_loc() {
            Loc::Dir(f) => Some(f),
            _ => None,
        }
    }

    pub(crate) fn in_trash(&self) -> bool {
        self.current_dir().map(|f| is_trash(&f)).unwrap_or(false)
    }

    // ================= views =================
    fn setup_views(self: &Rc<Self>) {
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
            k.connect_key_pressed(move |_, key, _, mods| {
                let Some(a) = a.upgrade() else { return glib::Propagation::Proceed };
                let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
                let alt = mods.contains(gdk::ModifierType::ALT_MASK);
                let action = match key {
                    gdk::Key::Delete if !ctrl && !alt => "trash",
                    gdk::Key::F2 => "rename",
                    gdk::Key::BackSpace if !ctrl && !alt => "back",
                    gdk::Key::c | gdk::Key::C if ctrl => "copy",
                    gdk::Key::x | gdk::Key::X if ctrl => "cut",
                    gdk::Key::v | gdk::Key::V if ctrl => "paste",
                    gdk::Key::a | gdk::Key::A if ctrl => "select-all",
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
                img.set_pixel_size(20);
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
                    set_icon(&img, &info, false);
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
            col.set_fixed_width(if kind == 3 { 170 } else { 140 });
        }
        self.list.append_column(&col);
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

    /// A compact popup menu built from plain buttons: every option is
    /// visible at once, no scrolling.
    fn popup_menu(&self, parent: &impl IsA<gtk::Widget>, x: f64, y: f64, items: Vec<Option<MItem>>) {
        let pop = gtk::Popover::new();
        pop.add_css_class("ctx-menu");
        pop.set_has_arrow(false);
        pop.set_position(gtk::PositionType::Bottom);
        let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mut last_sep = true;
        for it in items {
            match it {
                None => {
                    if !last_sep {
                        bx.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                        last_sep = true;
                    }
                }
                Some(m) => {
                    let b = gtk::Button::new();
                    b.add_css_class("flat");
                    b.add_css_class("ctx-item");
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 28);
                    let l = gtk::Label::new(Some(&m.label));
                    l.set_xalign(0.0);
                    l.set_hexpand(true);
                    row.append(&l);
                    if !m.accel.is_empty() {
                        let k = gtk::Label::new(Some(m.accel));
                        k.add_css_class("accel");
                        row.append(&k);
                    }
                    b.set_child(Some(&row));
                    b.set_action_name(Some(m.action));
                    if let Some(t) = &m.target {
                        b.set_action_target_value(Some(t));
                    }
                    let p = pop.downgrade();
                    b.connect_clicked(move |_| {
                        if let Some(p) = p.upgrade() {
                            p.popdown()
                        }
                    });
                    bx.append(&b);
                    last_sep = false;
                }
            }
        }
        if let Some(last) = bx.last_child() {
            if last.is::<gtk::Separator>() {
                bx.remove(&last);
            }
        }
        pop.set_child(Some(&bx));
        pop.set_parent(parent);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    }

    fn item_menu(&self, info: &gio::FileInfo) -> Vec<Option<MItem>> {
        let many = self.selected_infos().len() > 1;
        let dir = is_dir_like(info);
        let trash = self.in_trash();
        let cfg = self.config.borrow();
        let uri = item_file(info).uri().to_string();
        let mut m = Vec::new();
        if !many {
            m.push(mi("Open", "win.open", "Enter"));
            if dir {
                m.push(mi("Open in New Tab", "win.open-tab", "Middle-click"));
                if !trash {
                    m.push(mi("Open in Terminal", "win.terminal-selected", ""));
                }
            } else {
                m.push(mi("Open With…", "win.open-with", ""));
            }
            m.push(None);
        }
        if !trash {
            m.push(mi("Cut", "win.cut", "Ctrl+X"));
        }
        m.push(mi("Copy", "win.copy", "Ctrl+C"));
        if dir && !many && !trash {
            m.push(mi("Paste Into Folder", "win.paste-into", ""));
        }
        if !trash {
            m.push(None);
            m.push(mi("Compress…", "win.compress", ""));
            let sel = self.selected_infos();
            if sel.iter().all(|i| !is_dir_like(i) && crate::archive::is_archive(&i.name().to_string_lossy())) {
                m.push(mi("Extract Here", "win.extract-here", ""));
            }
        }
        m.push(None);
        if !many {
            let starred = cfg.starred.contains(&uri);
            m.push(mi(if starred { "Unstar" } else { "Star" }, "win.star", ""));
            if dir {
                let pinned = cfg.pins.contains(&uri);
                m.push(mi(if pinned { "Unpin from Sidebar" } else { "Pin to Sidebar" }, "win.pin-selected", ""));
            }
            if !trash {
                m.push(mi("Change Icon…", "win.change-icon", ""));
                let has_custom = info
                    .attribute_string("metadata::custom-icon")
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if has_custom {
                    m.push(mi("Reset Icon", "win.reset-icon", ""));
                }
            }
            m.push(None);
            if !trash {
                m.push(mi("Rename…", "win.rename", "F2"));
            }
        } else {
            m.push(mi("Star / Unstar", "win.star", ""));
            m.push(None);
            if !trash {
                m.push(mi("Rename…", "win.rename", "F2"));
            }
        }
        m.push(mi("Copy Path", "win.copy-path", ""));
        if trash {
            m.push(mi("Delete Permanently", "win.trash", "Delete"));
        } else {
            m.push(mi("Move to Trash", "win.trash", "Delete"));
        }
        m.push(None);
        m.push(mi("Properties", "win.properties", "Alt+Enter"));
        m
    }

    fn background_menu(&self) -> Vec<Option<MItem>> {
        let mut m = Vec::new();
        if self.in_trash() {
            m.push(mi("Empty Trash", "win.empty-trash", ""));
        } else if self.current_dir().is_some() {
            m.push(mi("New Folder…", "win.new-folder", "Ctrl+Shift+N"));
            m.push(mi("Paste", "win.paste", "Ctrl+V"));
            m.push(mi("Open in Terminal", "win.terminal-here", "Ctrl+Alt+T"));
        }
        m.push(None);
        m.push(mi("Select All", "win.select-all", "Ctrl+A"));
        if self.current_dir().is_some() {
            let pinned = self.current_dir().map(|f| self.is_pinned(&f)).unwrap_or(false);
            m.push(mi(if pinned { "Unpin This Folder" } else { "Pin This Folder" }, "win.pin-current", "Ctrl+D"));
        }
        m.push(mi(
            if self.show_hidden.get() { "Hide Hidden Files" } else { "Show Hidden Files" },
            "win.toggle-hidden",
            "Ctrl+H",
        ));
        m.push(mi("Reload", "win.refresh", "F5"));
        if self.current_dir().is_some() {
            m.push(None);
            m.push(mi("Properties", "win.properties", "Alt+Enter"));
        }
        m
    }

    fn activate(&self, name: &str) {
        gtk::prelude::ActionGroupExt::activate_action(&self.window, name, None);
    }

    // ================= opening =================
    fn open_position(self: &Rc<Self>, pos: u32) {
        if let Some(info) = self.selection.item(pos).and_downcast::<gio::FileInfo>() {
            self.open_info(&info);
        }
    }

    pub fn open_info(self: &Rc<Self>, info: &gio::FileInfo) {
        let file = item_file(info);
        if is_dir_like(info) {
            self.navigate(Loc::Dir(file));
        } else {
            self.launch(&file, false);
        }
    }

    fn launch(&self, file: &gio::File, ask: bool) {
        let l = gtk::FileLauncher::new(Some(file));
        l.set_always_ask(ask);
        let status = self.status.clone();
        l.launch(Some(&self.window), None::<&gio::Cancellable>, move |r| {
            if let Err(e) = r {
                if !e.matches(gtk::DialogError::Dismissed) {
                    status.set_text(&format!("Couldn't open: {}", e.message()));
                }
            }
        });
    }

    fn open_terminal(&self, dir: Option<gio::File>) {
        let Some(path) = dir.and_then(|d| d.path()) else {
            self.status.set_text("Terminal can only open in local folders");
            return;
        };
        if let Err(e) = fileops::open_terminal(&path) {
            self.status.set_text(&e);
        }
    }

    // ================= navigation =================
    pub fn navigate(self: &Rc<Self>, loc: Loc) {
        {
            let mut tabs = self.tabs.borrow_mut();
            let t = &mut tabs[self.current.get()];
            if t.loc.key() == loc.key() && !matches!(loc, Loc::Search { .. }) {
                drop(tabs);
                self.load_current();
                return;
            }
            let old = std::mem::replace(&mut t.loc, loc);
            t.back.push(old);
            t.fwd.clear();
        }
        self.load_current();
    }

    fn go_back(self: &Rc<Self>) {
        {
            let mut tabs = self.tabs.borrow_mut();
            let t = &mut tabs[self.current.get()];
            let Some(prev) = t.back.pop() else { return };
            let cur = std::mem::replace(&mut t.loc, prev);
            t.fwd.push(cur);
        }
        self.load_current();
    }

    fn go_forward(self: &Rc<Self>) {
        {
            let mut tabs = self.tabs.borrow_mut();
            let t = &mut tabs[self.current.get()];
            let Some(next) = t.fwd.pop() else { return };
            let cur = std::mem::replace(&mut t.loc, next);
            t.back.push(cur);
        }
        self.load_current();
    }

    fn go_up(self: &Rc<Self>) {
        match self.current_loc() {
            Loc::Dir(f) => {
                if let Some(p) = f.parent() {
                    self.navigate(Loc::Dir(p));
                }
            }
            Loc::Search { root, .. } => self.navigate(Loc::Dir(root)),
            Loc::Starred => {}
        }
    }

    fn load_current(self: &Rc<Self>) {
        let (loc, can_back, can_fwd) = {
            let tabs = self.tabs.borrow();
            let t = &tabs[self.current.get()];
            t.label.set_text(&t.loc.title());
            t.icon.set_icon_name(Some(t.loc.icon()));
            t.widget.set_tooltip_text(Some(&match &t.loc {
                Loc::Dir(f) => display_path(f),
                other => other.title(),
            }));
            (t.loc.clone(), !t.back.is_empty(), !t.fwd.is_empty())
        };
        self.search_gen.set(self.search_gen.get() + 1);
        self.back_btn.set_sensitive(can_back);
        self.fwd_btn.set_sensitive(can_fwd);
        *self.previewed.borrow_mut() = None;
        self.preview.clear();

        let new_query = match &loc {
            Loc::Dir(f) => {
                self.filter_model.set_model(Some(&self.dir_list));
                self.dir_list.set_file(None::<&gio::File>);
                self.dir_list.set_file(Some(f));
                self.path_entry.set_text(&display_path(f));
                self.up_btn.set_sensitive(f.parent().is_some());
                String::new()
            }
            Loc::Starred => {
                let files: Vec<_> = self.config.borrow().starred.iter().map(|u| gio::File::for_uri(u)).collect();
                let store = infos_store(files.into_iter());
                self.filter_model.set_model(Some(&store));
                self.path_entry.set_text("Starred");
                self.up_btn.set_sensitive(false);
                String::new()
            }
            Loc::Search { root, query, results } => {
                let store = infos_store(results.iter().map(gio::File::for_path));
                self.filter_model.set_model(Some(&store));
                self.path_entry
                    .set_text(&format!("Search “{query}” in {}", display_path(root)));
                self.up_btn.set_sensitive(true);
                query.clone()
            }
        };
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
    }

    /// Selects and scrolls to `name` in the current folder as soon as it
    /// has finished loading (used when RFiles is asked to show a file).
    pub fn select_when_loaded(self: &Rc<Self>, name: String) {
        *self.pending_select.borrow_mut() = Some(name);
        if !self.dir_list.is_loading() {
            self.apply_pending_select();
        }
    }

    fn apply_pending_select(&self) {
        let Some(name) = self.pending_select.borrow_mut().take() else { return };
        for pos in 0..self.selection.n_items() {
            let Some(info) = self.selection.item(pos).and_downcast::<gio::FileInfo>() else { continue };
            if info.name().to_string_lossy() == name {
                self.grid.scroll_to(pos, gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT, None);
                self.list.scroll_to(pos, None::<&gtk::ColumnViewColumn>, gtk::ListScrollFlags::NONE, None);
                break;
            }
        }
    }

    /// Stops a closing window's work right away: handlers on shared
    /// objects (the volume monitor) go, folder monitoring and any playing
    /// preview stop. The rest is freed when the window is destroyed.
    fn teardown(&self) {
        for (obj, id) in self.model_handlers.borrow_mut().drain(..) {
            obj.disconnect(id);
        }
        self.dir_list.set_file(None::<&gio::File>);
        self.preview.clear();
    }

    pub(crate) fn reload(self: &Rc<Self>) {
        self.load_current();
    }

    fn on_selection_changed(self: &Rc<Self>) {
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
                    self.preview.show(&infos[0]);
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

    fn update_status(&self) {
        let n = self.selection.n_items();
        let mut s = format!("{n} item{}", if n == 1 { "" } else { "s" });
        if matches!(self.current_loc(), Loc::Dir(_)) && self.dir_list.is_loading() {
            s.push_str("  ·  loading…");
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
    }

    // ================= tabs =================
    pub fn new_tab(self: &Rc<Self>, loc: Loc) {
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bx.add_css_class("tab");
        let icon = gtk::Image::from_icon_name("folder-symbolic");
        let label = gtk::Label::builder()
            .ellipsize(pango::EllipsizeMode::End)
            .max_width_chars(18)
            .width_chars(6)
            .xalign(0.0)
            .build();
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("tab-close");
        close.set_focus_on_click(false);
        close.set_tooltip_text(Some("Close tab (Ctrl+W)"));
        bx.append(&icon);
        bx.append(&label);
        bx.append(&close);

        let g = gtk::GestureClick::new();
        g.set_button(0);
        let a = Rc::downgrade(self);
        let w = bx.downgrade();
        g.connect_pressed(move |g, _, _, _| {
            let Some(a) = a.upgrade() else { return };
            let Some(w) = w.upgrade() else { return };
            let Some(i) = a.tab_index(&w) else { return };
            match g.current_button() {
                1 => a.switch_to(i),
                2 => a.close_tab(i),
                _ => {}
            }
        });
        bx.add_controller(g);
        let a = Rc::downgrade(self);
        let w = bx.downgrade();
        close.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            let Some(w) = w.upgrade() else { return };
            if let Some(i) = a.tab_index(&w) {
                a.close_tab(i);
            }
        });

        // New tabs go just before the + button
        bx.insert_before(&self.tab_strip, Some(&self.plus));
        let idx = {
            let mut tabs = self.tabs.borrow_mut();
            tabs.push(Tab { loc, back: vec![], fwd: vec![], widget: bx, label, icon, close });
            tabs.len() - 1
        };
        self.switch_to(idx);
        let adj = self.tab_scroll.hadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper()));
    }

    fn tab_index(&self, w: &gtk::Box) -> Option<usize> {
        self.tabs.borrow().iter().position(|t| &t.widget == w)
    }

    fn switch_to(self: &Rc<Self>, i: usize) {
        {
            let tabs = self.tabs.borrow();
            if i >= tabs.len() {
                return;
            }
            let many = tabs.len() > 1;
            for (j, t) in tabs.iter().enumerate() {
                if j == i {
                    t.widget.add_css_class("active");
                } else {
                    t.widget.remove_css_class("active");
                }
                t.close.set_visible(many);
            }
        }
        self.current.set(i);
        self.load_current();
    }

    fn close_tab(self: &Rc<Self>, i: usize) {
        let new_cur = {
            let mut tabs = self.tabs.borrow_mut();
            let n = tabs.len();
            if n <= 1 || i >= n {
                return;
            }
            let t = tabs.remove(i);
            self.tab_strip.remove(&t.widget);
            let cur = self.current.get();
            if i < cur {
                cur - 1
            } else if i == cur {
                i.min(n - 2)
            } else {
                cur
            }
        };
        self.switch_to(new_cur);
    }

    fn cycle_tab(self: &Rc<Self>, delta: i32) {
        let n = self.tabs.borrow().len() as i32;
        let i = (self.current.get() as i32 + delta).rem_euclid(n);
        self.switch_to(i as usize);
    }

    // ================= sidebar =================
    /// Built-in places: (key, icon, name, location)
    fn places(&self) -> Vec<(&'static str, &'static str, &'static str, Loc)> {
        let mut v = vec![
            ("home", "user-home-symbolic", "Home", Loc::Dir(home())),
            ("recent", "document-open-recent-symbolic", "Recent", Loc::Dir(gio::File::for_uri("recent:///"))),
            ("starred", "starred-symbolic", "Starred", Loc::Starred),
        ];
        let specials = [
            ("desktop", glib::UserDirectory::Desktop, "user-desktop-symbolic", "Desktop"),
            ("documents", glib::UserDirectory::Documents, "folder-documents-symbolic", "Documents"),
            ("downloads", glib::UserDirectory::Downloads, "folder-download-symbolic", "Downloads"),
            ("music", glib::UserDirectory::Music, "folder-music-symbolic", "Music"),
            ("pictures", glib::UserDirectory::Pictures, "folder-pictures-symbolic", "Pictures"),
            ("videos", glib::UserDirectory::Videos, "folder-videos-symbolic", "Videos"),
        ];
        for (key, d, icon, name) in specials {
            if let Some(p) = glib::user_special_dir(d) {
                if p != glib::home_dir() && p.is_dir() {
                    v.push((key, icon, name, Loc::Dir(gio::File::for_path(p))));
                }
            }
        }
        v.push(("trash", "user-trash-symbolic", "Trash", Loc::Dir(gio::File::for_uri("trash:///"))));

        // Apply the user's drag-and-drop order
        let order = self.config.borrow().places_order.clone();
        let rank = |k: &str| order.iter().position(|o| o == k).unwrap_or(usize::MAX);
        let mut indexed: Vec<_> = v.into_iter().enumerate().collect();
        indexed.sort_by_key(|(i, p)| (rank(p.0), *i));
        indexed.into_iter().map(|(_, p)| p).collect()
    }

    fn rebuild_sidebar(self: &Rc<Self>) {
        while let Some(c) = self.sidebar.first_child() {
            self.sidebar.remove(&c);
        }
        self.sidebar_targets.borrow_mut().clear();

        for (key, icon, name, loc) in self.places() {
            self.side_row(icon, name, loc, None, Some(format!("place:{key}")));
        }

        self.side_header("Pinned");
        crate::dnd::attach_pin_target(self, &self.sidebar);
        let pins = self.config.borrow().pins.clone();
        if pins.is_empty() {
            self.side_hint("Right-click a folder → Pin to Sidebar");
            crate::dnd::attach_pin_target(self, &self.sidebar);
        }
        for uri in pins {
            let f = gio::File::for_uri(&uri);
            let key = format!("pin:{uri}");
            self.side_row("folder-symbolic", &file_title(&f), Loc::Dir(f), Some(uri), Some(key));
        }

        self.side_header("Devices");
        self.side_row("drive-harddisk-symbolic", "Computer", Loc::Dir(gio::File::for_path("/")), None, None);
        for m in self.volumes.mounts() {
            if m.is_shadowed() {
                continue;
            }
            self.side_row_gicon(&m.symbolic_icon(), &m.name(), Loc::Dir(m.root()), None, None);
        }
        self.side_row("network-workgroup-symbolic", "Network", Loc::Dir(gio::File::for_uri("network:///")), None, None);

        let loc = self.tabs.borrow().get(self.current.get()).map(|t| t.loc.clone());
        if let Some(loc) = loc {
            self.highlight_sidebar(&loc);
        }
    }

    fn side_row(self: &Rc<Self>, icon: &str, name: &str, loc: Loc, pin: Option<String>, drag_key: Option<String>) {
        let gicon: gio::Icon = gio::ThemedIcon::new(icon).upcast();
        self.side_row_gicon(&gicon, name, loc, pin, drag_key);
    }

    fn side_row_gicon(
        self: &Rc<Self>,
        icon: &gio::Icon,
        name: &str,
        loc: Loc,
        pin: Option<String>,
        drag_key: Option<String>,
    ) {
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let img = gtk::Image::from_gicon(icon);
        let lbl = gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        bx.append(&img);
        bx.append(&lbl);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&bx));
        if let Loc::Dir(f) = &loc {
            row.set_tooltip_text(Some(&display_path(f)));
            crate::dnd::attach_folder_row(self, &row, f);
        }
        let idx = {
            let mut t = self.sidebar_targets.borrow_mut();
            t.push(Some(SideTarget { loc }));
            t.len() - 1
        };

        // Right-click / middle-click
        let g = gtk::GestureClick::new();
        g.set_button(0);
        let a = Rc::downgrade(self);
        let r = row.downgrade();
        let menu_key = drag_key.clone();
        g.connect_pressed(move |g, _, x, y| {
            let Some(a) = a.upgrade() else { return };
            let Some(r) = r.upgrade() else { return };
            match g.current_button() {
            3 => {
                g.set_state(gtk::EventSequenceState::Claimed);
                let mut items = vec![Some(MItem {
                    label: "Open in New Tab".into(),
                    action: "win.sidebar-tab",
                    target: Some((idx as i32).to_variant()),
                    accel: "",
                })];
                if let Some(k) = &menu_key {
                    items.push(None);
                    items.push(Some(MItem { label: "Move Up".into(), action: "win.side-up", target: Some(k.to_variant()), accel: "" }));
                    items.push(Some(MItem { label: "Move Down".into(), action: "win.side-down", target: Some(k.to_variant()), accel: "" }));
                    items.push(None);
                }
                if let Some(uri) = &pin {
                    items.push(Some(MItem {
                        label: "Unpin".into(),
                        action: "win.unpin",
                        target: Some(uri.to_variant()),
                        accel: "",
                    }));
                }
                a.popup_menu(&r, x, y, items);
            }
            2 => {
                g.set_state(gtk::EventSequenceState::Claimed);
                a.open_sidebar_in_tab(idx);
            }
            _ => {}
            }
        });
        row.add_controller(g);

        // Drag to reorder
        if let Some(key) = drag_key {
            let src = gtk::DragSource::new();
            src.set_actions(gdk::DragAction::MOVE);
            let k = key.clone();
            src.connect_prepare(move |_, _, _| Some(gdk::ContentProvider::for_value(&k.to_value())));
            let r = row.downgrade();
            src.connect_drag_begin(move |s, _| {
                if let Some(r) = r.upgrade() {
                    s.set_icon(Some(&gtk::WidgetPaintable::new(Some(&r))), 20, 16);
                }
            });
            row.add_controller(src);

            let drop = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
            crate::dnd::ignore_file_drags(&drop);
            let r = row.downgrade();
            drop.connect_motion(move |_, _, y| {
                let Some(r) = r.upgrade() else { return gdk::DragAction::empty() };
                let above = y < r.height() as f64 / 2.0;
                r.remove_css_class(if above { "drop-below" } else { "drop-above" });
                r.add_css_class(if above { "drop-above" } else { "drop-below" });
                gdk::DragAction::MOVE
            });
            let r = row.downgrade();
            drop.connect_leave(move |_| {
                if let Some(r) = r.upgrade() {
                    r.remove_css_class("drop-above");
                    r.remove_css_class("drop-below");
                }
            });
            let a = Rc::downgrade(self);
            let r = row.downgrade();
            drop.connect_drop(move |_, value, _, y| {
                let Some(a) = a.upgrade() else { return false };
                let Some(r) = r.upgrade() else { return false };
                r.remove_css_class("drop-above");
                r.remove_css_class("drop-below");
                let Ok(src) = value.get::<String>() else { return false };
                let before = y < r.height() as f64 / 2.0;
                let a = a.clone();
                let dst = key.clone();
                // Rebuild after the drop finishes, not while GTK is still using these rows
                glib::idle_add_local_once(move || a.reorder_sidebar(&src, &dst, before));
                true
            });
            row.add_controller(drop);
        }

        self.sidebar.append(&row);
    }

    fn reorder_sidebar(self: &Rc<Self>, src: &str, dst: &str, before: bool) {
        if src == dst {
            return;
        }
        fn move_in(list: &mut Vec<String>, src: &str, dst: &str, before: bool) -> bool {
            let Some(si) = list.iter().position(|k| k == src) else { return false };
            let item = list.remove(si);
            let Some(di) = list.iter().position(|k| k == dst) else {
                list.insert(si, item);
                return false;
            };
            list.insert(if before { di } else { di + 1 }, item);
            true
        }
        match (src.split_once(':'), dst.split_once(':')) {
            (Some(("place", s)), Some(("place", d))) => {
                let mut order: Vec<String> = self.places().iter().map(|p| p.0.to_string()).collect();
                if move_in(&mut order, s, d, before) {
                    let mut c = self.config.borrow_mut();
                    c.places_order = order;
                    c.save();
                }
            }
            (Some(("pin", s)), Some(("pin", d))) => {
                let mut c = self.config.borrow_mut();
                if move_in(&mut c.pins, s, d, before) {
                    c.save();
                }
            }
            _ => {
                self.status.set_text("Places and pinned folders can be reordered within their own section");
                return;
            }
        }
        self.rebuild_sidebar();
    }

    /// "Move Up" / "Move Down" from the sidebar's right-click menu.
    fn move_sidebar_item(self: &Rc<Self>, key: &str, up: bool) {
        let keys: Vec<String> = match key.split_once(':') {
            Some(("place", _)) => self.places().iter().map(|p| format!("place:{}", p.0)).collect(),
            Some(("pin", _)) => self.config.borrow().pins.iter().map(|u| format!("pin:{u}")).collect(),
            _ => return,
        };
        let Some(i) = keys.iter().position(|k| k == key) else { return };
        let j = if up { i.checked_sub(1) } else { Some(i + 1).filter(|&j| j < keys.len()) };
        if let Some(j) = j {
            self.reorder_sidebar(key, &keys[j], up);
        }
    }

    fn side_header(&self, text: &str) {
        let lbl = gtk::Label::new(Some(text));
        lbl.set_xalign(0.0);
        lbl.add_css_class("sidebar-header");
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&lbl));
        row.set_activatable(false);
        row.set_selectable(false);
        self.sidebar.append(&row);
        self.sidebar_targets.borrow_mut().push(None);
    }

    fn side_hint(&self, text: &str) {
        let lbl = gtk::Label::new(Some(text));
        lbl.set_xalign(0.0);
        lbl.set_wrap(true);
        lbl.add_css_class("sidebar-hint");
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&lbl));
        row.set_activatable(false);
        row.set_selectable(false);
        self.sidebar.append(&row);
        self.sidebar_targets.borrow_mut().push(None);
    }

    fn sidebar_loc(&self, idx: usize) -> Option<Loc> {
        self.sidebar_targets
            .borrow()
            .get(idx)
            .and_then(|t| t.as_ref().map(|t| t.loc.clone()))
    }

    fn open_sidebar_in_tab(self: &Rc<Self>, idx: usize) {
        if let Some(loc) = self.sidebar_loc(idx) {
            self.new_tab(loc);
        }
    }

    fn highlight_sidebar(&self, loc: &Loc) {
        let key = loc.key();
        let idx = self
            .sidebar_targets
            .borrow()
            .iter()
            .position(|t| t.as_ref().map(|t| t.loc.key() == key).unwrap_or(false));
        match idx.and_then(|i| self.sidebar.row_at_index(i as i32)) {
            Some(row) => self.sidebar.select_row(Some(&row)),
            None => self.sidebar.unselect_all(),
        }
    }

    pub(crate) fn set_pinned(self: &Rc<Self>, uri: String, pinned: bool) {
        {
            let mut c = self.config.borrow_mut();
            c.pins.retain(|u| u != &uri);
            if pinned {
                c.pins.push(uri);
            }
            c.save();
        }
        self.rebuild_sidebar();
        self.update_pin_button();
    }

    // ================= settings (icons) =================
    fn setup_settings_popover(self: &Rc<Self>) {
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
        style.set_selected(if cur == "system" {
            1
        } else if cur.starts_with("custom:") {
            2
        } else {
            0
        });

        let h2 = gtk::Label::new(Some("Icon size"));
        h2.set_xalign(0.0);
        h2.add_css_class("heading");
        let names: Vec<&str> = ICON_SIZES.iter().map(|s| s.1).collect();
        let size = gtk::DropDown::from_strings(&names);
        let cur_size = self.config.borrow().icon_size;
        size.set_selected(ICON_SIZES.iter().position(|s| s.0 == cur_size).unwrap_or(0) as u32);

        let hint = gtk::Label::new(Some("Tip: right-click any file or folder\n→ Change Icon… to give just that one\nits own icon."));
        hint.set_xalign(0.0);
        hint.add_css_class("dim");
        hint.add_css_class("small");

        bx.append(&h1);
        bx.append(&style);
        bx.append(&h2);
        bx.append(&size);
        bx.append(&hint);
        let pop = gtk::Popover::new();
        pop.set_child(Some(&bx));
        self.settings_btn.set_popover(Some(&pop));

        let a = Rc::downgrade(self);
        let busy = Rc::new(Cell::new(false));
        style.connect_selected_notify(move |dd| {
            let Some(a) = a.upgrade() else { return };
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
                            dd2.set_selected(if prev == "system" { 1 } else if prev.starts_with("custom:") { 2 } else { 0 });
                            busy2.set(false);
                        }
                    });
                }
            }
        });
        let a = Rc::downgrade(self);
        size.connect_selected_notify(move |dd| {
            let Some(a) = a.upgrade() else { return };
            let px = ICON_SIZES.get(dd.selected() as usize).map(|s| s.0).unwrap_or(40);
            ICON_SIZE.with(|s| s.set(px));
            {
                let mut c = a.config.borrow_mut();
                c.icon_size = px;
                c.save();
            }
            a.reload();
        });
    }

    fn set_folder_icon(self: &Rc<Self>, setting: String) {
        apply_folder_icon_setting(&setting);
        {
            let mut c = self.config.borrow_mut();
            c.folder_icon = setting;
            c.save();
        }
        self.reload();
    }

    fn pick_image(&self, title: &str, f: impl FnOnce(Option<gio::File>) + 'static) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_mime_type("image/*");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dlg = gtk::FileDialog::builder().title(title).modal(true).filters(&filters).build();
        dlg.open(Some(&self.window), None::<&gio::Cancellable>, move |r| f(r.ok()));
    }

    fn set_custom_icon(self: &Rc<Self>, target: &gio::File, icon_uri: &str) {
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

    // ================= search =================
    fn run_search(self: &Rc<Self>, root: gio::File, query: String) {
        let Some(path) = root.path() else {
            self.status.set_text("Deep search only works on local folders");
            return;
        };
        let g = self.search_gen.get() + 1;
        self.search_gen.set(g);
        self.status.set_text(&format!("Searching for “{query}” in {}…", path.display()));
        let hidden = self.show_hidden.get();
        let a = self.clone();
        let q = query.to_lowercase();
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || search_walk(&path, &q, hidden)).await;
            if a.search_gen.get() != g {
                return;
            }
            if let Ok(results) = res {
                a.navigate(Loc::Search { root, query, results: Rc::new(results) });
            }
        });
    }

    // ================= copy / cut / paste =================
    fn copy_selected(&self, cut: bool) {
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

    fn paste_into(self: &Rc<Self>, dest: gio::File) {
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
            let srcs: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
            if srcs.is_empty() {
                a.status.set_text("Can only paste local files");
                return;
            }
            let n = srcs.len();
            let verb = if cut { "Moving" } else { "Copying" };
            a.status.set_text(&format!("{verb} {n} item(s)…"));
            let progress = Arc::new(AtomicU64::new(0));
            let (p2, a2) = (progress.clone(), a.clone());
            let timer = glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
                let done = p2.load(AtomicOrdering::Relaxed);
                a2.status.set_text(&format!("{verb} {n} item(s)…  {}", glib::format_size(done)));
                glib::ControlFlow::Continue
            });
            let out = gio::spawn_blocking(move || fileops::transfer(srcs, dest_path, cut, progress)).await;
            timer.remove();
            match out {
                Ok(o) => {
                    if cut && o.errors.is_empty() {
                        let _ = cb.set_content(None::<&gdk::ContentProvider>);
                    }
                    let verb = if cut { "Moved" } else { "Pasted" };
                    if o.errors.is_empty() {
                        a.status.set_text(&format!("{verb} {} item(s)", o.done));
                    } else {
                        a.status.set_text(&format!(
                            "{verb} {}, {} failed — {}",
                            o.done,
                            o.errors.len(),
                            o.errors[0]
                        ));
                    }
                }
                Err(_) => a.status.set_text("Paste failed"),
            }
            if !matches!(a.current_loc(), Loc::Dir(_)) {
                a.reload();
            }
        });
    }

    // ================= other file operations =================
    fn prompt(self: &Rc<Self>, title: &str, initial: &str, ok: &str, f: impl Fn(String) -> Result<(), String> + 'static) {
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
        let stem = Path::new(initial)
            .file_stem()
            .map(|s| s.to_string_lossy().chars().count())
            .unwrap_or(0);
        entry.grab_focus();
        entry.select_region(0, stem as i32);
    }

    fn delete_selected(self: &Rc<Self>) {
        let infos = self.selected_infos();
        if infos.is_empty() {
            return;
        }
        let files: Vec<gio::File> = infos
            .iter()
            .map(|i| {
                i.attribute_object("standard::file")
                    .and_downcast::<gio::File>()
                    .unwrap_or_else(|| item_file(i))
            })
            .collect();
        if self.in_trash() {
            let what = if infos.len() == 1 {
                format!("“{}”", infos[0].display_name())
            } else {
                format!("{} items", infos.len())
            };
            let dlg = gtk::AlertDialog::builder()
                .message(format!("Permanently delete {what}?"))
                .detail("This can't be undone.")
                .buttons(["Cancel", "Delete"])
                .cancel_button(0)
                .default_button(0)
                .build();
            let a = self.clone();
            dlg.choose(Some(&self.window), None::<&gio::Cancellable>, move |r| {
                if r == Ok(1) {
                    for f in &files {
                        if let Err(e) = delete_recursive(f) {
                            a.status.set_text(&format!("Couldn't delete: {}", e.message()));
                        }
                    }
                }
            });
        } else {
            let mut failed = None;
            for f in &files {
                if let Err(e) = f.trash(None::<&gio::Cancellable>) {
                    failed = Some(e.message().to_string());
                }
            }
            match failed {
                Some(e) => self.status.set_text(&format!("Couldn't move to trash: {e}")),
                None => self.status.set_text(&format!("Moved {} item(s) to the trash", files.len())),
            }
            if !matches!(self.current_loc(), Loc::Dir(_)) {
                self.reload();
            }
        }
    }

    fn empty_trash(self: &Rc<Self>) {
        let dlg = gtk::AlertDialog::builder()
            .message("Empty the trash?")
            .detail("Everything in the trash will be permanently deleted.")
            .buttons(["Cancel", "Empty Trash"])
            .cancel_button(0)
            .default_button(0)
            .build();
        let a = self.clone();
        dlg.choose(Some(&self.window), None::<&gio::Cancellable>, move |r| {
            if r != Ok(1) {
                return;
            }
            let trash = gio::File::for_uri("trash:///");
            if let Ok(en) = trash.enumerate_children("standard::name", gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, None::<&gio::Cancellable>) {
                while let Ok(Some(info)) = en.next_file(None::<&gio::Cancellable>) {
                    let _ = trash.child(info.name()).delete(None::<&gio::Cancellable>);
                }
            }
            a.reload();
        });
    }

    fn toggle_star(self: &Rc<Self>) {
        let infos = self.selected_infos();
        if infos.is_empty() {
            return;
        }
        {
            let mut c = self.config.borrow_mut();
            for info in &infos {
                let uri = item_file(info).uri().to_string();
                if c.starred.contains(&uri) {
                    c.starred.retain(|u| u != &uri);
                } else {
                    c.starred.push(uri);
                }
            }
            c.save();
        }
        if matches!(self.current_loc(), Loc::Starred) {
            self.reload();
        }
    }

    /// The preview pane's width can only be applied once the window has a
    /// real size, so wait for the first frame, then place the divider.
    fn restore_layout(self: &Rc<Self>, preview_width: i32) {
        let a = self.clone();
        self.inner.add_tick_callback(move |p, _| {
            let w = p.width();
            if w <= 0 {
                return glib::ControlFlow::Continue;
            }
            p.set_position((w - preview_width).max(250));
            // Start saving changes only after the restore is applied
            let a = a.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                a.layout_ready.set(true)
            });
            glib::ControlFlow::Break
        });

        // Save shortly after any layout change, not only when closing, so
        // sizes survive however the app is closed.
        let watch = |obj: &glib::Object, prop: &str| {
            let a = Rc::downgrade(self);
            obj.connect_notify_local(Some(prop), move |_, _| {
                if let Some(a) = a.upgrade() {
                    a.schedule_save()
                }
            });
        };
        watch(self.outer.upcast_ref(), "position");
        watch(self.inner.upcast_ref(), "position");
        watch(self.window.upcast_ref(), "default-width");
        watch(self.window.upcast_ref(), "default-height");
        watch(self.window.upcast_ref(), "maximized");
    }

    fn schedule_save(self: &Rc<Self>) {
        if !self.layout_ready.get() || self.save_pending.replace(true) {
            return;
        }
        let a = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            a.save_pending.set(false);
            a.save_layout();
        });
    }

    fn save_layout(&self) {
        let mut c = self.config.borrow_mut();
        let maximized = self.window.is_maximized() || self.window.is_fullscreen();
        c.maximized = maximized;
        if !maximized {
            let (w, h) = self.window.default_size();
            if w > 0 && h > 0 {
                c.width = w;
                c.height = h;
            }
        }
        if self.layout_ready.get() {
            c.sidebar_pos = self.outer.position();
            let iw = self.inner.width();
            if iw > 0 {
                c.preview_width = (iw - self.inner.position()).max(150);
                c.files_pos = self.inner.position();
            }
        }
        c.list_view = self.view_btn.is_active();
        c.show_hidden = self.show_hidden.get();
        c.save();
    }

    fn is_pinned(&self, f: &gio::File) -> bool {
        let uri = f.uri().to_string();
        self.config.borrow().pins.contains(&uri)
    }

    /// The pin button lights up when the current folder is pinned.
    fn update_pin_button(&self) {
        let pinned = self.current_dir().map(|f| self.is_pinned(&f)).unwrap_or(false);
        if pinned {
            self.pin_btn.add_css_class("pinned");
            self.pin_btn.set_tooltip_text(Some("Unpin this folder from the sidebar (Ctrl+D)"));
        } else {
            self.pin_btn.remove_css_class("pinned");
            self.pin_btn.set_tooltip_text(Some("Pin this folder to the sidebar (Ctrl+D)"));
        }
        self.pin_btn.set_sensitive(self.current_dir().is_some());
    }

    // ================= actions & signals =================
    fn setup_actions(self: &Rc<Self>, gapp: &gtk::Application) {
        let add = |name: &str, f: Box<dyn Fn(&Rc<App>)>| {
            let act = gio::SimpleAction::new(name, None);
            let a = Rc::downgrade(self);
            act.connect_activate(move |_, _| {
                if let Some(a) = a.upgrade() {
                    f(&a)
                }
            });
            self.window.add_action(&act);
        };
        add("new-tab", Box::new(|a| a.new_tab(Loc::Dir(home()))));
        add("close-tab", Box::new(|a| a.close_tab(a.current.get())));
        add("next-tab", Box::new(|a| a.cycle_tab(1)));
        add("prev-tab", Box::new(|a| a.cycle_tab(-1)));
        add("back", Box::new(|a| a.go_back()));
        add("forward", Box::new(|a| a.go_forward()));
        add("up", Box::new(|a| a.go_up()));
        add("refresh", Box::new(|a| a.reload()));
        add("focus-search", Box::new(|a| {
            a.search.grab_focus();
        }));
        add("focus-path", Box::new(|a| {
            a.path_entry.grab_focus();
            a.path_entry.select_region(0, -1);
        }));
        add("toggle-hidden", Box::new(|a| a.hidden_btn.set_active(!a.hidden_btn.is_active())));
        add("grid-view", Box::new(|a| a.view_btn.set_active(false)));
        add("list-view", Box::new(|a| a.view_btn.set_active(true)));
        add("select-all", Box::new(|a| {
            a.selection.select_all();
        }));
        add("select-none", Box::new(|a| {
            a.selection.unselect_all();
        }));
        add("copy", Box::new(|a| a.copy_selected(false)));
        add("cut", Box::new(|a| {
            if !a.in_trash() {
                a.copy_selected(true)
            }
        }));
        add("paste", Box::new(|a| {
            if let Some(d) = a.current_dir().filter(|d| !is_trash(d)) {
                a.paste_into(d);
            }
        }));
        add("paste-into", Box::new(|a| {
            if let Some(i) = a.selected_info().filter(is_dir_like) {
                a.paste_into(item_file(&i));
            }
        }));
        add("terminal-here", Box::new(|a| a.open_terminal(a.current_dir())));
        add("terminal-selected", Box::new(|a| {
            let dir = a.selected_info().filter(is_dir_like).map(|i| item_file(&i));
            a.open_terminal(dir.or_else(|| a.current_dir()));
        }));
        add("pin-current", Box::new(|a| {
            if let Some(f) = a.current_dir() {
                let pinned = a.is_pinned(&f);
                a.set_pinned(f.uri().to_string(), !pinned);
                a.status.set_text(&if pinned {
                    format!("Unpinned “{}”", file_title(&f))
                } else {
                    format!("Pinned “{}” to the sidebar", file_title(&f))
                });
            }
        }));
        add("open", Box::new(|a| {
            if let Some(i) = a.selected_info() {
                a.open_info(&i);
            }
        }));
        add("open-tab", Box::new(|a| {
            if let Some(i) = a.selected_info() {
                a.new_tab(Loc::Dir(item_file(&i)));
            }
        }));
        add("open-with", Box::new(|a| {
            if let Some(i) = a.selected_info() {
                a.launch(&item_file(&i), true);
            }
        }));
        add("star", Box::new(|a| a.toggle_star()));
        add("pin-selected", Box::new(|a| {
            if let Some(i) = a.selected_info() {
                let uri = item_file(&i).uri().to_string();
                let pinned = a.config.borrow().pins.contains(&uri);
                a.set_pinned(uri, !pinned);
            }
        }));
        add("change-icon", Box::new(|a| {
            let Some(info) = a.selected_info() else { return };
            let target = item_file(&info);
            let a2 = a.clone();
            a.pick_image(&format!("Choose an icon for “{}”", info.display_name()), move |r| {
                if let Some(img) = r {
                    a2.set_custom_icon(&target, &img.uri());
                }
            });
        }));
        add("reset-icon", Box::new(|a| {
            if let Some(info) = a.selected_info() {
                a.set_custom_icon(&item_file(&info), "");
            }
        }));
        add("copy-path", Box::new(|a| {
            let paths: Vec<String> = a.selected_infos().iter().map(|i| item_file(i).parse_name().to_string()).collect();
            if !paths.is_empty() {
                a.window.clipboard().set_text(&paths.join("\n"));
            }
        }));
        add("compress", Box::new(|a| {
            let infos = a.selected_infos();
            let paths: Vec<PathBuf> = infos.iter().filter_map(|i| item_file(i).path()).collect();
            if a.in_trash() || paths.is_empty() {
                return;
            }
            if paths.len() != infos.len() {
                a.status.set_text("Can only compress local files");
                return;
            }
            // Starred / search results have no folder of their own: use the first item's
            let dir = a.current_dir().and_then(|d| d.path()).or_else(|| paths[0].parent().map(Path::to_path_buf));
            if let Some(dir) = dir {
                crate::archive::compress_dialog(&a.window, &a.status, dir, paths);
            }
        }));
        add("extract-here", Box::new(|a| {
            let infos = a.selected_infos();
            let paths: Vec<PathBuf> = infos.iter().filter_map(|i| item_file(i).path()).collect();
            if a.in_trash() || paths.is_empty() {
                return;
            }
            if paths.len() != infos.len() {
                a.status.set_text("Can only extract local files");
                return;
            }
            crate::archive::extract_here(&a.status, paths);
        }));
        add("trash", Box::new(|a| a.delete_selected()));
        add("empty-trash", Box::new(|a| a.empty_trash()));
        add("rename", Box::new(|a| {
            if a.in_trash() {
                return;
            }
            let mut infos = a.selected_infos();
            if infos.len() > 1 {
                crate::batch_rename::open(&a.window, &infos);
                return;
            }
            let Some(info) = infos.pop() else { return };
            let file = item_file(&info);
            let name = info.display_name().to_string();
            a.prompt("Rename", &name, "Rename", move |new| {
                file.set_display_name(&new, None::<&gio::Cancellable>)
                    .map(|_| ())
                    .map_err(|e| e.message().to_string())
            });
        }));
        // Properties of the selection, or of the current folder when
        // nothing is selected (the background menu clears the selection).
        add("properties", Box::new(|a| {
            let mut files: Vec<gio::File> = a.selected_infos().iter().map(item_file).collect();
            if files.is_empty() {
                files.extend(a.current_dir());
            }
            crate::properties::show(&a.window, files);
        }));
        add("new-folder", Box::new(|a| {
            let Some(dir) = a.current_dir().filter(|d| !is_trash(d)) else { return };
            a.prompt("New Folder", "New Folder", "Create", move |name| {
                if let Some(p) = folder_name_problem(&name) {
                    return Err(p.to_string());
                }
                dir.child(&name)
                    .make_directory(None::<&gio::Cancellable>)
                    .map_err(|e| e.message().to_string())
            });
        }));

        let unpin = gio::SimpleAction::new("unpin", Some(glib::VariantTy::STRING));
        let a = Rc::downgrade(self);
        unpin.connect_activate(move |_, v| {
            let Some(a) = a.upgrade() else { return };
            if let Some(uri) = v.and_then(|v| v.get::<String>()) {
                a.set_pinned(uri, false);
            }
        });
        self.window.add_action(&unpin);
        for (name, up) in [("side-up", true), ("side-down", false)] {
            let act = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let a = Rc::downgrade(self);
            act.connect_activate(move |_, v| {
                let Some(a) = a.upgrade() else { return };
                if let Some(k) = v.and_then(|v| v.get::<String>()) {
                    a.move_sidebar_item(&k, up);
                }
            });
            self.window.add_action(&act);
        }
        let stab = gio::SimpleAction::new("sidebar-tab", Some(glib::VariantTy::INT32));
        let a = Rc::downgrade(self);
        stab.connect_activate(move |_, v| {
            let Some(a) = a.upgrade() else { return };
            if let Some(i) = v.and_then(|v| v.get::<i32>()) {
                a.open_sidebar_in_tab(i as usize);
            }
        });
        self.window.add_action(&stab);

        let accels: &[(&str, &[&str])] = &[
            ("win.new-tab", &["<Control>t"]),
            ("win.close-tab", &["<Control>w"]),
            ("win.next-tab", &["<Control>Page_Down", "<Control>Tab"]),
            ("win.prev-tab", &["<Control>Page_Up", "<Control><Shift>Tab"]),
            ("win.back", &["<Alt>Left"]),
            ("win.forward", &["<Alt>Right"]),
            ("win.up", &["<Alt>Up"]),
            ("win.refresh", &["F5", "<Control>r"]),
            ("win.focus-search", &["<Control>f"]),
            ("win.focus-path", &["<Control>l"]),
            ("win.toggle-hidden", &["<Control>h"]),
            ("win.pin-current", &["<Control>d"]),
            ("win.new-folder", &["<Control><Shift>n"]),
            ("win.terminal-here", &["<Control><Alt>t"]),
            ("win.grid-view", &["<Control>1"]),
            ("win.list-view", &["<Control>2"]),
            ("win.properties", &["<Alt>Return", "<Control>i"]),
        ];
        for (action, keys) in accels {
            gapp.set_accels_for_action(action, keys);
        }
    }

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
            if let Some(loc) = a.sidebar_loc(row.index() as usize) {
                a.navigate(loc);
            }
        });
        for sig in ["mount-added", "mount-removed", "mount-changed"] {
            let a = Rc::downgrade(self);
            let id = self.volumes.connect_local(sig, false, move |_| {
                let a = a.upgrade()?;
                a.rebuild_sidebar();
                None
            });
            track(self.volumes.upcast_ref(), id);
        }

        // Path bar: type a path and press Enter
        let a = Rc::downgrade(self);
        self.path_entry.connect_activate(move |e| {
            let Some(a) = a.upgrade() else { return };
            let mut t = e.text().trim().to_string();
            if t == "~" || t.starts_with("~/") {
                t = format!("{}{}", glib::home_dir().display(), &t[1..]);
            }
            let f = gio::File::for_commandline_arg(&t);
            match f.query_file_type(gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>) {
                gio::FileType::Directory | gio::FileType::Mountable => a.navigate(Loc::Dir(f)),
                gio::FileType::Unknown if f.uri_scheme().map(|s| s != "file").unwrap_or(false) => {
                    a.navigate(Loc::Dir(f))
                }
                gio::FileType::Unknown => a.status.set_text(&format!("“{t}” doesn't exist")),
                _ => a.launch(&f, false),
            }
            a.grid.grab_focus();
        });

        // Search box (top right)
        let a = Rc::downgrade(self);
        self.search.connect_search_changed(move |e| {
            let Some(a) = a.upgrade() else { return };
            let q = e.text().to_lowercase();
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
                Loc::Starred => return,
            };
            a.run_search(root, q);
        });
        let a = Rc::downgrade(self);
        self.search.connect_stop_search(move |e| {
            let Some(a) = a.upgrade() else { return };
            e.set_text("");
            a.grid.grab_focus();
        });

        // Toggles
        let a = Rc::downgrade(self);
        self.hidden_btn.connect_toggled(move |b| {
            let Some(a) = a.upgrade() else { return };
            a.show_hidden.set(b.is_active());
            a.filter.changed(gtk::FilterChange::Different);
        });
        let a = Rc::downgrade(self);
        self.view_btn.connect_toggled(move |b| {
            let Some(a) = a.upgrade() else { return };
            a.view_stack
                .set_visible_child_name(if b.is_active() { "list" } else { "grid" });
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

fn delete_recursive(f: &gio::File) -> Result<(), glib::Error> {
    if f.query_file_type(gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, None::<&gio::Cancellable>)
        == gio::FileType::Directory
    {
        let en = f.enumerate_children("standard::name", gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, None::<&gio::Cancellable>)?;
        while let Some(info) = en.next_file(None::<&gio::Cancellable>)? {
            delete_recursive(&f.child(info.name()))?;
        }
    }
    f.delete(None::<&gio::Cancellable>)
}

/// Why `name` can't be a new folder's name, if it can't. `child()` would read "a/b" or ".." as a path and
/// create the folder somewhere else, so these are refused before it is called.
fn folder_name_problem(name: &str) -> Option<&'static str> {
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

/// Recursive name search, run on a worker thread.
fn search_walk(root: &Path, q: &str, hidden: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    let skip: &[&str] = &["/proc", "/sys", "/dev", "/run", "/snap"];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            visited += 1;
            if visited > SEARCH_MAX_VISITED || out.len() >= SEARCH_MAX_RESULTS {
                return out;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !hidden && name.starts_with('.') {
                continue;
            }
            let path = e.path();
            if name.to_lowercase().contains(q) {
                out.push(path.clone());
            }
            if let Ok(ft) = e.file_type() {
                if ft.is_dir() && !skip.iter().any(|s| path == Path::new(s)) {
                    stack.push(path);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
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
