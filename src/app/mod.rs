use crate::config::Config;
use crate::fileops;
use crate::i18n::{ntr, tr};
use crate::preview::Preview;
use crate::util::*;
use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

mod actions;
mod file_actions;
mod layout;
mod menus;
mod navigation;
mod open;
mod path_entry;
mod search;
mod settings;
mod sidebar;
mod tabs;
mod view_restore;
mod views;
mod window;

pub use self::window::build;
use self::file_actions::*;
use self::menus::*;
use self::path_entry::*;
use self::sidebar::*;
use self::tabs::*;
use self::view_restore::*;
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

#[derive(Clone)]
pub enum Loc {
    Dir(gio::File),
    Starred,
    /// The newest entries of the Downloads folder, newest first
    RecentDownloads,
    Search { root: gio::File, query: String, results: Rc<Vec<PathBuf>>, end: SearchEnd },
}

/// Why a deep search walk stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchEnd {
    Done,
    ResultLimit,
    VisitedLimit,
    Cancelled,
}

impl SearchEnd {
    /// What to tell the user when a limit cut the search short.
    fn note(self) -> Option<String> {
        use crate::properties::group_digits;
        match self {
            SearchEnd::ResultLimit => Some(format!("first {} results", group_digits(SEARCH_MAX_RESULTS as u64))),
            SearchEnd::VisitedLimit => Some(format!("stopped after {} files", group_digits(SEARCH_MAX_VISITED as u64))),
            SearchEnd::Done | SearchEnd::Cancelled => None,
        }
    }
}

impl Loc {
    fn title(&self) -> String {
        match self {
            Loc::Dir(f) => file_title(f),
            Loc::Starred => "Starred".into(),
            Loc::RecentDownloads => "Recent Downloads".into(),
            Loc::Search { query, .. } => format!("“{query}”"),
        }
    }
    fn icon(&self) -> &'static str {
        match self {
            Loc::Dir(f) => icon_for_file(f),
            Loc::Starred => "starred-symbolic",
            Loc::RecentDownloads => "folder-download-symbolic",
            Loc::Search { .. } => "system-search-symbolic",
        }
    }
    fn key(&self) -> String {
        match self {
            Loc::Dir(f) => f.uri().trim_end_matches('/').to_string(),
            Loc::Starred => "starred:".into(),
            Loc::RecentDownloads => "recent-downloads:".into(),
            Loc::Search { query, .. } => format!("search:{query}"),
        }
    }
}

#[cfg(test)]
mod loc_grid_tests {
    use super::*;

    #[test]
    fn grid_columns() {
        assert_eq!(grid_columns_for_width(0.0), 4);
        assert_eq!(grid_columns_for_width(-10.0), 4);
        assert_eq!(grid_columns_for_width(f64::NAN), 4);
        assert_eq!(grid_columns_for_width(640.0), 10);
        assert_eq!(grid_columns_for_width(641.0), 11);
        assert_eq!(grid_columns_for_width(1e9), 64);
        assert_eq!(grid_columns_for_width(f64::INFINITY), 64);
        // Never fewer columns than the narrowest tiles need, up to the cap.
        for w in (0..5000).step_by(7) {
            let w = w as f64;
            assert!(grid_columns_for_width(w) >= ((w / GRID_MIN_TILE_PX) as u32).min(64), "{w}");
        }
    }

    #[test]
    fn loc_keys_and_titles() {
        let _gvfs = crate::util::gvfs_lock();
        let tmp = Loc::Dir(gio::File::for_uri("file:///tmp/a%20b/"));
        assert_eq!((tmp.key(), tmp.title()), ("file:///tmp/a%20b".to_string(), "a b".to_string()));
        // With and without a trailing slash is the same place.
        assert_eq!(Loc::Dir(gio::File::for_path("/tmp/a b")).key(), tmp.key());
        assert_eq!(Loc::Dir(gio::File::for_uri("trash:///")).title(), "Trash");
        assert_eq!((Loc::Starred.key(), Loc::Starred.title()), ("starred:".to_string(), "Starred".to_string()));
        let search = Loc::Search {
            root: gio::File::for_path("/tmp"),
            query: "cat".into(),
            results: Rc::new(Vec::new()),
            end: SearchEnd::Done,
        };
        assert_eq!((search.key(), search.title()), ("search:cat".to_string(), "“cat”".to_string()));
    }
}

struct Tab {
    loc: Loc,
    /// How `loc` looked when it was last left (tab switch, reload); put back on the next load
    view: Option<ViewState>,
    back: Vec<(Loc, Option<ViewState>)>,
    fwd: Vec<(Loc, Option<ViewState>)>,
    widget: gtk::Box,
    label: gtk::Label,
    icon: gtk::Image,
    close: gtk::Button,
}

pub struct App {
    pub window: gtk::ApplicationWindow,
    tabs: RefCell<Vec<Tab>>,
    current: Cell<usize>,
    /// Tabs closed in this window, newest last, for Ctrl+Shift+T
    closed_tabs: RefCell<ClosedStack<ClosedTab>>,
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
    /// Candidate folders shown when Tab can't narrow the typed path further.
    path_pop: gtk::Popover,
    back_btn: gtk::Button,
    fwd_btn: gtk::Button,
    up_btn: gtk::Button,
    hidden_btn: gtk::ToggleButton,
    view_btn: gtk::ToggleButton,
    sidebar_btn: gtk::ToggleButton,
    preview_btn: gtk::ToggleButton,
    settings_btn: gtk::MenuButton,
    pin_btn: gtk::Button,
    search: gtk::SearchEntry,
    /// "Also search inside text files"; kept for the session only
    search_content: gtk::CheckButton,
    /// Lower-case query of the last search that also looked inside files; its results aren't name-filtered
    content_query: RefCell<Option<String>>,
    pub(crate) status: gtk::Label,
    /// One progress row per running copy / move, above the status bar
    pub(crate) transfers: gtk::Box,
    /// The last operation's result, between the transfer rows and the status bar
    banner: Rc<crate::banner::Banner>,
    view_stack: gtk::Stack,
    grid: gtk::GridView,
    list: gtk::ColumnView,
    grid_scroll: gtk::ScrolledWindow,
    list_scroll: gtk::ScrolledWindow,
    /// Icon + text over the file view when there is nothing to show
    empty_box: gtk::Box,
    empty_icon: gtk::Image,
    empty_label: gtk::Label,
    preview: Rc<Preview>,
    previewed: RefCell<Option<String>>,
    sidebar: gtk::ListBox,
    side_scroll: gtk::ScrolledWindow,
    /// Width of `inner` when the preview pane was last hidden (0: never), see `preview_show_position`
    preview_hidden_at: Cell<i32>,
    sidebar_targets: RefCell<Vec<Option<SideTarget>>>,
    outer: gtk::Paned,
    inner: gtk::Paned,
    volumes: gio::VolumeMonitor,
    /// The settings, shared with every other window of this process
    config: Rc<RefCell<Config>>,
    search_gen: Cell<u64>,
    /// Set to stop the running deep search walk, if there is one
    search_cancel: RefCell<Option<Arc<AtomicBool>>>,
    layout_ready: Cell<bool>,
    /// File name to select once the folder has loaded ("Show in folder")
    pending_select: RefCell<Option<String>>,
    /// The saved view of the place being loaded, until it is put back
    restore: RefCell<Option<Restore>>,
    /// Bumped whenever `restore` is replaced or dropped, so stale frame callbacks stop
    restore_gen: Cell<u64>,
    /// Handlers on objects that outlive the window (models, the volume
    /// monitor); disconnected on close so a closed window is freed.
    model_handlers: RefCell<Vec<(glib::Object, glib::SignalHandlerId)>>,
    save_pending: Cell<bool>,
    /// Sort key and reversed flag of both views. Set only from the list view's
    /// header sorter (`header_sort_changed`), which the menu drives too.
    sort: Rc<Cell<(SortKey, bool)>>,
    /// The file model's sorter; reads `sort`
    sorter: gtk::CustomSorter,
    /// Set while the model is already in its order (Recent Downloads): the sorter keeps it
    keep_order: Rc<Cell<bool>>,
    /// Watches Downloads while Recent Downloads is shown, so new downloads appear
    downloads_monitor: RefCell<Option<gio::FileMonitor>>,
}

thread_local! {
    /// Owns every open window's `App`. Signal handlers only hold weak
    /// references, so removing an entry when its window is destroyed frees
    /// the whole window (the process is shared by all windows).
    static OPEN: RefCell<Vec<Rc<App>>> = const { RefCell::new(Vec::new()) };
}

/// The windows that are open (not closing), for results whose own window is gone (`jobs::deliver`).
pub(crate) fn open_windows() -> Vec<Rc<App>> {
    OPEN.with(|o| o.borrow().iter().filter(|a| a.window.is_visible()).cloned().collect())
}

/// Runs `f` for every shown window, including the one asking; a window being closed is left alone.
fn for_each_window(f: impl Fn(&Rc<App>)) {
    // A copy: `f` may open or close windows
    let apps = OPEN.with(|o| o.borrow().clone());
    apps.iter().filter(|a| a.window.is_visible()).for_each(f);
}

impl App {
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

    /// Shows how an operation ended in the result banner, where it survives the next click. The status bar goes
    /// back to the item counts, or shows the result itself while an earlier error is kept in the banner.
    pub(crate) fn notify(&self, n: crate::banner::Notice) {
        self.update_status();
        if let Some(n) = self.banner.show(n) {
            self.status.set_text(&n.text);
        }
    }

    /// `notify` for code that has no `App`, holding the window weakly; see `jobs::deliver` for a closed window.
    fn notifier(self: &Rc<Self>) -> crate::archive::Notify {
        let w = Rc::downgrade(self);
        Rc::new(move |n| crate::jobs::deliver(&w, n))
    }

    /// Saves the shared settings. A failure is shown once, until a save works again; the next change retries.
    fn save_config(&self) {
        let err = self.config.borrow_mut().save();
        if let Some(e) = err {
            self.notify(crate::banner::Notice { text: format!("Couldn't save settings — {e}"), errors: vec![e] });
        }
    }
}
