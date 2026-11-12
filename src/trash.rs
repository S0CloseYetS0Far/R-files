//! Getting items back out of the trash: "Restore", and copying trashed items into a folder. Also moving to the
//! trash, deleting permanently and emptying the trash, all on worker threads.
//!
//! Trash items are `trash:///` URIs with no local path, so this goes through GIO instead of std::fs.

use crate::app::{App, Loc};
use crate::fileops::Outcome;
use gtk::prelude::*;
use gtk::{gio, glib};
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const NOFOLLOW: gio::FileQueryInfoFlags = gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS;

/// Where a top-level trash item came from. Items inside a trashed folder have none.
pub fn orig_path(info: &gio::FileInfo) -> Option<PathBuf> {
    use glib::translate::ToGlibPtr;
    // Read as bytes: the original path need not be UTF-8, and GString expects it to be
    let p = unsafe {
        gio::ffi::g_file_info_get_attribute_byte_string(info.to_glib_none().0, c"trash::orig-path".as_ptr())
    };
    if p.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
    let path = PathBuf::from(OsStr::from_bytes(bytes));
    path.is_absolute().then_some(path)
}

/// Restore is offered only when every selected item knows where it came from.
pub fn can_restore(origs: &[Option<PathBuf>]) -> bool {
    !origs.is_empty() && origs.iter().all(Option::is_some)
}

/// `target` if it is free, else "name (label).ext", "name (label 2).ext", … next to it. Folders keep dots in
/// their name whole.
fn free_target(target: &Path, label: &str, is_dir: bool, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    if !exists(target) {
        return target.to_path_buf();
    }
    let dir = target.parent().unwrap_or(Path::new("/"));
    let full = target.file_name().unwrap_or_default().to_string_lossy();
    // Same split as a pasted copy's name: "a.tar.gz" → "a (restored).tar.gz"
    let (stem, ext) = if is_dir { (&*full, "") } else { crate::util::split_ext(&full) };
    for i in 1.. {
        let n = if i == 1 { format!("{stem} ({label}){ext}") } else { format!("{stem} ({label} {i}){ext}") };
        let cand = dir.join(n);
        if !exists(&cand) {
            return cand;
        }
    }
    unreachable!()
}

/// Where a restored item goes: its original path, or "name (restored).ext" there when that is taken.
pub fn restore_target(orig: &Path, is_dir: bool, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    free_target(orig, "restored", is_dir, exists)
}

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

pub struct RestoreOutcome {
    pub done: usize,
    /// Names of items put back under a new name because the original one was taken
    pub renamed: Vec<String>,
    pub errors: Vec<String>,
}

/// Moves each (trash item, original path, is folder) back. Never overwrites. Runs on a worker thread.
pub fn restore(items: Vec<(gio::File, PathBuf, bool)>) -> RestoreOutcome {
    let mut out = RestoreOutcome { done: 0, renamed: Vec::new(), errors: Vec::new() };
    for (src, orig, is_dir) in items {
        let label = orig.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if let Some(parent) = orig.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                out.errors.push(format!("{label}: {e}"));
                continue;
            }
        }
        let target = restore_target(&orig, is_dir, &exists);
        // No OVERWRITE flag: if something appeared there meanwhile, this fails instead of replacing it
        let res = src.move_(
            &gio::File::for_path(&target),
            gio::FileCopyFlags::NOFOLLOW_SYMLINKS,
            None::<&gio::Cancellable>,
            None,
        );
        match res {
            Ok(()) => {
                out.done += 1;
                if target != orig {
                    out.renamed.push(target.file_name().unwrap_or_default().to_string_lossy().into_owned());
                }
            }
            Err(e) => out.errors.push(format!("{label}: {}", e.message())),
        }
    }
    out
}

/// The status bar line after a restore: how many, the first error, and any new names.
pub fn restore_message(o: &RestoreOutcome) -> String {
    let mut s = if o.errors.is_empty() {
        format!("Restored {} item(s)", o.done)
    } else {
        format!("Restored {}, {} failed — {}", o.done, o.errors.len(), o.errors[0])
    };
    match o.renamed.as_slice() {
        [] => {}
        [one] => s += &format!("; the name was taken, so it was restored as “{one}”"),
        many => s += &format!("; {} names were taken, so “(restored)” was added to them", many.len()),
    }
    s
}

/// Copies `src` (any GIO location) to `dst`, folders recursively.
fn copy_recursive(src: &gio::File, dst: &gio::File, done: &AtomicU64) -> Result<(), glib::Error> {
    let flags = gio::FileCopyFlags::NOFOLLOW_SYMLINKS | gio::FileCopyFlags::ALL_METADATA;
    if src.query_file_type(NOFOLLOW, None::<&gio::Cancellable>) == gio::FileType::Directory {
        dst.make_directory(None::<&gio::Cancellable>)?;
        let en = src.enumerate_children("standard::name", NOFOLLOW, None::<&gio::Cancellable>)?;
        while let Some(info) = en.next_file(None::<&gio::Cancellable>)? {
            copy_recursive(&src.child(info.name()), &dst.child(info.name()), done)?;
        }
        // Dates and permissions last, so creating the children doesn't change them again
        let _ = src.copy_attributes(dst, flags, None::<&gio::Cancellable>);
    } else {
        let mut last = 0i64;
        let mut cb = |cur: i64, _total: i64| {
            done.fetch_add(cur.saturating_sub(last).max(0) as u64, Ordering::Relaxed);
            last = cur;
        };
        src.copy(dst, flags, None::<&gio::Cancellable>, Some(&mut cb))?;
    }
    Ok(())
}

/// Copies non-local `sources` (such as trash items) into the local folder `dest`, with GIO. A taken name gets
/// "name (copy).ext" like a normal paste; nothing is overwritten. Runs on a worker thread.
pub fn copy_into(sources: Vec<gio::File>, dest: PathBuf, progress: Arc<AtomicU64>) -> Outcome {
    let mut out = Outcome { done: 0, errors: Vec::new() };
    for src in sources {
        let attrs = "standard::display-name,standard::type,trash::orig-path";
        let info = match src.query_info(attrs, NOFOLLOW, None::<&gio::Cancellable>) {
            Ok(i) => i,
            Err(e) => {
                out.errors.push(format!("{}: {}", src.parse_name(), e.message()));
                continue;
            }
        };
        // A top-level trash item's own name is an internal one; the real name is in its original path
        let name = orig_path(&info)
            .and_then(|p| p.file_name().map(OsStr::to_os_string))
            .or_else(|| src.basename().map(PathBuf::into_os_string))
            .unwrap_or_else(|| info.display_name().as_str().into());
        let label = name.to_string_lossy().into_owned();
        if src.path().is_some_and(|p| dest.starts_with(p)) {
            out.errors.push(format!("Can't put “{label}” inside itself"));
            continue;
        }
        let is_dir = info.file_type() == gio::FileType::Directory;
        let target = free_target(&dest.join(&name), "copy", is_dir, &exists);
        match copy_recursive(&src, &gio::File::for_path(&target), &progress) {
            Ok(()) => out.done += 1,
            Err(e) => out.errors.push(format!("{label}: {}", e.message())),
        }
    }
    out
}

#[cfg(test)]
mod restore_tests {
    use super::*;
    use std::collections::HashSet;

    fn taken(list: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = list.iter().map(PathBuf::from).collect();
        move |p: &Path| set.contains(p)
    }

    #[test]
    fn target_is_original_when_free() {
        let t = restore_target(Path::new("/home/u/a.txt"), false, &taken(&[]));
        assert_eq!(t, Path::new("/home/u/a.txt"));
    }

    #[test]
    fn taken_names_get_restored_suffix() {
        let ex = taken(&["/home/u/a.txt", "/home/u/a (restored).txt"]);
        assert_eq!(restore_target(Path::new("/home/u/a.txt"), false, &ex), Path::new("/home/u/a (restored 2).txt"));
        // folders keep their dots
        let ex = taken(&["/home/u/my.photos"]);
        let p = Path::new("/home/u/my.photos");
        assert_eq!(restore_target(p, true, &ex), Path::new("/home/u/my.photos (restored)"));
        assert_eq!(restore_target(p, false, &ex), Path::new("/home/u/my (restored).photos"));
        let ex = taken(&["/home/u/a.tar.gz"]);
        assert_eq!(restore_target(Path::new("/home/u/a.tar.gz"), false, &ex), Path::new("/home/u/a (restored).tar.gz"));
        let ex = taken(&["/home/u/.bashrc"]);
        assert_eq!(restore_target(Path::new("/home/u/.bashrc"), false, &ex), Path::new("/home/u/.bashrc (restored)"));
    }

    #[test]
    fn copy_label_matches_paste() {
        let ex = taken(&["/d/x.jpg", "/d/x (copy).jpg"]);
        assert_eq!(free_target(Path::new("/d/x.jpg"), "copy", false, &ex), Path::new("/d/x (copy 2).jpg"));
    }

    #[test]
    fn restore_status_line() {
        let o = |done, renamed: &[&str], errors: &[&str]| RestoreOutcome {
            done,
            renamed: renamed.iter().map(|s| s.to_string()).collect(),
            errors: errors.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(restore_message(&o(2, &[], &[])), "Restored 2 item(s)");
        assert_eq!(
            restore_message(&o(1, &["a (restored).txt"], &[])),
            "Restored 1 item(s); the name was taken, so it was restored as “a (restored).txt”"
        );
        assert_eq!(
            restore_message(&o(2, &["a (restored)", "b (restored)"], &["c: No space"])),
            "Restored 2, 1 failed — c: No space; 2 names were taken, so “(restored)” was added to them"
        );
    }

    #[test]
    fn restore_needs_every_original_path() {
        assert!(!can_restore(&[]));
        assert!(can_restore(&[Some("/a".into()), Some("/b".into())]));
        // an item inside a trashed folder has no original path
        assert!(!can_restore(&[Some("/a".into()), None]));
        assert!(!can_restore(&[None]));
    }

    #[test]
    fn orig_path_read_as_bytes() {
        let info = gio::FileInfo::new();
        assert_eq!(orig_path(&info), None);
        info.set_attribute_byte_string("trash::orig-path", "/home/u/a b.txt");
        assert_eq!(orig_path(&info), Some(PathBuf::from("/home/u/a b.txt")));
        info.set_attribute_byte_string("trash::orig-path", "relative");
        assert_eq!(orig_path(&info), None);
    }

    #[test]
    fn copy_into_folder_never_overwrites() {
        let root = std::env::temp_dir().join(format!("rfiles-trash-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (src, dst) = (root.join("src"), root.join("dst"));
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/f.txt"), "hi").unwrap();
        std::fs::create_dir_all(dst.join("src")).unwrap();
        let progress = Arc::new(AtomicU64::new(0));
        let out = copy_into(vec![gio::File::for_path(&src)], dst.clone(), progress.clone());
        assert_eq!((out.done, out.errors.len()), (1, 0));
        assert_eq!(std::fs::read_to_string(dst.join("src (copy)/sub/f.txt")).unwrap(), "hi");
        assert!(dst.join("src").read_dir().unwrap().next().is_none());
        assert_eq!(progress.load(Ordering::Relaxed), 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_moves_back_and_creates_parent() {
        let root = std::env::temp_dir().join(format!("rfiles-trash-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (a, b) = (root.join("a.txt"), root.join("b.txt"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        std::fs::write(root.join("taken.txt"), "old").unwrap();
        let items = vec![
            (gio::File::for_path(&a), root.join("gone/deeper/a.txt"), false),
            (gio::File::for_path(&b), root.join("taken.txt"), false),
        ];
        let out = restore(items);
        assert_eq!((out.done, out.errors.len()), (2, 0));
        assert_eq!(out.renamed, ["taken (restored).txt"]);
        assert_eq!(std::fs::read_to_string(root.join("gone/deeper/a.txt")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(root.join("taken.txt")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(root.join("taken (restored).txt")).unwrap(), "b");
        assert!(!a.exists() && !b.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}

// ================= moving to the trash and deleting =================

/// What a trash or delete job does, for its status bar lines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Job {
    Trash,
    Delete,
    EmptyTrash,
    ForgetRecent,
}

impl Job {
    fn running(self, n: usize) -> String {
        match self {
            Job::Trash => format!("Moving {n} item(s) to the trash…"),
            Job::Delete => format!("Deleting {n} item(s)…"),
            Job::EmptyTrash => "Emptying the trash…".into(),
            Job::ForgetRecent => format!("Removing {n} item(s) from Recent…"),
        }
    }

    fn failed(self) -> &'static str {
        match self {
            Job::Trash => "Couldn't move to the trash",
            Job::Delete => "Couldn't delete",
            Job::EmptyTrash => "Couldn't empty the trash",
            Job::ForgetRecent => "Couldn't remove from Recent",
        }
    }
}

/// The status bar line after a job: how many items worked, then the first error and how many more there were.
pub fn job_message(job: Job, o: &Outcome) -> String {
    let done = match job {
        Job::Trash => format!("Moved {} item(s) to the trash", o.done),
        Job::Delete => format!("Deleted {} item(s)", o.done),
        Job::EmptyTrash if o.errors.is_empty() => "Emptied the trash".into(),
        Job::EmptyTrash => format!("Deleted {} item(s) from the trash", o.done),
        Job::ForgetRecent => format!("Removed {} item(s) from Recent", o.done),
    };
    let Some(first) = o.errors.first() else { return done };
    let verb = match job {
        Job::Trash => "Couldn't move",
        Job::ForgetRecent => "Couldn't remove",
        Job::Delete | Job::EmptyTrash => "Couldn't delete",
    };
    let more = match o.errors.len() {
        1 => String::new(),
        n => format!(" (and {} more)", n - 1),
    };
    if o.done == 0 {
        format!("{verb} {first}{more}")
    } else {
        format!("{done}. {verb} {first}{more}")
    }
}

/// "“name”" for one item, "N items" for several, as the confirmation dialog names them.
pub fn what_to_delete(labels: &[&str]) -> String {
    match labels {
        [one] => format!("“{one}”"),
        many => format!("{} items", many.len()),
    }
}

pub struct TrashOutcome {
    pub out: Outcome,
    /// Items on drives without a trash (some USB sticks, network shares), with their labels
    pub unsupported: Vec<(gio::File, String)>,
}

/// Moves each (file, label) to the trash. Runs on a worker thread.
pub fn trash_all(items: Vec<(gio::File, String)>) -> TrashOutcome {
    let mut t = TrashOutcome { out: Outcome { done: 0, errors: Vec::new() }, unsupported: Vec::new() };
    for (f, label) in items {
        match f.trash(None::<&gio::Cancellable>) {
            Ok(()) => t.out.done += 1,
            Err(e) => {
                t.out.errors.push(format!("{label}: {}", e.message()));
                if e.matches(gio::IOErrorEnum::NotSupported) {
                    t.unsupported.push((f, label));
                }
            }
        }
    }
    t
}

/// Deletes `f`, and everything in it if it is a folder. Symlinks are removed, never followed. Every failure is
/// added to `errors`, named by its path under `label`. Returns whether `f` itself is gone.
fn delete_tree(f: &gio::File, label: &str, errors: &mut Vec<String>) -> bool {
    let c = None::<&gio::Cancellable>;
    // Trash and Recent items can only be deleted whole; their backends deal with what is inside
    let whole = crate::util::is_trash(f) || f.uri().starts_with("recent:");
    if !whole && f.query_file_type(NOFOLLOW, c) == gio::FileType::Directory {
        let before = errors.len();
        match f.enumerate_children("standard::name", NOFOLLOW, c) {
            Ok(en) => loop {
                match en.next_file(c) {
                    Ok(Some(info)) => {
                        let name = info.name();
                        delete_tree(&f.child(&name), &format!("{label}/{}", name.to_string_lossy()), errors);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        errors.push(format!("{label}: {}", e.message()));
                        break;
                    }
                }
            },
            Err(e) => errors.push(format!("{label}: {}", e.message())),
        }
        if errors.len() > before {
            // Something inside is still there, so deleting the folder would only add a "not empty" error
            return false;
        }
    }
    match f.delete(c) {
        Ok(()) => true,
        Err(e) => {
            errors.push(format!("{label}: {}", e.message()));
            false
        }
    }
}

/// Permanently deletes each (file, label), going on past failures. Runs on a worker thread.
pub fn delete_all(items: Vec<(gio::File, String)>) -> Outcome {
    let mut out = Outcome { done: 0, errors: Vec::new() };
    for (f, label) in items {
        if delete_tree(&f, &label, &mut out.errors) {
            out.done += 1;
        }
    }
    out
}

/// Permanently deletes everything in the trash. Runs on a worker thread.
fn empty_all() -> Outcome {
    let trash = gio::File::for_uri("trash:///");
    let c = None::<&gio::Cancellable>;
    let mut items = Vec::new();
    // List first, then delete, so the listing doesn't change under the enumerator
    let listed = trash.enumerate_children("standard::name,standard::display-name", NOFOLLOW, c).and_then(|en| {
        while let Some(info) = en.next_file(c)? {
            items.push((trash.child(info.name()), info.display_name().to_string()));
        }
        Ok(())
    });
    let mut out = delete_all(items);
    if let Err(e) = listed {
        out.errors.insert(0, format!("Trash: {}", e.message()));
    }
    out
}

/// Runs `work` on a worker thread, saying so in the status bar, then hands its result to `then`. The window is
/// held weakly; the application is kept running until the work is done, so closing the window doesn't cut it
/// short.
fn spawn_job<T: Send + 'static>(
    app: &Rc<App>,
    job: Job,
    n: usize,
    work: impl FnOnce() -> T + Send + 'static,
    then: impl FnOnce(&Rc<App>, T) + 'static,
) {
    app.status.set_text(&job.running(n));
    let hold = app.window.application().map(|g| g.hold());
    let weak = Rc::downgrade(app);
    glib::spawn_future_local(async move {
        let res = gio::spawn_blocking(work).await;
        drop(hold);
        let Some(app) = weak.upgrade() else { return };
        match res {
            Ok(v) => then(&app, v),
            Err(_) => app.status.set_text(job.failed()),
        }
        // Folder views update themselves; Starred, search results, Recent and the trash may not
        let watched = match app.current_loc() {
            Loc::Dir(d) => !crate::util::is_trash(&d) && !d.uri().starts_with("recent:"),
            _ => false,
        };
        if !watched {
            app.reload();
        }
    });
}

/// Moves (file, label) items to the trash, offering to delete permanently the ones whose drive has no trash.
pub fn move_to_trash(app: &Rc<App>, items: Vec<(gio::File, String)>) {
    if items.is_empty() {
        return;
    }
    spawn_job(app, Job::Trash, items.len(), move || trash_all(items), |app, t| {
        app.status.set_text(&job_message(Job::Trash, &t.out));
        if t.unsupported.is_empty() {
            return;
        }
        let dlg = gtk::AlertDialog::builder()
            .message(format!("{} item(s) can't be moved to the trash. Delete permanently?", t.unsupported.len()))
            .detail("This can't be undone.")
            .buttons(["Cancel", "Delete"])
            .cancel_button(0)
            .default_button(0)
            .build();
        let weak = Rc::downgrade(app);
        let items = t.unsupported;
        dlg.choose(Some(&app.window), None::<&gio::Cancellable>, move |r| {
            if let (Ok(1), Some(app)) = (r, weak.upgrade()) {
                delete_files(&app, items);
            }
        });
    });
}

/// Asks, then permanently deletes (file, label) items.
pub fn confirm_delete(app: &Rc<App>, items: Vec<(gio::File, String)>) {
    if items.is_empty() {
        return;
    }
    let labels: Vec<&str> = items.iter().map(|(_, l)| l.as_str()).collect();
    let dlg = gtk::AlertDialog::builder()
        .message(format!("Permanently delete {}?", what_to_delete(&labels)))
        .detail("This can't be undone.")
        .buttons(["Cancel", "Delete"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let weak = Rc::downgrade(app);
    dlg.choose(Some(&app.window), None::<&gio::Cancellable>, move |r| {
        if let (Ok(1), Some(app)) = (r, weak.upgrade()) {
            delete_files(&app, items);
        }
    });
}

fn delete_files(app: &Rc<App>, items: Vec<(gio::File, String)>) {
    spawn_job(app, Job::Delete, items.len(), move || delete_all(items), |app, o| {
        app.status.set_text(&job_message(Job::Delete, &o));
    });
}

/// Asks, then permanently deletes everything in the trash.
pub fn empty_trash(app: &Rc<App>) {
    let dlg = gtk::AlertDialog::builder()
        .message("Empty the trash?")
        .detail("Everything in the trash will be permanently deleted.")
        .buttons(["Cancel", "Empty Trash"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let weak = Rc::downgrade(app);
    dlg.choose(Some(&app.window), None::<&gio::Cancellable>, move |r| {
        if let (Ok(1), Some(app)) = (r, weak.upgrade()) {
            spawn_job(&app, Job::EmptyTrash, 0, empty_all, |app, o| {
                app.status.set_text(&job_message(Job::EmptyTrash, &o));
            });
        }
    });
}

/// Removes recent:/// entries from Recent; the files themselves stay.
pub fn forget_recent(app: &Rc<App>, items: Vec<(gio::File, String)>) {
    if items.is_empty() {
        return;
    }
    spawn_job(app, Job::ForgetRecent, items.len(), move || delete_all(items), |app, o| {
        app.status.set_text(&job_message(Job::ForgetRecent, &o));
    });
}

#[cfg(test)]
mod delete_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rfiles-delete-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn item(p: &Path) -> (gio::File, String) {
        (gio::File::for_path(p), p.file_name().unwrap().to_string_lossy().into_owned())
    }

    #[test]
    fn delete_does_not_follow_symlinks_out() {
        let root = tmp("symlink");
        let (outside, dir) = (root.join("outside"), root.join("dir"));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "keep").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/f.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link-dir")).unwrap();
        std::os::unix::fs::symlink(outside.join("keep.txt"), dir.join("sub/link-file")).unwrap();
        let out = delete_all(vec![item(&dir)]);
        assert_eq!((out.done, out.errors.len()), (1, 0), "{:?}", out.errors);
        assert!(dir.symlink_metadata().is_err());
        assert_eq!(std::fs::read_to_string(outside.join("keep.txt")).unwrap(), "keep");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_reports_every_failure_and_goes_on() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("partial");
        let (dir, locked, other) = (root.join("dir"), root.join("dir/locked"), root.join("other.txt"));
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("a"), "").unwrap();
        std::fs::write(locked.join("b"), "").unwrap();
        std::fs::write(dir.join("free.txt"), "").unwrap();
        std::fs::write(&other, "").unwrap();
        // Read-only folder: the files in it can't be unlinked
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let enforced = std::fs::File::create(locked.join("probe")).is_err();
        let out = delete_all(vec![item(&dir), item(&root.join("missing")), item(&other)]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!other.exists() && !dir.join("free.txt").exists());
        assert!(out.errors.iter().any(|e| e.starts_with("missing: ")), "{:?}", out.errors);
        // Root ignores the permission, so the rest is only checked where it holds
        if enforced {
            assert_eq!(out.done, 1);
            assert!(out.errors.iter().any(|e| e.starts_with("dir/locked/a: ")), "{:?}", out.errors);
            assert!(out.errors.iter().any(|e| e.starts_with("dir/locked/b: ")), "{:?}", out.errors);
            // No extra "not empty" errors for the folders still holding them
            assert_eq!(out.errors.len(), 3, "{:?}", out.errors);
            assert!(locked.join("a").exists());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn status_line_counts_errors() {
        let o = |done, errors: &[&str]| Outcome { done, errors: errors.iter().map(|s| s.to_string()).collect() };
        assert_eq!(job_message(Job::Trash, &o(3, &[])), "Moved 3 item(s) to the trash");
        assert_eq!(
            job_message(Job::Trash, &o(2, &["a: Permission denied", "b: x", "c: y"])),
            "Moved 2 item(s) to the trash. Couldn't move a: Permission denied (and 2 more)"
        );
        assert_eq!(job_message(Job::Delete, &o(0, &["a: Busy"])), "Couldn't delete a: Busy");
        assert_eq!(job_message(Job::EmptyTrash, &o(4, &[])), "Emptied the trash");
        assert_eq!(
            job_message(Job::EmptyTrash, &o(4, &["x/y: No", "z: No"])),
            "Deleted 4 item(s) from the trash. Couldn't delete x/y: No (and 1 more)"
        );
        assert_eq!(job_message(Job::ForgetRecent, &o(1, &[])), "Removed 1 item(s) from Recent");
    }

    #[test]
    fn dialog_names_one_item_or_counts() {
        assert_eq!(what_to_delete(&["a.txt"]), "“a.txt”");
        assert_eq!(what_to_delete(&["a", "b"]), "2 items");
    }

    #[test]
    fn trash_reports_failures_by_name() {
        let root = tmp("trash");
        let t = trash_all(vec![item(&root.join("missing"))]);
        assert_eq!(t.out.done, 0);
        assert_eq!(t.out.errors.len(), 1);
        assert!(t.out.errors[0].starts_with("missing: "), "{:?}", t.out.errors);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
