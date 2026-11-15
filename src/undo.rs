//! Undo and redo (Ctrl+Z / Ctrl+Shift+Z) of renames, moves, copies, new folders and moves to the trash.
//!
//! One history for the whole process, kept on the UI thread, so every window shares it. Operations are recorded
//! once they have ended, from what they report. Undo and redo never overwrite and never delete permanently: each
//! step first checks that the item is still where the operation left it and that its place is free, and skips it
//! with a reason otherwise. The file work runs on a worker thread, one undo or redo at a time.

use crate::app::{App, Loc};
use crate::fileops::Transferred;
use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// How many operations can be undone.
const MAX: usize = 30;

const NOFOLLOW: gio::FileQueryInfoFlags = gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS;

/// An item moved to the trash: where it was, and its trash:/// URI.
#[derive(Clone, Debug, PartialEq)]
pub struct Trashed {
    pub orig: PathBuf,
    pub item: String,
}

/// An operation that can be undone, holding only the items that can.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// (before, after). As batch rename reports them, an item inside a renamed folder has its new name in the
    /// folder's old path.
    Rename(Vec<(PathBuf, PathBuf)>),
    /// (source, target)
    Move(Vec<(PathBuf, PathBuf)>),
    /// (source, copy)
    Copy(Vec<(PathBuf, PathBuf)>),
    NewFolder(PathBuf),
    Trash(Vec<Trashed>),
}

fn name(p: &Path) -> String {
    p.file_name().unwrap_or(p.as_os_str()).to_string_lossy().into_owned()
}

fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}

impl Op {
    fn len(&self) -> usize {
        match self {
            Op::Rename(v) | Op::Move(v) | Op::Copy(v) => v.len(),
            Op::NewFolder(_) => 1,
            Op::Trash(v) => v.len(),
        }
    }

    /// What it did, for "Undo …" and "Redo …": "Move of 3 items", "Rename “a.txt”".
    pub fn describe(&self) -> String {
        let (what, one) = match self {
            Op::Rename(v) => ("Rename", v.first().map(|p| &p.0)),
            Op::Move(v) => ("Move", v.first().map(|p| &p.0)),
            Op::Copy(v) => ("Copy", v.first().map(|p| &p.0)),
            Op::NewFolder(p) => ("New Folder", Some(p)),
            Op::Trash(v) => ("Move to Trash", v.first().map(|t| &t.orig)),
        };
        match (self.len(), one) {
            (1, Some(p)) => format!("{what} “{}”", name(p)),
            (n, _) => format!("{what} of {}", items(n)),
        }
    }
}

/// The undo and redo stacks.
#[derive(Default)]
struct History {
    undo: VecDeque<Op>,
    redo: Vec<Op>,
    /// Counts new operations, so an undo that ends after one doesn't fill the redo stack again
    generation: u64,
}

impl History {
    /// A new operation: undoable, and nothing undone earlier can be redone any more.
    fn push(&mut self, op: Op) {
        if op.len() == 0 {
            return;
        }
        self.push_undo(op);
        self.redo.clear();
        self.generation += 1;
    }

    fn push_undo(&mut self, op: Op) {
        self.undo.push_back(op);
        while self.undo.len() > MAX {
            self.undo.pop_front();
        }
    }

    /// The latest operation, to undo (`redo` false) or redo, with the generation it was taken at.
    fn take(&mut self, redo: bool) -> Option<(Op, u64)> {
        let op = if redo { self.redo.pop() } else { self.undo.pop_back() };
        op.map(|o| (o, self.generation))
    }

    /// After an undo or redo: the items it did can be redone or undone again. Items it couldn't do are dropped.
    fn finish(&mut self, redo: bool, done: Option<Op>, generation: u64) {
        let Some(op) = done.filter(|o| o.len() > 0) else { return };
        if redo {
            self.push_undo(op);
        } else if generation == self.generation {
            self.redo.push(op);
        }
    }

    fn label(&self, redo: bool) -> Option<String> {
        if redo {
            self.redo.last().map(|o| format!("Redo {}", o.describe()))
        } else {
            self.undo.back().map(|o| format!("Undo {}", o.describe()))
        }
    }
}

thread_local! {
    static HISTORY: RefCell<History> = RefCell::default();
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

/// Records an operation that has just ended.
pub fn record(op: Op) {
    HISTORY.with(|h| h.borrow_mut().push(op));
}

/// The menu label for undo (`redo` false) or redo, or None when there is nothing to do or one is running.
pub fn label(redo: bool) -> Option<String> {
    if BUSY.get() {
        return None;
    }
    HISTORY.with(|h| h.borrow().label(redo))
}

/// The undoable part of a copy or move: items that replaced something can't be undone, their old content is gone.
/// Also returns how many replaced.
fn transfer_op(cut: bool, done: &[Transferred]) -> (Op, usize) {
    let pairs: Vec<(PathBuf, PathBuf)> =
        done.iter().filter(|t| !t.replaced).map(|t| (t.source.clone(), t.target.clone())).collect();
    let replaced = done.len() - pairs.len();
    (if cut { Op::Move(pairs) } else { Op::Copy(pairs) }, replaced)
}

/// What the banner adds when some items of a copy or move replaced others.
fn replaced_note(undoable: usize, replaced: usize) -> String {
    match (undoable, replaced) {
        (_, 0) => String::new(),
        (0, _) => "; replacing can't be undone".into(),
        (n, _) => format!("; Undo covers only the {} that replaced nothing", items(n)),
    }
}

/// Records the items a copy or move completed; returns a note for its banner line.
pub fn record_transfer(cut: bool, done: &[Transferred]) -> String {
    let (op, replaced) = transfer_op(cut, done);
    let note = replaced_note(op.len(), replaced);
    record(op);
    note
}

/// What the banner adds when some trashed items weren't found in the trash.
fn not_found_note(missing: usize) -> String {
    match missing {
        0 => String::new(),
        n => format!("; {} can't be undone (not found in the trash)", items(n)),
    }
}

/// Records a move to the trash from the originals and the trash items `find_trash_items` found for them; returns a
/// note for its banner line.
pub fn record_trash(origs: &[PathBuf], found: Vec<Option<String>>) -> String {
    let (op, missing) = trash_op(origs, found);
    record(op);
    not_found_note(missing)
}

fn trash_op(origs: &[PathBuf], found: Vec<Option<String>>) -> (Op, usize) {
    let list: Vec<Trashed> = origs
        .iter()
        .zip(found)
        .filter_map(|(o, f)| f.map(|item| Trashed { orig: o.clone(), item }))
        .collect();
    let missing = origs.len() - list.len();
    (Op::Trash(list), missing)
}

/// Records renames from (old URI, new URI) pairs. Items without a local path are left out.
pub fn record_renames(renames: &[(String, String)]) {
    let pairs = renames
        .iter()
        .filter_map(|(a, b)| Some((gio::File::for_uri(a).path()?, gio::File::for_uri(b).path()?)))
        // Renaming to the same name changed nothing
        .filter(|(a, b)| a != b)
        .collect();
    record(Op::Rename(pairs));
}

// ================= finding trash items =================

/// The local time as the trash writes deletion dates, "YYYY-MM-DDThh:mm:ss". Taken before trashing.
pub fn trash_stamp() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|d| d.format("%Y-%m-%dT%H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Which of `candidates` (original path, deletion date as GIO gives it, "YYYY-MM-DDThh:mm:ss") is the item just
/// trashed from `orig`: the newest one from that path, deleted no earlier than `since`. None when there is none, or
/// two share the newest date, since then it can't be told which one it is.
fn pick_trash_entry(orig: &Path, since: &str, candidates: &[(PathBuf, String)]) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut tie = false;
    for (i, (p, date)) in candidates.iter().enumerate() {
        if p != orig || date.as_str() < since {
            continue;
        }
        match best.map(|b| date.cmp(&candidates[b].1)) {
            None | Some(std::cmp::Ordering::Greater) => {
                best = Some(i);
                tie = false;
            }
            Some(std::cmp::Ordering::Equal) => tie = true,
            Some(std::cmp::Ordering::Less) => {}
        }
    }
    if tie { None } else { best }
}

/// The trash:/// URI of each item trashed from `origs` since `since` (a `trash_stamp`), found by listing the trash
/// (GIO gives no handle back). The trash backend may list new items a moment late, so it looks again a few times
/// while some are missing. Runs on a worker thread.
pub fn find_trash_items(origs: &[PathBuf], since: &str) -> Vec<Option<String>> {
    let mut found = vec![None; origs.len()];
    if origs.is_empty() {
        return found;
    }
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(100 * attempt));
        }
        let (cands, uris) = list_trash();
        for (f, o) in found.iter_mut().zip(origs).filter(|(f, _)| f.is_none()) {
            *f = pick_trash_entry(o, since, &cands).map(|i| uris[i].clone());
        }
        if found.iter().all(Option::is_some) {
            break;
        }
    }
    found
}

/// (original path, deletion date) and URI of each item at the top of the trash.
fn list_trash() -> (Vec<(PathBuf, String)>, Vec<String>) {
    let trash = gio::File::for_uri("trash:///");
    let c = None::<&gio::Cancellable>;
    let mut cands = Vec::new();
    let mut uris = Vec::new();
    if let Ok(en) = trash.enumerate_children("standard::name,trash::orig-path,trash::deletion-date", NOFOLLOW, c) {
        while let Ok(Some(info)) = en.next_file(c) {
            let Some(orig) = crate::trash::orig_path(&info) else { continue };
            let date = info.attribute_string("trash::deletion-date").map(|d| d.to_string()).unwrap_or_default();
            cands.push((orig, date));
            uris.push(trash.child(info.name()).uri().to_string());
        }
    }
    (cands, uris)
}

// ================= moving without overwriting =================

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

fn is_dir(p: &Path) -> bool {
    p.symlink_metadata().is_ok_and(|m| m.is_dir())
}

/// rename(2) that fails instead of replacing an item at `to`. Drives that can't do that (some FAT and network
/// ones) get a plain rename; the caller has just checked that `to` is free.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::{c_char, c_int, c_uint, CString};
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn renameat2(olddirfd: c_int, oldpath: *const c_char, newdirfd: c_int, newpath: *const c_char, flags: c_uint)
            -> c_int;
    }
    const AT_FDCWD: c_int = -100;
    const RENAME_NOREPLACE: c_uint = 1;
    let a = CString::new(from.as_os_str().as_bytes())?;
    let b = CString::new(to.as_os_str().as_bytes())?;
    if unsafe { renameat2(AT_FDCWD, a.as_ptr(), AT_FDCWD, b.as_ptr(), RENAME_NOREPLACE) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    // EINVAL, ENOSYS, EOPNOTSUPP: the flag isn't supported there
    if matches!(e.raw_os_error(), Some(22 | 38 | 95)) {
        return std::fs::rename(from, to);
    }
    Err(e)
}

/// Copies `from` to the free `to`, through a hidden name next to it, so a failure leaves only the original. The
/// partial copy is removed: it is ours.
fn copy_new(from: &Path, to: &Path) -> io::Result<()> {
    let progress = crate::fileops::CopyProgress::default();
    let mut skipped = 0;
    let tmp = temp_next_to(to);
    let res = crate::fileops::copy_recursive(from, &tmp, &progress, &mut skipped).and_then(|()| {
        if skipped > 0 {
            return Err(io::Error::other("holds pipes, sockets or devices that can't be copied"));
        }
        rename_noreplace(&tmp, to)
    });
    if res.is_err() {
        let _ = crate::fileops::remove_any(&tmp);
    }
    res
}

/// Moves `from` to `to`, never overwriting. Across drives the item is copied, then the original removed.
fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    if exists(to) {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    match rename_noreplace(from, to) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_new(from, to)?;
            crate::fileops::remove_any(from)
        }
        r => r,
    }
}

/// A free hidden name next to `p`, for an item whose place another item of the same undo still holds.
fn temp_next_to(p: &Path) -> PathBuf {
    let dir = p.parent().unwrap_or(Path::new("/"));
    for i in 0.. {
        let t = dir.join(format!(".rfiles-undo-{}-{i}", std::process::id()));
        if !exists(&t) {
            return t;
        }
    }
    unreachable!()
}

fn changed_since(p: &Path) -> String {
    format!("“{}” was changed or moved since", name(p))
}

/// Moves each (from, to) of `pairs`, never overwriting, and returns which were moved and a line for each that
/// wasn't. The order is found as it goes: an item waits while it isn't there yet (it is inside a folder that moves
/// first) or while its place is held by another item of the list; deeper items go first, so a folder's contents
/// move before the folder itself. Items that hold each other's place (swapped names) go through a temporary name.
fn relocate(pairs: &[(PathBuf, PathBuf)]) -> (Vec<bool>, Vec<String>) {
    let n = pairs.len();
    let mut cur: Vec<PathBuf> = pairs.iter().map(|p| p.0.clone()).collect();
    let mut done = vec![false; n];
    let mut open = vec![true; n];
    let mut temped = vec![false; n];
    let mut errors = Vec::new();
    let depth = |p: &Path| p.components().count();
    let parent_ok = |p: &Path| p.parent().is_some_and(is_dir);
    loop {
        let pending: Vec<usize> = (0..n).filter(|&i| open[i]).collect();
        if pending.is_empty() {
            break;
        }
        let ready = pending
            .iter()
            .copied()
            .filter(|&i| exists(&cur[i]) && !exists(&pairs[i].1) && parent_ok(&pairs[i].1))
            .max_by_key(|&i| (depth(&cur[i]), std::cmp::Reverse(i)));
        if let Some(i) = ready {
            open[i] = false;
            match move_no_replace(&cur[i], &pairs[i].1) {
                Ok(()) => done[i] = true,
                Err(e) => {
                    errors.push(format!("“{}”: {e}", name(&pairs[i].0)));
                    if temped[i] && move_no_replace(&cur[i], &pairs[i].0).is_err() {
                        errors.push(format!("“{}” was left at {}", name(&pairs[i].0), cur[i].display()));
                    }
                }
            }
            continue;
        }
        // Stuck: break a cycle by moving one item whose place another one holds out of the way
        let held = |i: usize| pending.iter().any(|&j| j != i && cur[j] == pairs[i].1);
        if let Some(i) = pending.iter().copied().find(|&i| !temped[i] && exists(&cur[i]) && held(i)) {
            let t = temp_next_to(&cur[i]);
            match move_no_replace(&cur[i], &t) {
                Ok(()) => {
                    cur[i] = t;
                    temped[i] = true;
                }
                Err(e) => {
                    open[i] = false;
                    errors.push(format!("“{}”: {e}", name(&pairs[i].0)));
                }
            }
            continue;
        }
        for i in pending {
            open[i] = false;
            let to = &pairs[i].1;
            errors.push(if !exists(&cur[i]) {
                changed_since(&pairs[i].0)
            } else if !parent_ok(to) {
                format!("“{}”: the folder {} is gone", name(&pairs[i].0), to.parent().unwrap_or(to).display())
            } else {
                format!("“{}”: there is already an item at {}", name(&pairs[i].0), to.display())
            });
            if temped[i] && move_no_replace(&cur[i], &pairs[i].0).is_err() {
                errors.push(format!("“{}” was left at {}", name(&pairs[i].0), cur[i].display()));
            }
        }
        break;
    }
    (done, errors)
}

/// The (old, new) pairs for `note_renames` after undoing `done` renames (before, after). Each pair is turned round;
/// a path inside another renamed folder is given in that folder's current (after) path, as `note_renames` expects
/// the parent's old path in nested pairs.
fn reversed_renames(done: &[(PathBuf, PathBuf)]) -> Vec<(PathBuf, PathBuf)> {
    let mut folders: Vec<&(PathBuf, PathBuf)> = done.iter().collect();
    folders.sort_by_key(|(b, _)| std::cmp::Reverse(b.components().count()));
    let map = |p: &Path| {
        let mut p = p.to_path_buf();
        for (b, a) in &folders {
            if p != *b {
                if let Ok(rest) = p.strip_prefix(b) {
                    p = a.join(rest);
                }
            }
        }
        p
    };
    done.iter().map(|(b, a)| (map(a), map(b))).collect()
}

// ================= undo and redo of each operation =================

/// What an undo or redo did.
#[derive(Debug, Default)]
struct Applied {
    /// The items it did, as the original operation, for the other stack
    op: Option<Op>,
    done: usize,
    errors: Vec<String>,
    /// (old path, new path) of renamed items, for pins and stars
    renames: Vec<(PathBuf, PathBuf)>,
    /// Added to the banner line
    note: String,
}

fn keep<T: Clone>(list: &[T], done: &[bool]) -> Vec<T> {
    list.iter().zip(done).filter(|(_, &d)| d).map(|(x, _)| x.clone()).collect()
}

fn turned(pairs: &[(PathBuf, PathBuf)]) -> Vec<(PathBuf, PathBuf)> {
    pairs.iter().map(|(a, b)| (b.clone(), a.clone())).collect()
}

/// Undoes (`redo` false) or redoes `op`. Runs on a worker thread.
fn apply(op: Op, redo: bool) -> Applied {
    let mut out = Applied::default();
    let done: Vec<bool>;
    out.op = Some(match op {
        Op::Rename(pairs) => {
            let steps = if redo { pairs.clone() } else { turned(&pairs) };
            let (d, errors) = relocate(&steps);
            out.errors = errors;
            let kept = keep(&pairs, &d);
            out.renames = if redo { kept.clone() } else { reversed_renames(&kept) };
            done = d;
            Op::Rename(kept)
        }
        Op::Move(pairs) => {
            let steps = if redo { pairs.clone() } else { turned(&pairs) };
            let (d, errors) = relocate(&steps);
            out.errors = errors;
            done = d;
            Op::Move(keep(&pairs, &done))
        }
        Op::Copy(pairs) => {
            let each = |(s, c): &(PathBuf, PathBuf)| if redo { copy_again(s, c) } else { trash_copy(c) };
            done = pairs.iter().map(|p| step(&mut out.errors, each(p))).collect();
            Op::Copy(keep(&pairs, &done))
        }
        Op::NewFolder(p) => {
            done = vec![step(&mut out.errors, if redo { create_again(&p) } else { remove_if_empty(&p) })];
            Op::NewFolder(p)
        }
        Op::Trash(list) if redo => {
            let since = trash_stamp();
            done = list.iter().map(|t| step(&mut out.errors, trash_again(&t.orig))).collect();
            let origs: Vec<PathBuf> = keep(&list, &done).into_iter().map(|t| t.orig).collect();
            let (op, missing) = trash_op(&origs, find_trash_items(&origs, &since));
            out.note = not_found_note(missing);
            op
        }
        Op::Trash(list) => {
            done = list.iter().map(|t| step(&mut out.errors, restore_one(t))).collect();
            Op::Trash(keep(&list, &done))
        }
    });
    out.done = done.iter().filter(|&&d| d).count();
    if out.done == 0 {
        out.op = None;
    }
    out
}

fn step(errors: &mut Vec<String>, r: Result<(), String>) -> bool {
    r.map_err(|e| errors.push(e)).is_ok()
}

/// Undo of a copy: the copy goes to the trash, never deleted.
fn trash_copy(copy: &Path) -> Result<(), String> {
    if !exists(copy) {
        return Err(changed_since(copy));
    }
    gio::File::for_path(copy)
        .trash(None::<&gio::Cancellable>)
        .map_err(|e| format!("“{}”: {}; it was left where it is", name(copy), e.message()))
}

/// Redo of a copy: copies the source again, if it is still there and the copy's name is free.
fn copy_again(src: &Path, copy: &Path) -> Result<(), String> {
    if !exists(src) {
        return Err(changed_since(src));
    }
    free_place(copy)?;
    copy_new(src, copy).map_err(|e| format!("“{}”: {e}", name(src)))
}

/// Err when `p` is taken or its folder is gone.
fn free_place(p: &Path) -> Result<(), String> {
    if exists(p) {
        return Err(format!("“{}”: there is already an item at {}", name(p), p.display()));
    }
    match p.parent() {
        Some(d) if is_dir(d) => Ok(()),
        _ => Err(format!("“{}”: the folder {} is gone", name(p), p.parent().unwrap_or(p).display())),
    }
}

/// Undo of a new folder: removes it only while it is empty.
fn remove_if_empty(p: &Path) -> Result<(), String> {
    if !is_dir(p) {
        return Err(changed_since(p));
    }
    // rmdir refuses a folder with anything in it, so nothing inside can be lost
    std::fs::remove_dir(p).map_err(|e| match e.kind() {
        io::ErrorKind::DirectoryNotEmpty => format!("“{}” isn't empty, so it was left alone", name(p)),
        _ => format!("“{}”: {e}", name(p)),
    })
}

fn create_again(p: &Path) -> Result<(), String> {
    free_place(p)?;
    std::fs::create_dir(p).map_err(|e| format!("“{}”: {e}", name(p)))
}

/// Undo of a move to the trash: moves the trash item back to its original path, if that is free.
fn restore_one(t: &Trashed) -> Result<(), String> {
    let item = gio::File::for_uri(&t.item);
    if item.query_info("standard::name", NOFOLLOW, None::<&gio::Cancellable>).is_err() {
        return Err(format!("“{}” is no longer in the trash", name(&t.orig)));
    }
    free_place(&t.orig)?;
    // No OVERWRITE flag: if something appeared there meanwhile, this fails instead of replacing it
    item.move_(
        &gio::File::for_path(&t.orig),
        gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
        None::<&gio::Cancellable>,
        None,
    )
    .map_err(|e| format!("“{}”: {}", name(&t.orig), e.message()))
}

fn trash_again(orig: &Path) -> Result<(), String> {
    if !exists(orig) {
        return Err(changed_since(orig));
    }
    gio::File::for_path(orig)
        .trash(None::<&gio::Cancellable>)
        .map_err(|e| format!("“{}”: {}", name(orig), e.message()))
}

/// The banner line after an undo or redo of `what` (an `Op::describe`), which did `done` of `total` items.
fn result_text(redo: bool, what: &str, done: usize, total: usize, errors: &[String]) -> String {
    let verb = if redo { "Redid" } else { "Undid" };
    let done_text =
        if done == total { format!("{verb} {what}") } else { format!("{verb} {what} for {done} of {total}") };
    let none = |_| format!("Couldn't {} {what}", if redo { "redo" } else { "undo" });
    crate::banner::outcome_text(&done_text, none, done, errors)
}

/// Undoes (`redo` false) or redoes the latest operation on a worker thread, then shows the result in the banner.
/// Does nothing while another undo or redo is running. The window is held weakly and the application kept
/// running until the work is done.
pub fn run(app: &Rc<App>, redo: bool) {
    if BUSY.get() {
        return;
    }
    let Some((op, generation)) = HISTORY.with(|h| h.borrow_mut().take(redo)) else {
        app.status.set_text(if redo { "Nothing to redo" } else { "Nothing to undo" });
        return;
    };
    BUSY.set(true);
    let what = op.describe();
    let total = op.len();
    app.status.set_text(&format!("{} {what}…", if redo { "Redoing" } else { "Undoing" }));
    let hold = crate::jobs::start(&format!("{} {what}", if redo { "Redoing" } else { "Undoing" }), None);
    let weak = Rc::downgrade(app);
    glib::spawn_future_local(async move {
        let res = gio::spawn_blocking(move || apply(op, redo)).await;
        BUSY.set(false);
        let (notice, renames) = match res {
            Ok(a) => {
                HISTORY.with(|h| h.borrow_mut().finish(redo, a.op, generation));
                let text = result_text(redo, &what, a.done, total, &a.errors) + &a.note;
                let uri = |p: &PathBuf| gio::File::for_path(p).uri().to_string();
                let renames: Vec<(String, String)> = a.renames.iter().map(|(o, n)| (uri(o), uri(n))).collect();
                (crate::banner::Notice { text, errors: a.errors }, renames)
            }
            Err(_) => (crate::banner::Notice::stopped(if redo { "Redo failed" } else { "Undo failed" }), Vec::new()),
        };
        if let Some(app) = weak.upgrade().filter(|_| !renames.is_empty()) {
            app.note_renames(&renames);
        }
        crate::jobs::deliver(&weak, notice);
        drop(hold);
        let Some(app) = weak.upgrade() else { return };
        // Folder views update themselves; Starred / search results don't
        if !matches!(app.current_loc(), Loc::Dir(_)) {
            app.reload();
        }
    });
}

#[cfg(test)]
mod undo_tests {
    use super::*;
    use std::fs;

    fn base(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rfiles-undo-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, s: &str) {
        fs::write(p, s).unwrap();
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }

    fn mv(d: &Path, a: &str, b: &str) -> (PathBuf, PathBuf) {
        (d.join(a), d.join(b))
    }

    #[test]
    fn history_is_bounded_and_new_operations_clear_redo() {
        let mut h = History::default();
        for i in 0..MAX + 5 {
            h.push(Op::NewFolder(PathBuf::from(format!("/x/{i}"))));
        }
        assert_eq!(h.undo.len(), MAX);
        assert_eq!(h.undo.front(), Some(&Op::NewFolder("/x/5".into())), "the oldest go first");
        h.push(Op::Move(Vec::new()));
        assert_eq!(h.undo.len(), MAX, "nothing undoable is not recorded");

        let (op, g) = h.take(false).unwrap();
        h.finish(false, Some(op.clone()), g);
        assert_eq!(h.redo, vec![op.clone()]);
        // Redo puts it back on the undo stack and keeps the rest of redo
        let (op2, g2) = h.take(true).unwrap();
        h.finish(true, Some(op2), g2);
        assert_eq!(h.undo.back(), Some(&op));
        assert!(h.redo.is_empty());

        let (op, g) = h.take(false).unwrap();
        h.finish(false, Some(op), g);
        assert_eq!(h.redo.len(), 1);
        h.push(Op::NewFolder("/y".into()));
        assert!(h.redo.is_empty(), "a new operation clears redo");

        // An undo that ends after a new operation doesn't fill redo again
        let (op, g) = h.take(false).unwrap();
        h.push(Op::NewFolder("/z".into()));
        h.finish(false, Some(op), g);
        assert!(h.redo.is_empty());
        // Nothing done: nothing to redo
        let (_, g) = h.take(false).unwrap();
        h.finish(false, None, g);
        assert!(h.redo.is_empty());
    }

    #[test]
    fn labels() {
        let one = Op::Rename(vec![("/d/a.txt".into(), "/d/b.txt".into())]);
        assert_eq!(one.describe(), "Rename “a.txt”");
        let three = Op::Move((0..3).map(|i| (PathBuf::from(format!("/a/{i}")), PathBuf::from("/b"))).collect());
        assert_eq!(three.describe(), "Move of 3 items");
        assert_eq!(Op::Copy(vec![("/a/x".into(), "/b/x".into())]).describe(), "Copy “x”");
        assert_eq!(Op::NewFolder("/a/New Folder".into()).describe(), "New Folder “New Folder”");
        let t = Trashed { orig: "/a/x".into(), item: "trash:///x".into() };
        assert_eq!(Op::Trash(vec![t.clone(), t]).describe(), "Move to Trash of 2 items");
        let mut h = History::default();
        assert_eq!(h.label(false), None);
        h.push(three);
        assert_eq!(h.label(false).as_deref(), Some("Undo Move of 3 items"));
        let (op, g) = h.take(false).unwrap();
        h.finish(false, Some(op), g);
        assert_eq!(h.label(true).as_deref(), Some("Redo Move of 3 items"));
        assert_eq!(h.label(false), None);
    }

    #[test]
    fn result_lines() {
        assert_eq!(result_text(false, "Move of 3 items", 3, 3, &[]), "Undid Move of 3 items");
        let e = vec!["“x” was changed or moved since".to_string()];
        assert_eq!(
            result_text(false, "Move of 3 items", 2, 3, &e),
            "Undid Move of 3 items for 2 of 3, 1 failed — “x” was changed or moved since"
        );
        let none = "Couldn't redo Rename “a” — “x” was changed or moved since";
        assert_eq!(result_text(true, "Rename “a”", 0, 1, &e), none);
    }

    #[test]
    fn replaced_items_are_not_undoable() {
        let t = |s: &str, r| Transferred { source: s.into(), target: format!("/d/{s}").into(), replaced: r };
        let (op, replaced) = transfer_op(true, &[t("a", false), t("b", true), t("c", false)]);
        assert_eq!(op, Op::Move(vec![("a".into(), "/d/a".into()), ("c".into(), "/d/c".into())]));
        assert_eq!(replaced, 1);
        let (op, _) = transfer_op(false, &[t("a", false)]);
        assert!(matches!(op, Op::Copy(_)));
        assert_eq!(replaced_note(2, 0), "");
        assert_eq!(replaced_note(0, 2), "; replacing can't be undone");
        assert_eq!(replaced_note(2, 1), "; Undo covers only the 2 items that replaced nothing");
        let (op, missing) = trash_op(&["/a".into(), "/b".into()], vec![None, Some("trash:///b".into())]);
        assert_eq!(op, Op::Trash(vec![Trashed { orig: "/b".into(), item: "trash:///b".into() }]));
        assert_eq!(not_found_note(missing), "; 1 item can't be undone (not found in the trash)");
    }

    #[test]
    fn picks_the_newest_trash_entry_from_that_path() {
        let c = |p: &str, d: &str| (PathBuf::from(p), d.to_string());
        let cands = vec![
            c("/h/a.txt", "2026-11-14T10:00:00"),
            c("/h/b.txt", "2026-11-14T12:00:00"),
            c("/h/a.txt", "2026-11-14T11:30:00"),
            c("/h/a.txt", "2025-12-01T23:59:59"),
        ];
        let pick = |p: &str, c: &[(PathBuf, String)]| pick_trash_entry(Path::new(p), "2026-01-01T00:00:00", c);
        assert_eq!(pick("/h/a.txt", &cands), Some(2));
        assert_eq!(pick("/h/b.txt", &cands), Some(1));
        assert_eq!(pick("/h/c.txt", &cands), None);
        // Trashed before this operation began: an older item from the same path, not this one
        assert_eq!(pick_trash_entry(Path::new("/h/a.txt"), "2026-11-14T11:30:01", &cands), None);
        assert_eq!(pick_trash_entry(Path::new("/h/a.txt"), "2026-11-14T11:30:00", &cands), Some(2));
        // Two from the same path in the same second: can't tell which, so no guess
        let tie = vec![c("/h/a.txt", "2026-11-14T11:30:00"), c("/h/a.txt", "2026-11-14T11:30:00")];
        assert_eq!(pick("/h/a.txt", &tie), None);
        // An older tie doesn't matter once a newer one is there
        let later = vec![tie[0].clone(), tie[1].clone(), c("/h/a.txt", "2026-11-14T11:30:01")];
        assert_eq!(pick("/h/a.txt", &later), Some(2));
    }

    #[test]
    fn never_overwrites() {
        let d = base("noreplace");
        write(&d.join("a"), "a");
        write(&d.join("b"), "b");
        assert!(move_no_replace(&d.join("a"), &d.join("b")).is_err());
        assert!(rename_noreplace(&d.join("a"), &d.join("b")).is_err());
        assert_eq!((read(&d.join("a")), read(&d.join("b"))), ("a".into(), "b".into()));
        move_no_replace(&d.join("a"), &d.join("c")).unwrap();
        assert_eq!(read(&d.join("c")), "a");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn copy_new_leaves_nothing_behind_on_failure() {
        let d = base("copynew");
        fs::create_dir(d.join("src")).unwrap();
        write(&d.join("src/f"), "data");
        copy_new(&d.join("src"), &d.join("dst")).unwrap();
        assert_eq!(read(&d.join("dst/f")), "data");
        assert!(copy_new(&d.join("missing"), &d.join("dst2")).is_err());
        let left: Vec<_> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 2, "no temporary is left: {left:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rename_back_and_again() {
        let d = base("rename");
        write(&d.join("b.txt"), "x");
        let op = Op::Rename(vec![mv(&d, "a.txt", "b.txt")]);
        let a = apply(op.clone(), false);
        assert!(a.errors.is_empty(), "{:?}", a.errors);
        assert_eq!(read(&d.join("a.txt")), "x");
        assert!(!d.join("b.txt").exists());
        assert_eq!(a.op, Some(op.clone()));
        assert_eq!(a.renames, vec![mv(&d, "b.txt", "a.txt")], "pins and stars follow back");
        let r = apply(op.clone(), true);
        assert!(r.errors.is_empty());
        assert_eq!(read(&d.join("b.txt")), "x");
        assert_eq!(r.renames, vec![mv(&d, "a.txt", "b.txt")]);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn swapped_names_go_back() {
        let d = base("swap");
        // A batch rename swapped a and b
        write(&d.join("a"), "was b");
        write(&d.join("b"), "was a");
        let op = Op::Rename(vec![mv(&d, "a", "b"), mv(&d, "b", "a")]);
        let a = apply(op, false);
        assert!(a.errors.is_empty(), "{:?}", a.errors);
        assert_eq!((read(&d.join("a")), read(&d.join("b"))), ("was a".into(), "was b".into()));
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2, "no temporary is left");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn nested_renames_undo_and_redo() {
        let d = base("nested");
        // Folder F became G and its item a became b; the item is reported in the folder's old path
        fs::create_dir(d.join("G")).unwrap();
        write(&d.join("G/b"), "x");
        let op = Op::Rename(vec![mv(&d, "F", "G"), mv(&d, "F/a", "F/b")]);
        let a = apply(op.clone(), false);
        assert!(a.errors.is_empty(), "{:?}", a.errors);
        assert_eq!(read(&d.join("F/a")), "x");
        assert_eq!(a.renames, vec![mv(&d, "G", "F"), mv(&d, "G/b", "G/a")]);
        let r = apply(op, true);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(read(&d.join("G/b")), "x");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn move_back_and_again() {
        let d = base("move");
        fs::create_dir_all(d.join("from")).unwrap();
        fs::create_dir_all(d.join("to/sub")).unwrap();
        write(&d.join("to/x (copy)"), "x");
        write(&d.join("to/sub/f"), "f");
        let op = Op::Move(vec![mv(&d, "from/x", "to/x (copy)"), mv(&d, "from/sub", "to/sub")]);
        let a = apply(op.clone(), false);
        assert!(a.errors.is_empty(), "{:?}", a.errors);
        assert_eq!(read(&d.join("from/x")), "x", "goes back under its old name");
        assert_eq!(read(&d.join("from/sub/f")), "f");
        assert_eq!(a.done, 2);
        let r = apply(op, true);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(read(&d.join("to/x (copy)")), "x");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn changed_items_are_skipped_and_the_rest_done() {
        let d = base("changed");
        fs::create_dir_all(d.join("from")).unwrap();
        fs::create_dir_all(d.join("to")).unwrap();
        write(&d.join("to/a"), "a");
        write(&d.join("to/c"), "c");
        // b was moved away since; the name a is taken in its old folder
        write(&d.join("from/a"), "someone else's");
        let op = Op::Move(vec![mv(&d, "from/a", "to/a"), mv(&d, "from/b", "to/b"), mv(&d, "from/c", "to/c")]);
        let a = apply(op, false);
        assert_eq!(a.done, 1);
        assert_eq!(a.op, Some(Op::Move(vec![mv(&d, "from/c", "to/c")])), "only the undone part can be redone");
        assert_eq!(a.errors.len(), 2, "{:?}", a.errors);
        assert!(a.errors.iter().any(|e| e == "“b” was changed or moved since"), "{:?}", a.errors);
        assert!(a.errors.iter().any(|e| e.starts_with("“a”: there is already an item at")), "{:?}", a.errors);
        assert_eq!(read(&d.join("from/a")), "someone else's");
        assert_eq!(read(&d.join("to/a")), "a");
        assert_eq!(read(&d.join("from/c")), "c");
        // Its old folder is gone
        let a = apply(Op::Move(vec![mv(&d, "gone/a", "to/a")]), false);
        assert_eq!(a.op, None);
        assert!(a.errors[0].contains("is gone"), "{:?}", a.errors);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn new_folder_removed_only_while_empty() {
        let d = base("newfolder");
        let p = d.join("New Folder");
        fs::create_dir(&p).unwrap();
        let a = apply(Op::NewFolder(p.clone()), false);
        assert!(a.errors.is_empty() && !p.exists());
        let r = apply(Op::NewFolder(p.clone()), true);
        assert!(r.errors.is_empty() && p.is_dir());
        write(&p.join("f"), "f");
        let a = apply(Op::NewFolder(p.clone()), false);
        assert_eq!(a.errors, vec!["“New Folder” isn't empty, so it was left alone".to_string()]);
        assert_eq!(a.op, None);
        assert_eq!(read(&p.join("f")), "f");
        // Redo when the name is taken again does nothing
        assert_eq!(apply(Op::NewFolder(p.clone()), true).done, 0);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn redo_of_a_copy_copies_again() {
        let d = base("copy");
        write(&d.join("a"), "a");
        let op = Op::Copy(vec![mv(&d, "a", "a (copy)"), mv(&d, "gone", "gone (copy)")]);
        let r = apply(op, true);
        assert_eq!(read(&d.join("a (copy)")), "a");
        assert_eq!(r.op, Some(Op::Copy(vec![mv(&d, "a", "a (copy)")])));
        assert_eq!(r.errors, vec!["“gone” was changed or moved since".to_string()]);
        // Never over a copy that is there
        assert_eq!(apply(Op::Copy(vec![mv(&d, "a", "a (copy)")]), true).done, 0);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn nested_renames_reverse_for_pins() {
        let p = |s: &str| PathBuf::from(s);
        let done = vec![(p("/d/F"), p("/d/G")), (p("/d/F/s"), p("/d/F/t")), (p("/d/F/s/x"), p("/d/F/s/y"))];
        assert_eq!(
            reversed_renames(&done),
            vec![(p("/d/G"), p("/d/F")), (p("/d/G/t"), p("/d/G/s")), (p("/d/G/t/y"), p("/d/G/t/x"))]
        );
    }
}
