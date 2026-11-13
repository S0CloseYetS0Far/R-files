use crate::config::Config;
use crate::fileops;
use crate::preview::Preview;
use crate::util::*;
use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
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

#[derive(Clone)]
pub enum Loc {
    Dir(gio::File),
    Starred,
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

/// Menu key or Shift+F10: the keyboard's way to ask for a right-click menu.
fn is_menu_key(key: gdk::Key, mods: gdk::ModifierType) -> bool {
    let other = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK;
    match key {
        gdk::Key::Menu => !mods.intersects(other),
        gdk::Key::F10 => mods.contains(gdk::ModifierType::SHIFT_MASK) && !mods.intersects(other),
        _ => false,
    }
}

/// A focus move inside a popup menu.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MenuNav {
    Up,
    Down,
    Home,
    End,
}

/// Where focus goes in a popup menu. `usable[i]` is false for separators and disabled
/// entries; Up and Down wrap at the ends. None when nothing can take focus.
fn menu_nav_target(usable: &[bool], current: Option<usize>, nav: MenuNav) -> Option<usize> {
    let n = usable.len();
    let first = usable.iter().position(|&u| u)?;
    let last = usable.iter().rposition(|&u| u)?;
    let cur = current.filter(|&c| c < n);
    match (nav, cur) {
        (MenuNav::Home, _) | (MenuNav::Down, None) => Some(first),
        (MenuNav::End, _) | (MenuNav::Up, None) => Some(last),
        (MenuNav::Down, Some(c)) => (1..=n).map(|d| (c + d) % n).find(|&i| usable[i]),
        (MenuNav::Up, Some(c)) => (1..=n).map(|d| (c + n - d) % n).find(|&i| usable[i]),
    }
}

#[cfg(test)]
mod menu_nav_tests {
    use super::{is_menu_key, menu_nav_target, MenuNav::*};
    use gtk::gdk;

    // entry, separator, entry, disabled entry, entry
    const M: [bool; 5] = [true, false, true, false, true];

    #[test]
    fn down_skips_separators_and_wraps() {
        assert_eq!(menu_nav_target(&M, Some(0), Down), Some(2));
        assert_eq!(menu_nav_target(&M, Some(2), Down), Some(4));
        assert_eq!(menu_nav_target(&M, Some(4), Down), Some(0));
        assert_eq!(menu_nav_target(&M, None, Down), Some(0));
    }

    #[test]
    fn up_skips_separators_and_wraps() {
        assert_eq!(menu_nav_target(&M, Some(4), Up), Some(2));
        assert_eq!(menu_nav_target(&M, Some(2), Up), Some(0));
        assert_eq!(menu_nav_target(&M, Some(0), Up), Some(4));
        assert_eq!(menu_nav_target(&M, None, Up), Some(4));
    }

    #[test]
    fn home_and_end_jump_to_usable_ends() {
        let m = [false, true, true, false];
        assert_eq!(menu_nav_target(&m, Some(2), Home), Some(1));
        assert_eq!(menu_nav_target(&m, Some(1), End), Some(2));
        assert_eq!(menu_nav_target(&m, None, Home), Some(1));
    }

    #[test]
    fn single_entry_and_out_of_range_current() {
        assert_eq!(menu_nav_target(&[true], Some(0), Down), Some(0));
        assert_eq!(menu_nav_target(&[true], Some(0), Up), Some(0));
        assert_eq!(menu_nav_target(&M, Some(9), Down), Some(0));
    }

    #[test]
    fn nothing_focusable() {
        for nav in [Up, Down, Home, End] {
            assert_eq!(menu_nav_target(&[], None, nav), None);
            assert_eq!(menu_nav_target(&[false, false], Some(0), nav), None);
        }
    }

    #[test]
    fn menu_keys() {
        let none = gdk::ModifierType::empty();
        let shift = gdk::ModifierType::SHIFT_MASK;
        assert!(is_menu_key(gdk::Key::Menu, none));
        assert!(is_menu_key(gdk::Key::F10, shift));
        assert!(!is_menu_key(gdk::Key::F10, none));
        assert!(!is_menu_key(gdk::Key::F10, shift | gdk::ModifierType::CONTROL_MASK));
        assert!(!is_menu_key(gdk::Key::F2, shift));
    }
}

/// Moves focus between the entries of a `popup_menu` box.
fn menu_focus(bx: &gtk::Box, nav: MenuNav) {
    let kids: Vec<gtk::Widget> =
        std::iter::successors(bx.first_child(), |w| w.next_sibling()).collect();
    let usable: Vec<bool> =
        kids.iter().map(|w| w.is::<gtk::Button>() && w.is_visible() && w.is_sensitive()).collect();
    let current = bx.focus_child().and_then(|f| kids.iter().position(|w| *w == f));
    if let Some(i) = menu_nav_target(&usable, current, nav) {
        kids[i].grab_focus();
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

/// Most selected names remembered per history entry, so history stays small.
const SAVED_NAMES_MAX: usize = 200;

/// The view of a place as it was when it was left.
#[derive(Clone, Debug, Default, PartialEq)]
struct ViewState {
    /// Selected file names, at most `SAVED_NAMES_MAX`
    names: Vec<String>,
    /// Grid and list scroll offsets; `None` scrolls to the first selected name instead
    scroll: Option<[f64; 2]>,
}

impl ViewState {
    /// Only selects `name` and scrolls to it (the folder we came from).
    fn select(name: String) -> Self {
        ViewState { names: vec![name], scroll: None }
    }
}

/// A saved view being put back after a load. Each part is cleared once applied.
#[derive(Debug, Default, PartialEq)]
struct Restore {
    /// Names to select once the place has loaded
    names: Option<Vec<String>>,
    /// Scroll to the first selected name rather than to saved offsets
    to_item: bool,
    /// Grid and list offsets still to apply, each once its view is laid out
    scroll: [Option<f64>; 2],
}

impl From<ViewState> for Restore {
    fn from(v: ViewState) -> Self {
        let scroll = v.scroll.map_or([None; 2], |s| s.map(Some));
        Restore { names: Some(v.names), to_item: v.scroll.is_none(), scroll }
    }
}

/// How the user got to the place being loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nav {
    /// Typed path, sidebar, double-click: start at the top
    Fresh,
    Up,
    Back,
    Forward,
}

/// The view to show on arriving at `arriving` from `left`, given the state
/// saved for `arriving`'s history entry (if any).
fn arrival_view(nav: Nav, saved: Option<ViewState>, left: &Loc, arriving: &Loc) -> Option<ViewState> {
    match nav {
        Nav::Fresh => None,
        Nav::Up => came_from(left, arriving).map(ViewState::select),
        Nav::Back => saved.or_else(|| came_from(left, arriving).map(ViewState::select)),
        Nav::Forward => saved,
    }
}

/// The name of the folder `left` when it sits directly in `arriving`.
fn came_from(left: &Loc, arriving: &Loc) -> Option<String> {
    let (Loc::Dir(child), Loc::Dir(parent)) = (left, arriving) else { return None };
    if !child.parent().is_some_and(|p| p.equal(parent)) {
        return None;
    }
    child.basename().map(|n| n.to_string_lossy().into_owned())
}

/// What to remember for the place being left: the live selection and
/// scroll, except for the parts of a restore that have not landed yet
/// (leaving again before a load finished must not lose the saved state).
fn saved_view(live_names: Vec<String>, live_scroll: [f64; 2], pending: Option<&Restore>) -> ViewState {
    let mut v = match pending {
        None => ViewState { names: live_names, scroll: Some(live_scroll) },
        Some(p) => ViewState {
            names: p.names.clone().unwrap_or(live_names),
            scroll: if p.to_item && p.names.is_some() {
                None
            } else {
                Some([p.scroll[0].unwrap_or(live_scroll[0]), p.scroll[1].unwrap_or(live_scroll[1])])
            },
        },
    };
    v.names.truncate(SAVED_NAMES_MAX);
    v
}

/// Positions in `items` (the shown names, in order) of the saved names that still exist.
fn positions_of(saved: &[String], items: &[String]) -> Vec<u32> {
    let want: std::collections::HashSet<&str> = saved.iter().map(String::as_str).collect();
    items.iter().enumerate().filter(|(_, n)| want.contains(n.as_str())).map(|(i, _)| i as u32).collect()
}

#[cfg(test)]
mod view_restore_tests {
    use super::*;

    fn dir(p: &str) -> Loc {
        Loc::Dir(gio::File::for_path(p))
    }

    fn state(names: &[&str], scroll: Option<[f64; 2]>) -> ViewState {
        ViewState { names: names.iter().map(|s| s.to_string()).collect(), scroll }
    }

    #[test]
    fn fresh_places_start_at_the_top() {
        let saved = Some(state(&["a"], Some([10.0, 20.0])));
        assert_eq!(arrival_view(Nav::Fresh, saved, &dir("/x/y"), &dir("/x")), None);
    }

    #[test]
    fn up_selects_the_folder_we_came_from() {
        assert_eq!(arrival_view(Nav::Up, None, &dir("/x/y"), &dir("/x")), Some(ViewState::select("y".into())));
        let search = Loc::Search {
            root: gio::File::for_path("/x"),
            query: "q".into(),
            results: Rc::new(vec![]),
            end: SearchEnd::Done,
        };
        assert_eq!(arrival_view(Nav::Up, None, &search, &dir("/x")), None);
    }

    #[test]
    fn back_restores_saved_state_or_selects_the_child() {
        let saved = state(&["a", "b"], Some([5.0, 7.0]));
        assert_eq!(arrival_view(Nav::Back, Some(saved.clone()), &dir("/x/y"), &dir("/x")), Some(saved));
        assert_eq!(arrival_view(Nav::Back, None, &dir("/x/y"), &dir("/x")), Some(ViewState::select("y".into())));
        // Back to somewhere unrelated with nothing saved: top
        assert_eq!(arrival_view(Nav::Back, None, &dir("/x/y"), &dir("/z")), None);
    }

    #[test]
    fn forward_restores_only_saved_state() {
        let saved = state(&["c"], Some([1.0, 2.0]));
        assert_eq!(arrival_view(Nav::Forward, Some(saved.clone()), &dir("/x"), &dir("/x/y")), Some(saved));
        assert_eq!(arrival_view(Nav::Forward, None, &dir("/x/y"), &dir("/x")), None);
    }

    #[test]
    fn came_from_needs_a_direct_child() {
        assert_eq!(came_from(&dir("/x/y"), &dir("/x")), Some("y".into()));
        assert_eq!(came_from(&dir("/x/y/z"), &dir("/x")), None);
        assert_eq!(came_from(&dir("/x"), &dir("/x/y")), None);
        assert_eq!(came_from(&Loc::Starred, &dir("/x")), None);
    }

    #[test]
    fn leaving_saves_the_live_view_capped() {
        let names: Vec<String> = (0..500).map(|i| format!("f{i}")).collect();
        let v = saved_view(names, [3.0, 4.0], None);
        assert_eq!(v.names.len(), SAVED_NAMES_MAX);
        assert_eq!(v.names[0], "f0");
        assert_eq!(v.scroll, Some([3.0, 4.0]));
    }

    #[test]
    fn leaving_before_the_restore_landed_keeps_the_saved_parts() {
        // Nothing applied yet: the saved state survives as it was
        let p = Restore::from(state(&["a"], Some([50.0, 60.0])));
        assert_eq!(saved_view(vec![], [0.0, 0.0], Some(&p)), state(&["a"], Some([50.0, 60.0])));
        // Selection and grid applied, the hidden list view's offset still waits
        let p = Restore { names: None, to_item: false, scroll: [None, Some(60.0)] };
        assert_eq!(saved_view(vec!["b".into()], [55.0, 0.0], Some(&p)), state(&["b"], Some([55.0, 60.0])));
        // A "select the child" restore not yet applied stays one
        let p = Restore::from(ViewState::select("y".into()));
        assert_eq!(saved_view(vec![], [0.0, 0.0], Some(&p)), ViewState::select("y".into()));
    }

    #[test]
    fn restore_selects_only_names_that_still_exist() {
        let saved = vec!["b".to_string(), "gone".into(), "d".into()];
        let items: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(positions_of(&saved, &items), vec![1, 3]);
        assert!(positions_of(&[], &items).is_empty());
        assert!(positions_of(&saved, &[]).is_empty());
    }
}

/// Where a sidebar row goes.
enum SideTarget {
    Place(Loc),
    /// A volume that isn't mounted: activating the row mounts it, then opens it
    Volume(gio::Volume),
}

/// What the Devices section needs to know about a volume; plain data so the choice is testable.
struct VolFacts {
    name: String,
}

/// What the Devices section needs to know about a mount.
struct MountFacts {
    /// Index into the volumes list of the volume this mount belongs to, if it's listed
    volume: Option<usize>,
    shadowed: bool,
}

/// One Devices row: a volume (mounted or not) or a mount that has no listed volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevRow {
    Volume(usize),
    Mount(usize),
}

/// Rows for the Devices section, each drive once: every named volume, then the mounts
/// no shown volume covers (network shares, MTP phones…), leaving out shadowed ones.
fn device_rows(vols: &[VolFacts], mounts: &[MountFacts]) -> Vec<DevRow> {
    let shown: Vec<bool> = vols.iter().map(|v| !v.name.trim().is_empty()).collect();
    let mut rows: Vec<DevRow> = (0..vols.len()).filter(|&i| shown[i]).map(DevRow::Volume).collect();
    rows.extend(
        mounts
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.shadowed && !m.volume.is_some_and(|v| shown.get(v).copied().unwrap_or(false)))
            .map(|(i, _)| DevRow::Mount(i)),
    );
    rows
}

/// Tooltip of a mounted row's eject button, or None when it gets no button.
fn eject_tooltip(can_eject: bool, can_unmount: bool) -> Option<&'static str> {
    if can_eject {
        Some("Eject")
    } else if can_unmount {
        Some("Unmount")
    } else {
        None
    }
}

/// Status bar text once an eject or unmount of `name` has finished.
fn eject_message(name: &str, eject: bool, err: Option<&str>) -> String {
    match (err, eject) {
        (None, true) => format!("You can now remove “{name}”"),
        (None, false) => format!("Unmounted “{name}”"),
        (Some(e), true) => format!("Couldn't eject “{name}”: {e}"),
        (Some(e), false) => format!("Couldn't unmount “{name}”: {e}"),
    }
}

/// Whether `loc` shows a folder on the mount whose root is `root`.
fn loc_on_mount(loc: &Loc, root: &gio::File) -> bool {
    let f = match loc {
        Loc::Dir(f) | Loc::Search { root: f, .. } => f,
        Loc::Starred => return false,
    };
    f.equal(root) || f.has_prefix(root)
}

#[cfg(test)]
mod devices_tests {
    use super::{device_rows, eject_message, eject_tooltip, loc_on_mount, DevRow, Loc, MountFacts, VolFacts};
    use gtk::gio;
    use gtk::prelude::*;

    fn vol(name: &str) -> VolFacts {
        VolFacts { name: name.into() }
    }

    #[test]
    fn volumes_and_their_mounts_show_once() {
        let vols = [vol("USB stick"), vol("Data")];
        // Mount 0 is the USB stick's, mount 1 a network share with no volume
        let mounts = [MountFacts { volume: Some(0), shadowed: false }, MountFacts { volume: None, shadowed: false }];
        assert_eq!(device_rows(&vols, &mounts), vec![DevRow::Volume(0), DevRow::Volume(1), DevRow::Mount(1)]);
    }

    #[test]
    fn nameless_volumes_and_shadowed_mounts_are_left_out() {
        let vols = [vol(""), vol("  "), vol("Backup")];
        let mounts = [
            MountFacts { volume: None, shadowed: true },
            // Its volume is hidden, so the mount shows on its own
            MountFacts { volume: Some(0), shadowed: false },
            MountFacts { volume: Some(2), shadowed: false },
        ];
        assert_eq!(device_rows(&vols, &mounts), vec![DevRow::Volume(2), DevRow::Mount(1)]);
        assert!(device_rows(&[], &[]).is_empty());
    }

    #[test]
    fn eject_button_and_tooltip() {
        assert_eq!(eject_tooltip(true, true), Some("Eject"));
        assert_eq!(eject_tooltip(true, false), Some("Eject"));
        assert_eq!(eject_tooltip(false, true), Some("Unmount"));
        assert_eq!(eject_tooltip(false, false), None);
    }

    #[test]
    fn eject_messages() {
        assert_eq!(eject_message("Stick", true, None), "You can now remove “Stick”");
        assert_eq!(eject_message("Data", false, None), "Unmounted “Data”");
        assert_eq!(eject_message("Stick", true, Some("busy")), "Couldn't eject “Stick”: busy");
        assert_eq!(eject_message("Data", false, Some("busy")), "Couldn't unmount “Data”: busy");
    }

    #[test]
    fn tabs_on_a_mount() {
        let root = gio::File::for_path("/media/me/STICK");
        assert!(loc_on_mount(&Loc::Dir(root.clone()), &root));
        assert!(loc_on_mount(&Loc::Dir(root.child("a/b")), &root));
        assert!(!loc_on_mount(&Loc::Dir(gio::File::for_path("/media/me/STICK2")), &root));
        assert!(!loc_on_mount(&Loc::Dir(gio::File::for_path("/media")), &root));
        assert!(!loc_on_mount(&Loc::Starred, &root));
    }
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
    pub(crate) status: gtk::Label,
    /// One progress row per running copy / move, above the status bar
    pub(crate) transfers: gtk::Box,
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
    config: RefCell<Config>,
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
}

thread_local! {
    /// Owns every open window's `App`. Signal handlers only hold weak
    /// references, so removing an entry when its window is destroyed frees
    /// the whole window (the process is shared by all windows).
    static OPEN: RefCell<Vec<Rc<App>>> = const { RefCell::new(Vec::new()) };
}

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

/// Arrow and Home/End keys on a focused tab.
#[derive(Clone, Copy)]
enum TabNav {
    Left,
    Right,
    Home,
    End,
}

/// Which tab gets focus after `key` on tab `focused` of `n`. Stops at the first tab; Right on the last tab
/// returns `n`, meaning the + button.
fn tab_focus_target(n: usize, focused: usize, key: TabNav) -> usize {
    match key {
        TabNav::Left => focused.saturating_sub(1),
        TabNav::Right => (focused + 1).min(n),
        TabNav::Home => 0,
        TabNav::End => n.saturating_sub(1),
    }
}

/// The current tab after closing tab `i` of `n` while `cur` was current (`n` is the count before closing).
fn current_after_close(n: usize, cur: usize, i: usize) -> usize {
    if i < cur {
        cur - 1
    } else if i == cur {
        i.min(n - 2)
    } else {
        cur
    }
}

#[cfg(test)]
mod tab_focus_tests {
    use super::*;

    #[test]
    fn arrows_stop_at_the_ends() {
        assert_eq!(tab_focus_target(3, 1, TabNav::Left), 0);
        assert_eq!(tab_focus_target(3, 0, TabNav::Left), 0);
        assert_eq!(tab_focus_target(3, 1, TabNav::Right), 2);
        assert_eq!(tab_focus_target(3, 2, TabNav::Right), 3); // the + button
        assert_eq!(tab_focus_target(1, 0, TabNav::Right), 1);
    }

    #[test]
    fn home_and_end() {
        assert_eq!(tab_focus_target(4, 2, TabNav::Home), 0);
        assert_eq!(tab_focus_target(4, 1, TabNav::End), 3);
        assert_eq!(tab_focus_target(1, 0, TabNav::End), 0);
    }

    #[test]
    fn current_after_closing() {
        // closing the current tab: the one that slid into its place, or the new last one
        assert_eq!(current_after_close(4, 1, 1), 1);
        assert_eq!(current_after_close(4, 3, 3), 2);
        assert_eq!(current_after_close(2, 0, 0), 0);
        // closing an earlier tab shifts the current one down, a later one leaves it
        assert_eq!(current_after_close(4, 2, 0), 1);
        assert_eq!(current_after_close(4, 1, 3), 1);
    }
}

fn set_tab_title(widget: &gtk::Box, label: &gtk::Label, title: &str) {
    label.set_text(title);
    widget.update_property(&[gtk::accessible::Property::Label(title)]);
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
    let sorter = gtk::CustomSorter::new(move |a, b| {
        let (Some(a), Some(b)) = (a.downcast_ref::<gio::FileInfo>(), b.downcast_ref::<gio::FileInfo>()) else {
            return gtk::Ordering::Equal;
        };
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
        status,
        transfers,
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
        config: RefCell::new(config),
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
    app.view_stack.set_visible_child_name(if list_view { "list" } else { "grid" });
    // Applied before the buttons, so their toggled handlers find nothing to change or save
    app.side_scroll.set_visible(show_sidebar);
    app.preview.root.set_visible(show_preview);
    app.sidebar_btn.set_active(show_sidebar);
    app.preview_btn.set_active(show_preview);

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
        col.set_id(Some(SortKey::ALL[kind as usize].as_str()));
        // Only makes the header clickable and show the arrow: the model is sorted by
        // `App::sorter`, because a reversed column sorter would put folders last.
        col.set_sorter(Some(&gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal)));
        self.list.append_column(&col);
    }

    /// Sorts both views. Goes through the list view's header sorter so its arrow
    /// always matches; `header_sort_changed` does the rest.
    fn set_sort(&self, key: SortKey, desc: bool) {
        let cols = self.list.columns();
        let col = (0..cols.n_items())
            .filter_map(|i| cols.item(i).and_downcast::<gtk::ColumnViewColumn>())
            .find(|c| c.id().as_deref() == Some(key.as_str()));
        let order = if desc { gtk::SortType::Descending } else { gtk::SortType::Ascending };
        self.list.sort_by_column(col.as_ref(), order);
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
        let mut c = self.config.borrow_mut();
        (c.sort, c.sort_desc) = state;
        c.save();
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
        // The role can only be set at construction
        let bx = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .accessible_role(gtk::AccessibleRole::Menu)
            .build();
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
                    let b = gtk::Button::builder().accessible_role(gtk::AccessibleRole::MenuItem).build();
                    b.update_property(&[gtk::accessible::Property::Label(&m.label)]);
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

        // Keyboard: arrows, Home/End and Tab move between entries; Escape goes back to
        // whatever had focus. Capture, so the box's own arrow handling doesn't stop at the ends.
        let prev = parent.root().and_then(|r| r.focus()).map(|w| w.downgrade());
        let k = gtk::EventControllerKey::new();
        k.set_propagation_phase(gtk::PropagationPhase::Capture);
        let b = bx.downgrade();
        let p = pop.downgrade();
        k.connect_key_pressed(move |_, key, _, mods| {
            let Some(bx) = b.upgrade() else { return glib::Propagation::Proceed };
            let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
            let nav = match key {
                gdk::Key::Up | gdk::Key::KP_Up => MenuNav::Up,
                gdk::Key::Down | gdk::Key::KP_Down => MenuNav::Down,
                gdk::Key::Home | gdk::Key::KP_Home => MenuNav::Home,
                gdk::Key::End | gdk::Key::KP_End => MenuNav::End,
                gdk::Key::ISO_Left_Tab => MenuNav::Up,
                gdk::Key::Tab | gdk::Key::KP_Tab => if shift { MenuNav::Up } else { MenuNav::Down },
                gdk::Key::Escape => {
                    // Focus back first, so closing doesn't drop it somewhere else
                    if let Some(w) = prev.as_ref().and_then(|w| w.upgrade()) {
                        w.grab_focus();
                    }
                    if let Some(p) = p.upgrade() {
                        p.popdown();
                    }
                    return glib::Propagation::Stop;
                }
                _ => return glib::Propagation::Proceed,
            };
            menu_focus(&bx, nav);
            glib::Propagation::Stop
        });
        pop.add_controller(k);

        pop.popup();
        menu_focus(&bx, MenuNav::Home);
    }

    /// Menu key / Shift+F10 on a file view: the item menu for the selection, anchored at
    /// the focused item when it is in view, or the background menu when nothing is selected.
    fn keyboard_menu(&self, scroll: &gtk::ScrolledWindow) {
        let Some(info) = self.selected_infos().into_iter().next() else {
            self.popup_menu(scroll, 16.0, 16.0, self.background_menu());
            return;
        };
        // Like right-click, attach to the view stack, not the tile
        let stack = &self.view_stack;
        let visible = scroll.compute_bounds(stack);
        let focus = scroll.root().and_then(|r| r.focus()).filter(|f| {
            f.is_ancestor(scroll) && !f.is::<gtk::GridView>() && !f.is::<gtk::ListView>() && !f.is::<gtk::ColumnView>()
        });
        let at = focus
            .and_then(|f| f.compute_bounds(stack))
            .filter(|r| visible.is_none_or(|v| v.intersection(r).is_some()))
            .map(|r| (r.x() + (r.width() / 2.0).min(48.0), r.y() + r.height() * 0.75));
        let (x, y) = at.unwrap_or_else(|| visible.map_or((16.0, 16.0), |v| (v.x() + 16.0, v.y() + 16.0)));
        self.popup_menu(stack, x as f64, y as f64, self.item_menu(&info));
    }

    fn item_menu(&self, info: &gio::FileInfo) -> Vec<Option<MItem>> {
        let many = self.selected_infos().len() > 1;
        let dir = is_dir_like(info);
        let trash = self.in_trash();
        let cfg = self.config.borrow();
        let uri = item_file(info).uri().to_string();
        let mut m = Vec::new();
        // Only top-level trash items know where they came from
        let origs: Vec<Option<PathBuf>> = self.selected_infos().iter().map(crate::trash::orig_path).collect();
        if trash && crate::trash::can_restore(&origs) {
            m.push(mi("Restore", "win.restore", ""));
            m.push(None);
        }
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
            if self.in_recent() {
                m.push(mi("Remove from Recent", "win.forget-recent", ""));
            }
            m.push(mi("Move to Trash", "win.trash", "Delete"));
            m.push(mi("Delete Permanently…", "win.delete-permanently", "Shift+Delete"));
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
        m.push(None);
        let (key, desc) = self.sort.get();
        for (k, label) in [
            (SortKey::Name, "Sort by Name"),
            (SortKey::Size, "Sort by Size"),
            (SortKey::Modified, "Sort by Modified"),
            (SortKey::Type, "Sort by Type"),
        ] {
            let accel = if k == key { "✓" } else { "" };
            m.push(Some(MItem { label: label.into(), action: "win.sort-by", target: Some(k.as_str().to_variant()), accel }));
        }
        m.push(mi("Reversed Order", "win.sort-reversed", if desc { "✓" } else { "" }));
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
        // Enter with several items selected opens all of them, not just the focused one
        if self.selection.is_selected(pos) && self.selection.selection().size() > 1 {
            self.open_selected();
            return;
        }
        if let Some(info) = self.selection.item(pos).and_downcast::<gio::FileInfo>() {
            self.open_info(&info);
        }
    }

    /// Opens every selected item: files in their default app, the first folder here, other folders in new tabs.
    fn open_selected(self: &Rc<Self>) {
        let infos = self.selected_infos();
        let plan = plan_open(&infos.iter().map(is_dir_like).collect::<Vec<_>>());
        for &i in &plan.files {
            self.launch(&item_file(&infos[i]), false);
        }
        if let Some(i) = plan.first_dir {
            self.navigate(Loc::Dir(item_file(&infos[i])));
        }
        for &i in &plan.other_dirs {
            self.new_tab(Loc::Dir(item_file(&infos[i])));
        }
        if plan.skipped > 0 {
            self.status.set_text(&format!("Opened the first {MAX_OPEN} of {} items", MAX_OPEN + plan.skipped));
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

    fn go_back(self: &Rc<Self>) {
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

    fn go_forward(self: &Rc<Self>) {
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

    fn go_up(self: &Rc<Self>) {
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

    /// The current view as it should be remembered when leaving it.
    fn capture_view(&self) -> ViewState {
        let names = self
            .selected_infos()
            .iter()
            .take(SAVED_NAMES_MAX)
            .map(|i| i.name().to_string_lossy().into_owned())
            .collect();
        let scroll = [self.grid_scroll.vadjustment().value(), self.list_scroll.vadjustment().value()];
        saved_view(names, scroll, self.restore.borrow().as_ref())
    }

    /// Remembers the current view on the current tab, for a tab switch or reload.
    fn save_view(&self) {
        let v = self.capture_view();
        if let Some(t) = self.tabs.borrow_mut().get_mut(self.current.get()) {
            t.view = Some(v);
        }
    }

    fn drop_restore(&self) {
        self.restore_gen.set(self.restore_gen.get() + 1);
        *self.restore.borrow_mut() = None;
    }

    /// Puts back the selection of a saved view once the place has loaded,
    /// then its scroll offsets once the views are laid out.
    fn apply_restore(self: &Rc<Self>) {
        if self.pending_select.borrow().is_some() {
            // "Show in folder" wins
            self.drop_restore();
            return;
        }
        let (names, to_item) = {
            let mut r = self.restore.borrow_mut();
            let Some(r) = r.as_mut() else { return };
            let Some(names) = r.names.take() else { return };
            (names, r.to_item)
        };
        let n = self.selection.n_items();
        if !names.is_empty() {
            let items: Vec<String> = (0..n)
                .map(|i| {
                    self.selection
                        .item(i)
                        .and_downcast::<gio::FileInfo>()
                        .map(|f| f.name().to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            let pos = positions_of(&names, &items);
            if !pos.is_empty() {
                let sel = gtk::Bitset::new_empty();
                for &p in &pos {
                    sel.add(p);
                }
                // One change for the whole selection, not one per name
                self.selection.set_selection(&sel, &gtk::Bitset::new_range(0, n));
                if to_item {
                    let (none, focus) = (gtk::ListScrollFlags::NONE, gtk::ListScrollFlags::FOCUS);
                    let list = self.view_btn.is_active();
                    let (grid_flags, list_flags) = if list { (none, focus) } else { (focus, none) };
                    self.grid.scroll_to(pos[0], grid_flags, None);
                    self.list.scroll_to(pos[0], None::<&gtk::ColumnViewColumn>, list_flags, None);
                }
            }
        }
        // The shown view now; the other when it is next shown (see setup_view_restore)
        for (i, scroll) in [&self.grid_scroll, &self.list_scroll].into_iter().enumerate() {
            if scroll.is_mapped() {
                self.restore_scroll_when_laid_out(i);
            }
        }
        self.finish_restore_if_done();
    }

    fn finish_restore_if_done(&self) {
        let done = self.restore.borrow().as_ref().is_some_and(|r| r.names.is_none() && r.scroll == [None, None]);
        if done {
            self.drop_restore();
        }
    }

    /// Sets view `i`'s saved offset after a frame has laid it out with the
    /// new contents (a value set before that is clamped to the old size or reset).
    fn restore_scroll_when_laid_out(self: &Rc<Self>, i: usize) {
        if self.restore.borrow().as_ref().and_then(|r| r.scroll[i]).is_none() {
            return;
        }
        let scroll = if i == 0 { &self.grid_scroll } else { &self.list_scroll };
        let gen = self.restore_gen.get();
        let frames = Cell::new(0u32);
        let a = Rc::downgrade(self);
        scroll.add_tick_callback(move |s, _| {
            let Some(a) = a.upgrade() else { return glib::ControlFlow::Break };
            if a.restore_gen.get() != gen {
                return glib::ControlFlow::Break;
            }
            let Some(value) = a.restore.borrow().as_ref().and_then(|r| r.scroll[i]) else {
                return glib::ControlFlow::Break;
            };
            frames.set(frames.get() + 1);
            let adj = s.vadjustment();
            // The first frame's layout runs after this callback; give up
            // waiting for the full height after a few (the folder shrank)
            if frames.get() < 2 || (adj.upper() - adj.page_size() < value && frames.get() < 10) {
                return glib::ControlFlow::Continue;
            }
            adj.set_value(value);
            if let Some(r) = a.restore.borrow_mut().as_mut() {
                r.scroll[i] = None;
            }
            a.finish_restore_if_done();
            glib::ControlFlow::Break
        });
    }

    /// Drops a restore the user has overtaken by scrolling, clicking or
    /// typing in a view, and applies the hidden view's offset when it is shown.
    fn setup_view_restore(self: &Rc<Self>) {
        for (i, scroll) in [&self.grid_scroll, &self.list_scroll].into_iter().enumerate() {
            let c = gtk::EventControllerLegacy::new();
            c.set_propagation_phase(gtk::PropagationPhase::Capture);
            let a = Rc::downgrade(self);
            c.connect_event(move |_, ev| {
                use gdk::EventType as E;
                if matches!(ev.event_type(), E::ButtonPress | E::Scroll | E::KeyPress | E::TouchBegin) {
                    if let Some(a) = a.upgrade() {
                        // The other (hidden) view keeps its offset: Ctrl+1/2 must still restore it
                        if let Some(r) = a.restore.borrow_mut().as_mut() {
                            r.names = None;
                            r.scroll[i] = None;
                        }
                        a.finish_restore_if_done();
                    }
                }
                glib::Propagation::Proceed
            });
            scroll.add_controller(c);

            let a = Rc::downgrade(self);
            scroll.connect_map(move |_| {
                let Some(a) = a.upgrade() else { return };
                // Only once the selection is back, i.e. the place has loaded
                if a.restore.borrow().as_ref().is_some_and(|r| r.names.is_none()) {
                    a.restore_scroll_when_laid_out(i);
                }
            });
        }
    }

    fn load_current(self: &Rc<Self>) {
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

    fn apply_pending_select(&self) {
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
                self.grid.scroll_to(pos, grid_flags, None);
                self.list.scroll_to(pos, None::<&gtk::ColumnViewColumn>, list_flags, None);
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
        self.cancel_search();
    }

    pub(crate) fn reload(self: &Rc<Self>) {
        self.save_view();
        self.load_current();
    }

    fn on_selection_changed(self: &Rc<Self>) {
        // A hidden preview pane does no work; showing it previews the selection
        if !self.preview.root.is_visible() {
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
    fn update_empty_view(&self) {
        let loc = self.current_loc();
        let place = match &loc {
            Loc::Dir(f) if f.uri().trim_end_matches('/') == "trash:" => Place::Trash,
            Loc::Dir(_) => Place::Dir,
            Loc::Starred => Place::Starred,
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

    // ================= tabs =================
    pub fn new_tab(self: &Rc<Self>, loc: Loc) {
        // Focusable so the keyboard and screen readers can reach it; every tab is in the Tab order (no roving focus)
        let bx = gtk::Box::builder()
            .spacing(6)
            .accessible_role(gtk::AccessibleRole::Tab)
            .focusable(true)
            .focus_on_click(false)
            .build();
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
        close.update_property(&[gtk::accessible::Property::Label("Close tab")]);
        // The keyboard closes a tab with Delete on the tab itself
        close.set_focusable(false);
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
        let keys = gtk::EventControllerKey::new();
        let a = Rc::downgrade(self);
        let w = bx.downgrade();
        keys.connect_key_pressed(move |_, key, _, _| {
            let (Some(a), Some(w)) = (a.upgrade(), w.upgrade()) else { return glib::Propagation::Proceed };
            let Some(i) = a.tab_index(&w) else { return glib::Propagation::Proceed };
            a.tab_key(i, key)
        });
        bx.add_controller(keys);
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
        self.save_view();
        let idx = {
            let mut tabs = self.tabs.borrow_mut();
            tabs.push(Tab { loc, view: None, back: vec![], fwd: vec![], widget: bx, label, icon, close });
            tabs.len() - 1
        };
        self.show_tab(idx);
        let adj = self.tab_scroll.hadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper()));
    }

    /// Keys on a focused tab; returns Stop for the ones it handles.
    fn tab_key(self: &Rc<Self>, i: usize, key: gdk::Key) -> glib::Propagation {
        let nav = match key {
            gdk::Key::Left | gdk::Key::KP_Left => Some(TabNav::Left),
            gdk::Key::Right | gdk::Key::KP_Right => Some(TabNav::Right),
            gdk::Key::Home | gdk::Key::KP_Home => Some(TabNav::Home),
            gdk::Key::End | gdk::Key::KP_End => Some(TabNav::End),
            _ => None,
        };
        if let Some(nav) = nav {
            let n = self.tabs.borrow().len();
            let to = tab_focus_target(n, i, nav);
            if to >= n {
                self.plus.grab_focus();
            } else if let Some(t) = self.tabs.borrow().get(to) {
                t.widget.grab_focus();
            }
            return glib::Propagation::Stop;
        }
        match key {
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space => self.switch_to(i),
            gdk::Key::Delete | gdk::Key::KP_Delete => self.close_tab(i),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    fn tab_index(&self, w: &gtk::Box) -> Option<usize> {
        self.tabs.borrow().iter().position(|t| &t.widget == w)
    }

    fn switch_to(self: &Rc<Self>, i: usize) {
        if i >= self.tabs.borrow().len() {
            return;
        }
        self.save_view();
        self.show_tab(i);
    }

    /// Makes tab `i` current and loads it, without saving the view being left.
    fn show_tab(self: &Rc<Self>, i: usize) {
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
                t.widget.update_state(&[gtk::accessible::State::Selected(Some(j == i))]);
                t.close.set_visible(many);
            }
        }
        self.current.set(i);
        self.load_current();
    }

    fn close_tab(self: &Rc<Self>, i: usize) {
        // Before the indexes shift; harmless when the closed tab is the current one
        self.save_view();
        let (new_cur, had_focus) = {
            let mut tabs = self.tabs.borrow_mut();
            let n = tabs.len();
            if n <= 1 || i >= n {
                return;
            }
            let t = tabs.remove(i);
            let had_focus = t.widget.has_focus();
            self.tab_strip.remove(&t.widget);
            (current_after_close(n, self.current.get(), i), had_focus)
        };
        self.show_tab(new_cur);
        // Keep keyboard focus in the strip, on the tab that became current
        if had_focus {
            if let Some(t) = self.tabs.borrow().get(new_cur) {
                t.widget.grab_focus();
            }
        }
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
        // Pins known to be missing stay dimmed until the new check answers, so they don't flicker
        let was_missing: Vec<String> = (0..self.sidebar_targets.borrow().len())
            .filter(|&i| self.sidebar.row_at_index(i as i32).is_some_and(|r| r.has_css_class("pin-missing")))
            .filter_map(|i| self.sidebar_loc(i).map(|l| l.key()))
            .collect();
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
        let mut check = Vec::new();
        for uri in pins {
            let f = gio::File::for_uri(&uri);
            let key = format!("pin:{uri}");
            let title = file_title(&f);
            let loc = Loc::Dir(f);
            let missing = was_missing.contains(&loc.key());
            self.side_row("folder-symbolic", &title, loc, Some(uri.clone()), Some(key));
            if let Some(row) = self.sidebar.last_child().and_downcast::<gtk::ListBoxRow>() {
                if missing {
                    row.add_css_class("pin-missing");
                }
                check.push((uri, row.downgrade()));
            }
        }
        self.mark_missing_pins(check);

        self.side_header("Devices");
        self.side_row("drive-harddisk-symbolic", "Computer", Loc::Dir(gio::File::for_path("/")), None, None);
        let vols = self.volumes.volumes();
        let mounts = self.volumes.mounts();
        let vol_facts: Vec<VolFacts> = vols.iter().map(|v| VolFacts { name: v.name().to_string() }).collect();
        let mount_facts: Vec<MountFacts> = mounts
            .iter()
            .map(|m| MountFacts {
                // Match by object, or by root in case the monitor hands out different wrappers
                volume: m.volume().and_then(|mv| vols.iter().position(|v| *v == mv)).or_else(|| {
                    vols.iter().position(|v| v.get_mount().is_some_and(|vm| vm.root().equal(&m.root())))
                }),
                shadowed: m.is_shadowed(),
            })
            .collect();
        for row in device_rows(&vol_facts, &mount_facts) {
            let (mount, vol) = match row {
                DevRow::Volume(i) => (vols[i].get_mount(), Some(&vols[i])),
                DevRow::Mount(i) => (Some(mounts[i].clone()), None),
            };
            if let Some(m) = mount {
                self.side_row_gicon(&m.symbolic_icon(), &m.name(), SideTarget::Place(Loc::Dir(m.root())), None, None);
                self.add_eject_button(&m);
            } else if let Some(v) = vol {
                self.side_row_gicon(&v.symbolic_icon(), &v.name(), SideTarget::Volume(v.clone()), None, None);
            }
        }
        self.side_row("network-workgroup-symbolic", "Network", Loc::Dir(gio::File::for_uri("network:///")), None, None);

        let loc = self.tabs.borrow().get(self.current.get()).map(|t| t.loc.clone());
        if let Some(loc) = loc {
            self.highlight_sidebar(&loc);
        }
    }

    fn side_row(self: &Rc<Self>, icon: &str, name: &str, loc: Loc, pin: Option<String>, drag_key: Option<String>) {
        let gicon: gio::Icon = gio::ThemedIcon::new(icon).upcast();
        self.side_row_gicon(&gicon, name, SideTarget::Place(loc), pin, drag_key);
    }

    /// A sidebar row's menu (right-click or Menu key). `drag_key`: the row can be moved; `pin`: its pinned URI.
    fn sidebar_menu(idx: usize, drag_key: Option<&str>, pin: Option<&str>) -> Vec<Option<MItem>> {
        let mut items = vec![Some(MItem {
            label: "Open in New Tab".into(),
            action: "win.sidebar-tab",
            target: Some((idx as i32).to_variant()),
            accel: "",
        })];
        if let Some(k) = drag_key {
            items.push(None);
            let target = Some(k.to_variant());
            items.push(Some(MItem { label: "Move Up".into(), action: "win.side-up", target: target.clone(), accel: "" }));
            items.push(Some(MItem { label: "Move Down".into(), action: "win.side-down", target, accel: "" }));
            items.push(None);
        }
        if let Some(uri) = pin {
            items.push(Some(MItem {
                label: "Unpin".into(),
                action: "win.unpin",
                target: Some(uri.to_variant()),
                accel: "",
            }));
        }
        items
    }

    fn side_row_gicon(
        self: &Rc<Self>,
        icon: &gio::Icon,
        name: &str,
        target: SideTarget,
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
        match &target {
            SideTarget::Place(Loc::Dir(f)) => {
                row.set_tooltip_text(Some(&display_path(f)));
                crate::dnd::attach_folder_row(self, &row, f);
            }
            SideTarget::Volume(_) => row.set_tooltip_text(Some("Not mounted: click to mount")),
            SideTarget::Place(_) => {}
        }
        let idx = {
            let mut t = self.sidebar_targets.borrow_mut();
            t.push(Some(target));
            t.len() - 1
        };

        // Menu key / Shift+F10 on the focused row
        let k = gtk::EventControllerKey::new();
        let a = Rc::downgrade(self);
        let r = row.downgrade();
        let (menu_key, menu_pin) = (drag_key.clone(), pin.clone());
        k.connect_key_pressed(move |_, key, _, mods| {
            if !is_menu_key(key, mods) {
                return glib::Propagation::Proceed;
            }
            let (Some(a), Some(r)) = (a.upgrade(), r.upgrade()) else { return glib::Propagation::Proceed };
            let items = Self::sidebar_menu(idx, menu_key.as_deref(), menu_pin.as_deref());
            a.popup_menu(&r, 24.0, r.height() as f64 * 0.75, items);
            glib::Propagation::Stop
        });
        row.add_controller(k);

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
                a.popup_menu(&r, x, y, Self::sidebar_menu(idx, menu_key.as_deref(), pin.as_deref()));
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
        match self.sidebar_targets.borrow().get(idx) {
            Some(Some(SideTarget::Place(loc))) => Some(loc.clone()),
            _ => None,
        }
    }

    /// The unmounted volume at sidebar row `idx`, if that row is one.
    fn sidebar_volume(&self, idx: usize) -> Option<gio::Volume> {
        match self.sidebar_targets.borrow().get(idx) {
            Some(Some(SideTarget::Volume(v))) => Some(v.clone()),
            _ => None,
        }
    }

    fn open_sidebar_in_tab(self: &Rc<Self>, idx: usize) {
        if self.refuse_missing_pin(idx) {
            return;
        }
        if let Some(v) = self.sidebar_volume(idx) {
            self.mount_volume(v, true);
        } else if let Some(loc) = self.sidebar_loc(idx) {
            self.new_tab(loc);
        }
    }

    /// Mounts `vol` (asking for a password if it needs one), then opens it here or in a new tab.
    fn mount_volume(self: &Rc<Self>, vol: gio::Volume, new_tab: bool) {
        let name = vol.name().to_string();
        self.status.set_text("Mounting…");
        let op = gtk::MountOperation::new(Some(&self.window));
        let a = Rc::downgrade(self);
        let v = vol.clone();
        vol.mount(gio::MountMountFlags::NONE, Some(&op), None::<&gio::Cancellable>, move |r| {
            let Some(a) = a.upgrade() else { return };
            match r {
                Ok(()) => match v.get_mount() {
                    Some(m) => {
                        let loc = Loc::Dir(m.root());
                        if new_tab {
                            a.new_tab(loc);
                        } else {
                            a.navigate(loc);
                        }
                    }
                    None => a.status.set_text(&format!("Mounted “{name}”, but can't find where")),
                },
                // The user cancelled the password prompt
                Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => a.update_status(),
                Err(e) => a.status.set_text(&format!("Couldn't mount “{name}”: {}", e.message())),
            }
        });
    }

    /// Puts an eject (or unmount) button at the end of the sidebar row just added for `mount`.
    fn add_eject_button(self: &Rc<Self>, mount: &gio::Mount) {
        let Some(tip) = eject_tooltip(mount.can_eject(), mount.can_unmount()) else { return };
        let Some(bx) = self.sidebar.last_child().and_downcast::<gtk::ListBoxRow>().and_then(|r| r.child()) else {
            return;
        };
        let Ok(bx) = bx.downcast::<gtk::Box>() else { return };
        let btn = gtk::Button::from_icon_name("media-eject-symbolic");
        btn.add_css_class("flat");
        btn.add_css_class("sidebar-eject");
        btn.set_focus_on_click(false);
        btn.set_tooltip_text(Some(tip));
        btn.set_hexpand(true);
        btn.set_halign(gtk::Align::End);
        btn.set_valign(gtk::Align::Center);
        let a = Rc::downgrade(self);
        let m = mount.downgrade();
        // The button claims its click, so the row isn't activated as well
        btn.connect_clicked(move |_| {
            if let (Some(a), Some(m)) = (a.upgrade(), m.upgrade()) {
                a.eject_mount(m);
            }
        });
        bx.append(&btn);
    }

    /// Ejects or unmounts `mount`, first moving this window's tabs off it so we don't keep it busy.
    fn eject_mount(self: &Rc<Self>, mount: gio::Mount) {
        let name = mount.name().to_string();
        self.leave_mount(&mount.root());
        let eject = mount.can_eject();
        self.status.set_text(&format!("{} “{name}”…", if eject { "Ejecting" } else { "Unmounting" }));
        let op = gtk::MountOperation::new(Some(&self.window));
        // "Writing data, don't unplug" notes while the cache is flushed. Read as values: the message may be NULL.
        let a = Rc::downgrade(self);
        op.connect_local("show-unmount-progress", false, move |args| {
            let a = a.upgrade()?;
            let msg = args.get(1).and_then(|v| v.get::<Option<String>>().ok()).flatten()?;
            let time_left = args.get(2).and_then(|v| v.get::<i64>().ok()).unwrap_or(0);
            let bytes_left = args.get(3).and_then(|v| v.get::<i64>().ok()).unwrap_or(0);
            // Zero and zero means done; the finish callback reports that
            if time_left != 0 || bytes_left != 0 {
                a.status.set_text(&msg.lines().collect::<Vec<_>>().join(": "));
            }
            None
        });
        let a = Rc::downgrade(self);
        let done = move |r: Result<(), glib::Error>| {
            let Some(a) = a.upgrade() else { return };
            match r {
                // The user dismissed the "still in use" dialog
                Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => a.update_status(),
                Err(e) => a.status.set_text(&eject_message(&name, eject, Some(e.message()))),
                Ok(()) => a.status.set_text(&eject_message(&name, eject, None)),
            }
        };
        let flags = gio::MountUnmountFlags::NONE;
        if eject {
            mount.eject_with_operation(flags, Some(&op), None::<&gio::Cancellable>, done);
        } else {
            mount.unmount_with_operation(flags, Some(&op), None::<&gio::Cancellable>, done);
        }
    }

    /// Sends every tab of this window that shows a folder under `root` to Home.
    fn leave_mount(self: &Rc<Self>, root: &gio::File) {
        let current = self.current.get();
        let mut leave_current = false;
        for (i, t) in self.tabs.borrow_mut().iter_mut().enumerate() {
            if !loc_on_mount(&t.loc, root) {
                continue;
            }
            if i == current {
                leave_current = true;
                continue;
            }
            let old = std::mem::replace(&mut t.loc, Loc::Dir(home()));
            t.back.push((old, t.view.take()));
            t.fwd.clear();
            set_tab_title(&t.widget, &t.label, &t.loc.title());
            t.icon.set_icon_name(Some(t.loc.icon()));
            t.widget.set_tooltip_text(Some(&display_path(&home())));
        }
        if leave_current {
            self.navigate(Loc::Dir(home()));
        }
    }

    fn highlight_sidebar(&self, loc: &Loc) {
        let key = loc.key();
        let idx = self
            .sidebar_targets
            .borrow()
            .iter()
            .position(|t| matches!(t, Some(SideTarget::Place(l)) if l.key() == key));
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

    /// Keeps pins and stars on items RFiles renamed: `renames` are (old URI, new URI).
    pub(crate) fn note_renames(self: &Rc<Self>, renames: &[(String, String)]) {
        let changed = {
            let mut c = self.config.borrow_mut();
            let changed = c.apply_renames(renames);
            if changed {
                c.save();
            }
            changed
        };
        if changed {
            self.rebuild_sidebar();
            self.update_pin_button();
        }
    }

    /// Dims pinned folders that aren't there, once a check off the UI thread says so.
    fn mark_missing_pins(&self, rows: Vec<(String, glib::WeakRef<gtk::ListBoxRow>)>) {
        if rows.is_empty() {
            return;
        }
        let uris: Vec<String> = rows.iter().map(|(u, _)| u.clone()).collect();
        glib::spawn_future_local(async move {
            let Ok(missing) =
                gio::spawn_blocking(move || uris.iter().map(|u| crate::config::pin_is_missing(u)).collect::<Vec<_>>())
                    .await
            else {
                return;
            };
            // Rows of a sidebar rebuilt meanwhile are gone and don't upgrade
            for ((uri, row), missing) in rows.into_iter().zip(missing) {
                let Some(row) = row.upgrade() else { continue };
                let path = display_path(&gio::File::for_uri(&uri));
                if missing {
                    row.add_css_class("pin-missing");
                    row.set_tooltip_text(Some(&format!("Missing: {path}")));
                } else {
                    row.remove_css_class("pin-missing");
                    row.set_tooltip_text(Some(&path));
                }
            }
        });
    }

    /// Shows why a missing pinned folder can't be opened; returns whether row `idx` is one.
    fn refuse_missing_pin(self: &Rc<Self>, idx: usize) -> bool {
        let Some(row) = self.sidebar.row_at_index(idx as i32) else { return false };
        if !row.has_css_class("pin-missing") {
            return false;
        }
        let name = match self.sidebar_loc(idx) {
            Some(Loc::Dir(f)) => file_title(&f),
            _ => String::new(),
        };
        self.status.set_text(&format!(
            "“{name}” is missing: moved, renamed or deleted, or its drive is not connected. Right-click to unpin."
        ));
        // Look again, so a folder that is back opens on the next click
        let a = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(a) = a.upgrade() {
                a.rebuild_sidebar();
            }
        });
        true
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

        let hint = gtk::Label::new(Some("Tip: right-click any file or folder\n→ Change Icon… to give just that one\nits own icon."));
        hint.set_xalign(0.0);
        hint.add_css_class("dim");
        hint.add_css_class("small");

        bx.append(&h1);
        bx.append(&style);
        bx.append(&choose);
        bx.append(&h2);
        bx.append(&size);
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
        self.cancel_search();
        let cancel = Arc::new(AtomicBool::new(false));
        *self.search_cancel.borrow_mut() = Some(cancel.clone());
        self.status.set_text(&format!("Searching for “{query}” in {}…", path.display()));
        let hidden = self.show_hidden.get();
        let a = Rc::downgrade(self);
        let q = query.to_lowercase();
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || {
                search_walk(&path, &q, hidden, &cancel, SEARCH_MAX_RESULTS, SEARCH_MAX_VISITED)
            })
            .await;
            // The window may have closed while the walk ran
            let Some(a) = a.upgrade() else { return };
            if a.search_gen.get() != g {
                return;
            }
            a.search_cancel.borrow_mut().take();
            if let Ok((results, end)) = res {
                if end != SearchEnd::Cancelled {
                    a.navigate(Loc::Search { root, query, results: Rc::new(results), end });
                }
            }
        });
    }

    /// Stops the running deep search walk, if any. Returns whether one was running.
    fn cancel_search(&self) -> bool {
        match self.search_cancel.borrow_mut().take() {
            Some(c) => {
                c.store(true, AtomicOrdering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// The user stopped a deep search (Escape or an emptied search box).
    fn stop_search_by_user(&self) {
        if self.cancel_search() {
            // Its result is ignored even if the walk already finished
            self.search_gen.set(self.search_gen.get() + 1);
            self.status.set_text("Search stopped");
        }
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
            // Cleared after a complete move, so pasting again doesn't look for files that are gone
            fileops::start_transfer(&a, files, dest_path, cut, move |o, cancelled| {
                if cut && o.errors.is_empty() && !cancelled {
                    let _ = cb.set_content(None::<&gdk::ContentProvider>);
                }
            });
        });
    }

    // ================= other file operations =================
    fn prompt(self: &Rc<Self>, title: &str, initial: &str, select: usize, ok: &str, f: impl Fn(String) -> Result<(), String> + 'static) {
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

    fn in_recent(&self) -> bool {
        self.current_dir().is_some_and(|d| d.uri().starts_with("recent:"))
    }

    /// Moves the selection to the trash, or deletes it permanently after asking (always so in the trash).
    fn delete_selected(self: &Rc<Self>, permanent: bool) {
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
    fn forget_recent_selected(self: &Rc<Self>) {
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
    fn restore_selected(self: &Rc<Self>) {
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
        self.status.set_text(&format!("Restoring {} item(s)…", items.len()));
        let a = self.clone();
        glib::spawn_future_local(async move {
            match gio::spawn_blocking(move || crate::trash::restore(items)).await {
                Ok(o) => a.status.set_text(&crate::trash::restore_message(&o)),
                Err(_) => a.status.set_text("Restore failed"),
            }
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

    /// Forgets starred files that are gone for good, so the saved list doesn't keep dead
    /// entries forever. The view already leaves them out; this only cleans the config.
    fn drop_dead_stars(self: &Rc<Self>) {
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
            let mut c = a.config.borrow_mut();
            c.starred.retain(|u| !dead.contains(u));
            c.save();
        });
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
            self.note_widths(&mut c);
        }
        c.list_view = self.view_btn.is_active();
        c.show_sidebar = self.sidebar_btn.is_active();
        c.show_preview = self.preview_btn.is_active();
        c.show_hidden = self.show_hidden.get();
        c.save();
    }

    /// Puts the pane widths into `c`. A hidden pane keeps the width it had.
    fn note_widths(&self, c: &mut Config) {
        let sidebar = self.side_scroll.is_visible().then(|| self.outer.position());
        let iw = self.inner.width();
        let inner = (self.preview.root.is_visible() && iw > 0).then(|| (iw, self.inner.position()));
        (c.sidebar_pos, c.preview_width, c.files_pos) =
            widths_to_save((c.sidebar_pos, c.preview_width, c.files_pos), sidebar, inner);
    }

    /// Moves keyboard focus to the file view if it is inside `pane`, which is about to be hidden.
    fn focus_files_if_in(&self, pane: &gtk::Widget) {
        let inside = GtkWindowExt::focus(&self.window).is_some_and(|f| &f == pane || f.is_ancestor(pane));
        if inside {
            if self.view_btn.is_active() {
                self.list.grab_focus();
            } else {
                self.grid.grab_focus();
            }
        }
    }

    /// Saves the shown/hidden state of the panes, once the window is up.
    fn save_pane_state(&self) {
        let mut c = self.config.borrow_mut();
        c.show_sidebar = self.sidebar_btn.is_active();
        c.show_preview = self.preview_btn.is_active();
        if self.window.is_mapped() {
            c.save();
        }
    }

    /// Shows or hides the sidebar (F9). GtkPaned gives the rest all the room and keeps its
    /// position (the sidebar width) while the sidebar is hidden.
    fn set_sidebar_shown(&self, show: bool) {
        if self.side_scroll.is_visible() == show {
            return;
        }
        if !show {
            if self.layout_ready.get() {
                self.note_widths(&mut self.config.borrow_mut());
            }
            self.focus_files_if_in(self.side_scroll.upcast_ref());
        }
        self.side_scroll.set_visible(show);
        self.save_pane_state();
    }

    /// Shows or hides the preview pane (F4). While hidden nothing is previewed and a playing
    /// video stops; showing it again previews the selection at the width it had.
    fn set_preview_shown(self: &Rc<Self>, show: bool) {
        let root = &self.preview.root;
        if root.is_visible() == show {
            return;
        }
        if show {
            root.set_visible(true);
            let w = self.inner.width();
            // Not laid out yet: `restore_layout` places the divider
            if w > 0 {
                let pw = self.config.borrow().preview_width;
                self.inner.set_position(preview_show_position(self.preview_hidden_at.get(), w, pw));
            }
            self.on_selection_changed();
        } else {
            // Note its width now: a later save can't see it any more
            if self.layout_ready.get() {
                self.note_widths(&mut self.config.borrow_mut());
            }
            self.preview_hidden_at.set(self.inner.width());
            self.focus_files_if_in(root.upcast_ref());
            root.set_visible(false);
            *self.previewed.borrow_mut() = None;
            self.preview.clear();
        }
        self.save_pane_state();
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
        add("toggle-sidebar", Box::new(|a| a.sidebar_btn.set_active(!a.sidebar_btn.is_active())));
        add("toggle-preview", Box::new(|a| a.preview_btn.set_active(!a.preview_btn.is_active())));
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
        add("open", Box::new(|a| match a.selected_infos().len() {
            0 => {}
            1 => {
                if let Some(i) = a.selected_info() {
                    a.open_info(&i);
                }
            }
            _ => a.open_selected(),
        }));
        let gw = gapp.downgrade();
        add("new-window", Box::new(move |a| {
            if let Some(g) = gw.upgrade() {
                build(&g, a.current_dir().unwrap_or_else(home));
            }
        }));
        let gw = gapp.downgrade();
        add("quit", Box::new(move |_| {
            let Some(g) = gw.upgrade() else { return };
            // close(), not quit() alone: only close-request saves each window's layout
            for w in g.windows() {
                w.close();
            }
            g.quit();
        }));
        add("shortcuts", Box::new(|a| show_shortcuts(&a.window)));
        add("about", Box::new(|a| show_about(&a.window)));
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
        add("trash", Box::new(|a| a.delete_selected(false)));
        add("delete-permanently", Box::new(|a| a.delete_selected(true)));
        add("forget-recent", Box::new(|a| a.forget_recent_selected()));
        add("empty-trash", Box::new(crate::trash::empty_trash));
        add("restore", Box::new(|a| a.restore_selected()));
        add("rename", Box::new(|a| {
            if a.in_trash() {
                return;
            }
            let mut infos = a.selected_infos();
            let w = Rc::downgrade(a);
            if infos.len() > 1 {
                crate::batch_rename::open(&a.window, &infos, move |renames| {
                    if let Some(a) = w.upgrade() {
                        a.note_renames(renames);
                    }
                });
                return;
            }
            let Some(info) = infos.pop() else { return };
            let file = item_file(&info);
            let name = info.display_name().to_string();
            let sel = crate::util::select_len(&name, is_dir_like(&info));
            a.prompt("Rename", &name, sel, "Rename", move |new| {
                let renamed = file
                    .set_display_name(&new, None::<&gio::Cancellable>)
                    .map_err(|e| e.message().to_string())?;
                if let Some(a) = w.upgrade() {
                    a.note_renames(&[(file.uri().to_string(), renamed.uri().to_string())]);
                }
                Ok(())
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
            a.prompt("New Folder", "New Folder", "New Folder".len(), "Create", move |name| {
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
        let sort_by = gio::SimpleAction::new("sort-by", Some(glib::VariantTy::STRING));
        let a = Rc::downgrade(self);
        sort_by.connect_activate(move |_, v| {
            let Some(a) = a.upgrade() else { return };
            if let Some(k) = v.and_then(|v| v.get::<String>()).and_then(|k| SortKey::parse(&k)) {
                a.set_sort(k, a.sort.get().1);
            }
        });
        self.window.add_action(&sort_by);
        add("sort-reversed", Box::new(|a| {
            let (key, desc) = a.sort.get();
            a.set_sort(key, !desc);
        }));

        let accels: &[(&str, &[&str])] = &[
            ("win.new-tab", &["<Control>t"]),
            ("win.new-window", &["<Control>n"]),
            ("win.quit", &["<Control>q"]),
            ("win.shortcuts", &["<Control>question", "<Control><Shift>slash", "F1"]),
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
            ("win.toggle-sidebar", &["F9"]),
            ("win.toggle-preview", &["F4"]),
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
        });
        let a = Rc::downgrade(self);
        self.view_btn.connect_toggled(move |b| {
            let Some(a) = a.upgrade() else { return };
            a.view_stack
                .set_visible_child_name(if b.is_active() { "list" } else { "grid" });
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

/// Most items "Open" handles at once, so a stray Ctrl+A then Enter can't launch hundreds of apps.
const MAX_OPEN: usize = 20;

/// Which of several selected items go where, as indexes into the selection.
#[derive(Debug, PartialEq, Eq)]
struct OpenPlan {
    /// Default app
    files: Vec<usize>,
    /// Opens in the current tab
    first_dir: Option<usize>,
    /// Open in new tabs
    other_dirs: Vec<usize>,
    /// Selected items beyond the cap, left alone
    skipped: usize,
}

fn plan_open(is_dir: &[bool]) -> OpenPlan {
    let skipped = is_dir.len().saturating_sub(MAX_OPEN);
    let mut plan = OpenPlan { files: vec![], first_dir: None, other_dirs: vec![], skipped };
    for (i, &d) in is_dir.iter().take(MAX_OPEN).enumerate() {
        match (d, plan.first_dir) {
            (false, _) => plan.files.push(i),
            (true, None) => plan.first_dir = Some(i),
            (true, Some(_)) => plan.other_dirs.push(i),
        }
    }
    plan
}

#[cfg(test)]
mod open_plan_tests {
    use super::{plan_open, MAX_OPEN};

    #[test]
    fn files_go_to_apps_and_folders_to_tabs() {
        let p = plan_open(&[false, true, false, true, true]);
        assert_eq!(p.files, vec![0, 2]);
        assert_eq!(p.first_dir, Some(1));
        assert_eq!(p.other_dirs, vec![3, 4]);
        assert_eq!(p.skipped, 0);
    }

    #[test]
    fn only_files_leave_the_tab_alone() {
        let p = plan_open(&[false, false]);
        assert_eq!((p.files, p.first_dir, p.other_dirs), (vec![0, 1], None, vec![]));
    }

    #[test]
    fn capped_at_twenty() {
        let p = plan_open(&vec![false; MAX_OPEN + 5]);
        assert_eq!(p.files.len(), MAX_OPEN);
        assert_eq!(p.skipped, 5);
        assert_eq!(plan_open(&vec![true; MAX_OPEN]).skipped, 0);
        assert_eq!(plan_open(&[]).skipped, 0);
    }
}

/// What the path bar shows for a location. Used when loading it and when the typed text is abandoned.
fn loc_path_text(loc: &Loc) -> String {
    match loc {
        Loc::Dir(f) => display_path(f),
        Loc::Starred => "Starred".into(),
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
    fn setup_path_bar(self: &Rc<Self>) {
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

/// Every shortcut, shown in the shortcuts window. Keep in step with the README table.
const SHORTCUTS: &[(&str, &str)] = &[
    ("New tab / close tab", "Ctrl+T / Ctrl+W"),
    ("New window", "Ctrl+N"),
    ("Quit", "Ctrl+Q"),
    ("Switch tabs", "Ctrl+PageDown / Ctrl+PageUp"),
    ("Tab keys (tab focused)", "Left / Right / Home / End move, Enter or Space opens, Delete closes"),
    ("Preview / open", "Click / double-click or Enter"),
    ("Copy / cut / paste", "Ctrl+C / Ctrl+X / Ctrl+V"),
    ("Select several", "Ctrl+click, Shift+click, drag a box, Ctrl+A"),
    ("Rename / move to trash", "F2 (several selected = batch rename) / Delete"),
    ("Delete permanently", "Shift+Delete (asks first)"),
    ("Properties", "Alt+Enter or Ctrl+I"),
    ("Drag to copy / move", "Hold Ctrl to copy, Shift to move"),
    ("New folder", "Ctrl+Shift+N"),
    ("Pin / unpin this folder", "Ctrl+D"),
    ("Open in terminal", "Ctrl+Alt+T"),
    ("Search / path bar", "Ctrl+F / Ctrl+L (Tab completes folders, Esc cancels)"),
    ("Back / forward / up", "Alt+Left / Alt+Right / Alt+Up"),
    ("Hidden files", "Ctrl+H"),
    ("Grid / list view", "Ctrl+1 / Ctrl+2"),
    ("Right-click menu from the keyboard", "Menu key or Shift+F10"),
    ("Show / hide sidebar / preview pane", "F9 / F4"),
    ("This list", "Ctrl+? or F1"),
];

fn show_shortcuts(parent: &gtk::ApplicationWindow) {
    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(24)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(20)
        .margin_end(20)
        .build();
    for (row, (what, keys)) in SHORTCUTS.iter().enumerate() {
        let l = gtk::Label::new(Some(what));
        l.set_xalign(0.0);
        let k = gtk::Label::new(Some(keys));
        k.set_xalign(0.0);
        k.add_css_class("dim");
        grid.attach(&l, 0, row as i32, 1, 1);
        grid.attach(&k, 1, row as i32, 1, 1);
    }
    let scroll = gtk::ScrolledWindow::builder().child(&grid).build();
    let win = gtk::Window::builder()
        .title("Keyboard shortcuts")
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .default_width(560)
        .default_height(520)
        .child(&scroll)
        .build();
    // Esc closes it
    let k = gtk::EventControllerKey::new();
    let ww = win.downgrade();
    k.connect_key_pressed(move |_, key, _, _| {
        if key != gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        if let Some(w) = ww.upgrade() {
            w.close();
        }
        glib::Propagation::Stop
    });
    win.add_controller(k);
    win.present();
}

fn show_about(parent: &gtk::ApplicationWindow) {
    gtk::AboutDialog::builder()
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .program_name("RFiles")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("A black, tabbed file explorer with live preview.")
        .website("https://github.com/S0CloseYetS0Far/R-files")
        .build()
        .present();
}

/// The (sidebar, preview, files) widths to save. `sidebar` is the outer divider position and
/// `inner` the inner paned's (width, divider position), each None while that pane is hidden:
/// its paned position then says nothing about it, so the width saved for it is kept.
fn widths_to_save(saved: (i32, i32, i32), sidebar: Option<i32>, inner: Option<(i32, i32)>) -> (i32, i32, i32) {
    let (mut side, mut preview, mut files) = saved;
    if let Some(pos) = sidebar {
        side = pos;
    }
    if let Some((width, pos)) = inner {
        preview = (width - pos).max(150);
        files = pos;
    }
    (side, preview, files)
}

#[cfg(test)]
mod pane_width_tests {
    use super::*;

    #[test]
    fn shown_panes_are_measured() {
        assert_eq!(widths_to_save((220, 420, 700), Some(250), Some((1000, 600))), (250, 400, 600));
        // A divider pushed to the end still saves a usable preview width
        assert_eq!(widths_to_save((220, 420, 700), Some(250), Some((1000, 990))), (250, 150, 990));
    }

    #[test]
    fn hidden_panes_keep_their_width() {
        assert_eq!(widths_to_save((220, 420, 700), None, Some((1200, 800))), (220, 400, 800));
        assert_eq!(widths_to_save((220, 420, 700), Some(180), None), (180, 420, 700));
        assert_eq!(widths_to_save((220, 420, 700), None, None), (220, 420, 700));
    }

    #[test]
    fn show_position_keeps_the_preview_width() {
        // GtkPaned later adds (width now - width when it last laid out both children);
        // the preview must come back `preview` wide either way.
        let pos = preview_show_position(1000, 1600, 420);
        assert_eq!(pos + 1600 - 1000, 1600 - 420);
        assert_eq!(preview_show_position(1600, 1000, 420) + 1000 - 1600, 1000 - 420);
        // Never laid out with the preview (started hidden): the position is used as is
        assert_eq!(preview_show_position(0, 1600, 420), 1180);
        // Never negative, which would unset the position
        assert!(preview_show_position(300, 1600, 420) > 0);
    }
}

/// Divider position that brings the preview pane back `preview` wide in an inner paned now
/// `width` wide. `hidden_at` is the paned's width when the preview was hidden, 0 if never:
/// GtkPaned shifts a set position by how much its width changed since it last laid out both
/// children (start side resizes, end doesn't), and that was at `hidden_at`.
fn preview_show_position(hidden_at: i32, width: i32, preview: i32) -> i32 {
    let base = if hidden_at > 0 { hidden_at } else { width };
    (base - preview).max(1)
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

/// Recursive name search, run on a worker thread. Stops once `cancel` is set or a limit is reached, and
/// says which.
fn search_walk(
    root: &Path,
    q: &str,
    hidden: bool,
    cancel: &AtomicBool,
    max_results: usize,
    max_visited: usize,
) -> (Vec<PathBuf>, SearchEnd) {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    let skip: &[&str] = &["/proc", "/sys", "/dev", "/run", "/snap"];
    while let Some(dir) = stack.pop() {
        if cancel.load(AtomicOrdering::Relaxed) {
            return (out, SearchEnd::Cancelled);
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            // Checked per entry too: one folder can hold a huge number of files
            if cancel.load(AtomicOrdering::Relaxed) {
                return (out, SearchEnd::Cancelled);
            }
            visited += 1;
            if visited > max_visited {
                return (out, SearchEnd::VisitedLimit);
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !hidden && name.starts_with('.') {
                continue;
            }
            let path = e.path();
            if name.to_lowercase().contains(q) {
                // Only a match past the limit means results were left out
                if out.len() >= max_results {
                    return (out, SearchEnd::ResultLimit);
                }
                out.push(path.clone());
            }
            if let Ok(ft) = e.file_type() {
                if ft.is_dir() && !skip.iter().any(|s| path == Path::new(s)) {
                    stack.push(path);
                }
            }
        }
    }
    (out, SearchEnd::Done)
}

#[cfg(test)]
mod tests {
    use super::{empty_message, folder_name_problem, folder_style_index, search_walk, Place, SearchEnd};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    /// root/{Alpha.txt, beta.txt, .alpha-hidden, sub/{alpha2, deep/ALPHA3}, .hid/alpha4}
    fn search_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rfiles-search-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::create_dir_all(root.join(".hid")).unwrap();
        for f in ["Alpha.txt", "beta.txt", ".alpha-hidden", "sub/alpha2", "sub/deep/ALPHA3", ".hid/alpha4"] {
            std::fs::write(root.join(f), b"").unwrap();
        }
        root
    }

    fn names(found: &[PathBuf], root: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> =
            found.iter().map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn search_walk_matches_and_hidden() {
        let root = search_tree("match");
        let no = AtomicBool::new(false);
        let (found, end) = search_walk(&root, "alpha", false, &no, 100, 100);
        assert_eq!(end, SearchEnd::Done);
        assert_eq!(names(&found, &root), ["Alpha.txt", "sub/alpha2", "sub/deep/ALPHA3"]);
        let (found, end) = search_walk(&root, "alpha", true, &no, 100, 100);
        assert_eq!(end, SearchEnd::Done);
        assert_eq!(
            names(&found, &root),
            [".alpha-hidden", ".hid/alpha4", "Alpha.txt", "sub/alpha2", "sub/deep/ALPHA3"]
        );
        // Exactly as many results as the limit is not a cut-off search
        let (found, end) = search_walk(&root, "alpha", false, &no, 3, 100);
        assert_eq!((found.len(), end), (3, SearchEnd::Done));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_walk_limits() {
        let root = search_tree("limits");
        let no = AtomicBool::new(false);
        let (found, end) = search_walk(&root, "alpha", true, &no, 2, 100);
        assert_eq!((found.len(), end), (2, SearchEnd::ResultLimit));
        assert_eq!(end.note().as_deref(), Some("first 2,000 results"));
        // Root holds 5 entries; a limit of 3 visited stops inside it
        let (found, end) = search_walk(&root, "zzz", true, &no, 100, 3);
        assert_eq!((found.len(), end), (0, SearchEnd::VisitedLimit));
        assert_eq!(end.note().as_deref(), Some("stopped after 400,000 files"));
        assert_eq!(SearchEnd::Done.note(), None);
        assert_eq!(SearchEnd::Cancelled.note(), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_walk_cancelled() {
        let root = search_tree("cancel");
        let (found, end) = search_walk(&root, "alpha", true, &AtomicBool::new(true), 100, 100);
        assert_eq!((found.len(), end), (0, SearchEnd::Cancelled));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn folder_style_indexes() {
        assert_eq!(folder_style_index("blue"), 0);
        assert_eq!(folder_style_index(""), 0);
        assert_eq!(folder_style_index("system"), 1);
        assert_eq!(folder_style_index("custom:/a/b.png"), 2);
    }

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
