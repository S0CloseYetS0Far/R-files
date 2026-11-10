//! Drag and drop of files (GNOME Files style).
//!
//! * Items in the grid / list views are drag sources. The content is a
//!   `gdk::FileList`, which GTK also offers as `text/uri-list` and the portal
//!   formats, so other apps (GNOME Files, the desktop, browsers) accept it.
//! * Drop targets: folder items, the empty area of the current folder, sidebar
//!   folders (Trash = move to trash) and the "Pinned" header (pins folders).
//! * Same drive → move, different drive → copy; Ctrl forces copy, Shift move.
//!
//! The copy / move itself runs on a worker thread via `fileops::transfer`.
//! Handlers hold only weak references to the window and list items, so the
//! recycled item widgets never keep anything alive.

use crate::app::{App, Loc};
use crate::fileops;
use crate::util::{is_dir_like, is_trash, item_file};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// CSS class put on the widget under the pointer while it would accept a drop.
const HOVER_CLASS: &str = "dnd-hover";

/// Where dropped files go.
#[derive(Clone, Debug, PartialEq)]
enum Dest {
    Dir(PathBuf),
    Trash,
    /// The "Pinned" sidebar header: pin dropped folders.
    Pin,
}

/// What a drop onto a folder could do, worked out once per hovered folder.
struct Verdict {
    same_device: bool,
    can_copy: bool,
    can_move: bool,
}

struct Hover {
    /// Identifies the drag (the `gdk::Drop` object), so a later drag never
    /// reuses an old verdict.
    drop_id: usize,
    dest: PathBuf,
    verdict: Verdict,
}

thread_local! {
    static HOVER: RefCell<Option<Hover>> = const { RefCell::new(None) };
    /// The single widget currently showing the drop highlight.
    static LIT: RefCell<Option<gtk::Widget>> = const { RefCell::new(None) };
}

// ================= pure decisions (unit tested) =================

/// What we would like to do: Ctrl copies, Shift moves, otherwise move on the
/// same drive and copy across drives. Sent to the drag source / compositor as
/// the preferred action while hovering.
fn preference(ctrl: bool, shift: bool, same_device: bool) -> gdk::DragAction {
    match (ctrl, shift) {
        (true, false) => gdk::DragAction::COPY,
        (false, true) => gdk::DragAction::MOVE,
        _ if same_device => gdk::DragAction::MOVE,
        _ => gdk::DragAction::COPY,
    }
}

/// The action actually performed on drop. When the drag has already been
/// narrowed to one action (Wayland: the compositor's pick from our preference
/// and the held modifiers; X11: Ctrl / Shift held), that one wins; otherwise
/// (X11 with no modifier) our preference, if the source allows it.
fn final_action(offered: gdk::DragAction, pref: gdk::DragAction) -> gdk::DragAction {
    let offered = offered & (gdk::DragAction::COPY | gdk::DragAction::MOVE);
    if offered.contains(pref) && offered != pref {
        pref
    } else {
        offered // empty, or the single allowed action
    }
}

/// Whether `src` would actually be copied / moved into `dest`: never into
/// itself or one of its subfolders, and moving into its own folder is a no-op.
fn keeps(src: &Path, dest: &Path, cut: bool) -> bool {
    !dest.starts_with(src) && !(cut && src.parent() == Some(dest))
}

/// True when every source sits on the same filesystem as `dest`. Checks the
/// sources' folders (deduplicated), usually a single `stat`.
fn same_device(sources: &[PathBuf], dest: &Path) -> bool {
    let Ok(d) = std::fs::metadata(dest).map(|m| m.dev()) else { return false };
    let mut seen: Vec<&Path> = Vec::new();
    for parent in sources.iter().filter_map(|s| s.parent()) {
        if seen.contains(&parent) {
            continue;
        }
        seen.push(parent);
        match std::fs::metadata(parent) {
            Ok(m) if m.dev() == d => {}
            _ => return false,
        }
    }
    true
}

fn verdict(sources: &[PathBuf], dest: &Path) -> Verdict {
    Verdict {
        same_device: same_device(sources, dest),
        can_copy: sources.iter().any(|s| keeps(s, dest, false)),
        can_move: sources.iter().any(|s| keeps(s, dest, true)),
    }
}

/// The drop destination for a folder (sidebar row or folder item).
fn dest_for_folder(f: &gio::File) -> Option<Dest> {
    if is_trash(f) {
        // Only the trash itself, not folders inside it
        return (f.uri().trim_end_matches('/') == "trash:").then_some(Dest::Trash);
    }
    f.path().map(Dest::Dir)
}

// ================= highlight =================

fn set_lit(w: Option<&gtk::Widget>) {
    LIT.with(|l| {
        let mut l = l.borrow_mut();
        if l.as_ref() == w {
            return;
        }
        if let Some(old) = l.take() {
            old.remove_css_class(HOVER_CLASS);
        }
        if let Some(w) = w {
            w.add_css_class(HOVER_CLASS);
            *l = Some(w.clone());
        }
    });
}

fn unlight(w: &gtk::Widget) {
    let mine = LIT.with(|l| l.borrow().as_ref() == Some(w));
    if mine {
        set_lit(None);
    }
}

/// The widget to highlight for an item: its grid tile / list row, not just the
/// cell the controller sits on.
fn item_frame(w: gtk::Widget) -> gtk::Widget {
    let mut cur = Some(w.clone());
    for _ in 0..3 {
        let Some(c) = cur else { break };
        let name = c.css_name();
        if name == "child" || name == "row" {
            return c;
        }
        cur = c.parent();
    }
    w
}

// ================= drop targets =================

fn modifiers(t: &gtk::DropTarget) -> (bool, bool) {
    let state = t
        .widget()
        .and_then(|w| w.display().default_seat())
        .and_then(|s| s.keyboard())
        .map(|k| k.modifier_state())
        .unwrap_or_else(|| t.current_event_state());
    (
        state.contains(gdk::ModifierType::CONTROL_MASK),
        state.contains(gdk::ModifierType::SHIFT_MASK),
    )
}

fn dropped_paths(t: &gtk::DropTarget) -> Option<Vec<PathBuf>> {
    let list = t.value_as::<gdk::FileList>()?;
    Some(list.files().iter().filter_map(|f| f.path()).collect())
}

/// While hovering (`dropping == false`): the preferred action for `dest`.
/// On drop: the action to perform. Empty means "can't drop here".
fn action_for(t: &gtk::DropTarget, dest: &Dest, dropping: bool) -> gdk::DragAction {
    let Some(drop) = t.current_drop() else { return gdk::DragAction::empty() };
    let offered = drop.actions();
    if (offered & (gdk::DragAction::COPY | gdk::DragAction::MOVE)).is_empty() {
        return gdk::DragAction::empty();
    }
    let dir = match dest {
        // Pinning and trashing don't depend on copy vs move
        Dest::Pin if dropping => return offered,
        Dest::Trash if dropping => return offered,
        Dest::Pin => return gdk::DragAction::COPY,
        Dest::Trash => return gdk::DragAction::MOVE,
        Dest::Dir(d) => d,
    };
    let (ctrl, shift) = modifiers(t);
    let drop_id = drop.as_ptr() as usize;
    HOVER.with(|h| {
        let mut h = h.borrow_mut();
        let fresh = matches!(&*h, Some(x) if x.drop_id == drop_id && &x.dest == dir);
        if !fresh {
            // The dropped files are preloaded; until they arrive, guess.
            let Some(paths) = dropped_paths(t) else {
                return preference(ctrl, shift, false);
            };
            *h = Some(Hover { drop_id, dest: dir.clone(), verdict: verdict(&paths, dir) });
        }
        let v = &h.as_ref().unwrap().verdict;
        let mut a = preference(ctrl, shift, v.same_device);
        if dropping {
            a = final_action(offered, a);
        }
        let possible = if a == gdk::DragAction::MOVE { v.can_move } else { v.can_copy };
        if possible && !a.is_empty() {
            a
        } else {
            gdk::DragAction::empty()
        }
    })
}

/// True if the drag carries files (ours or another app's).
fn is_file_drag(d: &gdk::Drop) -> bool {
    let f = d.formats();
    f.contains_type(gdk::FileList::static_type()) || f.contain_mime_type("text/uri-list")
}

/// Adds a file drop target to `widget`. `dest` is asked on every event, so it
/// follows recycled list items. `frame` maps the widget to what gets lit.
fn add_target(
    widget: &impl IsA<gtk::Widget>,
    dest: impl Fn() -> Option<Dest> + 'static,
    frame: fn(gtk::Widget) -> gtk::Widget,
    app: Weak<App>,
) {
    let t = gtk::DropTarget::new(
        gdk::FileList::static_type(),
        gdk::DragAction::COPY | gdk::DragAction::MOVE | gdk::DragAction::LINK,
    );
    t.set_preload(true);
    let dest = Rc::new(dest);

    let d = dest.clone();
    // Not a target (e.g. a plain file): let the drop fall through to the view
    t.connect_accept(move |t, drop| {
        is_file_drag(drop) && !(drop.actions() & t.actions()).is_empty() && d().is_some()
    });

    let d = dest.clone();
    let motion = move |t: &gtk::DropTarget, _: f64, _: f64| {
        let a = d().map(|d| action_for(t, &d, false)).unwrap_or_else(gdk::DragAction::empty);
        let w = t.widget().map(frame);
        set_lit(if a.is_empty() { None } else { w.as_ref() });
        a
    };
    t.connect_enter(motion.clone());
    t.connect_motion(motion);

    t.connect_leave(move |t| {
        if let Some(w) = t.widget().map(frame) {
            unlight(&w);
        }
        HOVER.with(|h| h.borrow_mut().take());
    });

    t.connect_drop(move |t, value, _, _| {
        set_lit(None);
        let action = dest().map(|d| (action_for(t, &d, true), d));
        HOVER.with(|h| h.borrow_mut().take());
        let (Some(app), Some((action, dest))) = (app.upgrade(), action) else { return false };
        if action.is_empty() {
            return false;
        }
        let Ok(list) = value.get::<gdk::FileList>() else { return false };
        let files = list.files();
        match dest {
            Dest::Pin => pin_folders(&app, files),
            Dest::Trash => trash_files(&app, local_paths(&app, &files)),
            Dest::Dir(dir) => {
                let cut = action == gdk::DragAction::MOVE;
                let srcs: Vec<PathBuf> =
                    local_paths(&app, &files).into_iter().filter(|s| keeps(s, &dir, cut)).collect();
                if srcs.is_empty() {
                    return false;
                }
                run_transfer(&app, srcs, dir, cut);
            }
        }
        true
    });
    widget.add_controller(t);
}

fn local_paths(app: &App, files: &[gio::File]) -> Vec<PathBuf> {
    let paths: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
    if paths.len() < files.len() {
        app.status.set_text("Only local files can be dropped here");
    }
    paths
}

// ================= public hooks (called from app.rs) =================

/// Makes a file view item (grid tile or list cell) draggable, and a drop
/// target when it is a folder.
pub(crate) fn attach_item(app: &Rc<App>, widget: &gtk::Box, li: &gtk::ListItem) {
    let src = gtk::DragSource::new();
    src.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
    let (wa, wl) = (Rc::downgrade(app), li.downgrade());
    src.connect_prepare(move |_, _, _| {
        let (app, li) = (wa.upgrade()?, wl.upgrade()?);
        li.item()?;
        if app.in_trash() {
            return None;
        }
        // Dragging an unselected item drags just that item
        let pos = li.position();
        if !app.selection.is_selected(pos) {
            app.selection.select_item(pos, true);
        }
        let files: Vec<gio::File> = app.selected_infos().iter().map(item_file).collect();
        if files.is_empty() {
            return None;
        }
        Some(gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value()))
    });
    let wl = li.downgrade();
    src.connect_drag_begin(move |s, _| {
        let Some(w) = s.widget() else { return };
        if w.has_css_class("tile") {
            // Grid: the tile itself, like GNOME Files
            let (x, y) = (w.width() / 2, w.height() / 3);
            s.set_icon(Some(&gtk::WidgetPaintable::new(Some(&w))), x, y);
        } else if let Some(icon) = wl.upgrade().and_then(|li| li.item()).and_downcast::<gio::FileInfo>().and_then(|i| i.icon()) {
            let theme = gtk::IconTheme::for_display(&w.display());
            let p = theme.lookup_by_gicon(&icon, 32, w.scale_factor(), gtk::TextDirection::None, gtk::IconLookupFlags::empty());
            s.set_icon(Some(&p), 16, 16);
        }
    });
    widget.add_controller(src);

    let wl = li.downgrade();
    let dest = move || {
        let info = wl.upgrade()?.item().and_downcast::<gio::FileInfo>()?;
        if !is_dir_like(&info) {
            return None;
        }
        dest_for_folder(&item_file(&info))
    };
    add_target(widget, dest, item_frame, Rc::downgrade(app));
}

/// Drops on the empty area of a file view go into the current folder.
pub(crate) fn attach_view(app: &Rc<App>, scroll: &gtk::ScrolledWindow) {
    let wa = Rc::downgrade(app);
    let dest = move || match wa.upgrade()?.current_loc() {
        Loc::Dir(f) => dest_for_folder(&f),
        _ => None,
    };
    add_target(scroll, dest, |w| w, Rc::downgrade(app));
}

/// Sidebar place / pinned folder / device: drops go into that folder.
pub(crate) fn attach_folder_row(app: &Rc<App>, row: &gtk::ListBoxRow, folder: &gio::File) {
    let Some(d) = dest_for_folder(folder) else { return };
    add_target(row, move || Some(d.clone()), |w| w, Rc::downgrade(app));
}

/// The "Pinned" header (the last row added to `sidebar`): dropped folders
/// get pinned.
pub(crate) fn attach_pin_target(app: &Rc<App>, sidebar: &gtk::ListBox) {
    if let Some(row) = sidebar.last_child() {
        add_target(&row, || Some(Dest::Pin), |w| w, Rc::downgrade(app));
    }
}

/// The sidebar's reorder target takes strings; file drags can be offered as
/// text too, so keep them away from it.
pub(crate) fn ignore_file_drags(reorder: &gtk::DropTarget) {
    reorder.connect_accept(|t, d| {
        !is_file_drag(d)
            && !(d.actions() & t.actions()).is_empty()
            && d.formats().contains_type(String::static_type())
    });
}

// ================= doing the work =================

fn run_transfer(app: &Rc<App>, srcs: Vec<PathBuf>, dest: PathBuf, cut: bool) {
    let n = srcs.len();
    let verb = if cut { "Moving" } else { "Copying" };
    app.status.set_text(&format!("{verb} {n} item(s)…"));
    let progress = Arc::new(AtomicU64::new(0));
    let p = progress.clone();
    let status = app.status.clone();
    let timer = glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
        status.set_text(&format!("{verb} {n} item(s)…  {}", glib::format_size(p.load(Ordering::Relaxed))));
        glib::ControlFlow::Continue
    });
    let app = app.clone();
    glib::spawn_future_local(async move {
        let out = gio::spawn_blocking(move || fileops::transfer(srcs, dest, cut, progress)).await;
        timer.remove();
        let verb = if cut { "Moved" } else { "Copied" };
        app.status.set_text(&match out {
            Ok(o) if o.errors.is_empty() => format!("{verb} {} item(s)", o.done),
            Ok(o) => format!("{verb} {}, {} failed — {}", o.done, o.errors.len(), o.errors[0]),
            Err(_) => "Drop failed".into(),
        });
        // Folder views update themselves; Starred / search results don't
        if !matches!(app.current_loc(), Loc::Dir(_)) {
            app.reload();
        }
    });
}

fn trash_files(app: &Rc<App>, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let app = app.clone();
    glib::spawn_future_local(async move {
        let res = gio::spawn_blocking(move || {
            let mut failed = None;
            let n = paths.len();
            for p in paths {
                if let Err(e) = gio::File::for_path(&p).trash(None::<&gio::Cancellable>) {
                    failed = Some(e.message().to_string());
                }
            }
            (n, failed)
        })
        .await;
        app.status.set_text(&match res {
            Ok((n, None)) => format!("Moved {n} item(s) to the trash"),
            Ok((_, Some(e))) => format!("Couldn't move to trash: {e}"),
            Err(_) => "Couldn't move to trash".into(),
        });
        if !matches!(app.current_loc(), Loc::Dir(_)) {
            app.reload();
        }
    });
}

/// Pins the dropped folders (checked off the UI thread; files are skipped).
fn pin_folders(app: &Rc<App>, files: Vec<gio::File>) {
    let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
    let app = app.clone();
    glib::spawn_future_local(async move {
        let dirs = gio::spawn_blocking(move || {
            uris.into_iter()
                .filter(|u| {
                    gio::File::for_uri(u).query_file_type(gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>)
                        == gio::FileType::Directory
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        if dirs.is_empty() {
            app.status.set_text("Only folders can be pinned");
            return;
        }
        let n = dirs.len();
        for uri in dirs {
            app.set_pinned(uri, true);
        }
        app.status.set_text(&format!("Pinned {n} folder(s) to the sidebar"));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdk::DragAction as A;

    #[test]
    fn preference_follows_device_and_modifiers() {
        assert_eq!(preference(false, false, true), A::MOVE);
        assert_eq!(preference(false, false, false), A::COPY);
        assert_eq!(preference(true, false, true), A::COPY);
        assert_eq!(preference(false, true, false), A::MOVE);
        assert_eq!(preference(true, true, false), A::COPY);
    }

    #[test]
    fn final_action_respects_narrowed_drag() {
        let all = A::COPY | A::MOVE | A::LINK;
        assert_eq!(final_action(all, A::MOVE), A::MOVE);
        assert_eq!(final_action(all, A::COPY), A::COPY);
        // The compositor / source already picked one
        assert_eq!(final_action(A::COPY, A::MOVE), A::COPY);
        assert_eq!(final_action(A::MOVE | A::LINK, A::COPY), A::MOVE);
        assert_eq!(final_action(A::LINK, A::COPY), A::empty());
    }

    #[test]
    fn never_into_itself_and_same_folder_move_is_noop() {
        let src = Path::new("/a/b");
        assert!(!keeps(src, Path::new("/a/b"), false));
        assert!(!keeps(src, Path::new("/a/b/c"), true));
        assert!(!keeps(src, Path::new("/a"), true));
        assert!(keeps(src, Path::new("/a"), false)); // copy next to itself
        assert!(keeps(src, Path::new("/x"), true));
        assert!(keeps(Path::new("/a/bc"), Path::new("/a/b"), true));
    }

    #[test]
    fn device_check() {
        let base = std::env::temp_dir().join(format!("rfiles-dnd-test-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("f"), b"x").unwrap();
        assert!(same_device(&[a.join("f")], &b));
        assert!(!same_device(&[a.join("f")], &base.join("missing")));
        let v = verdict(&[a.join("f")], &a);
        assert!(v.same_device && v.can_copy && !v.can_move);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn folder_destinations() {
        assert_eq!(dest_for_folder(&gio::File::for_path("/tmp")), Some(Dest::Dir("/tmp".into())));
        assert_eq!(dest_for_folder(&gio::File::for_uri("trash:///")), Some(Dest::Trash));
        assert_eq!(dest_for_folder(&gio::File::for_uri("trash:///x")), None);
        assert_eq!(dest_for_folder(&gio::File::for_uri("recent:///")), None);
    }
}
