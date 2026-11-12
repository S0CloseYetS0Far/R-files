//! Getting items back out of the trash: "Restore", and copying trashed items into a folder.
//!
//! Trash items are `trash:///` URIs with no local path, so this goes through GIO instead of std::fs.

use crate::fileops::Outcome;
use gtk::prelude::*;
use gtk::{gio, glib};
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
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
    let name = target.file_name().unwrap_or_default();
    let (stem, ext) = if is_dir {
        (name.to_string_lossy().into_owned(), String::new())
    } else {
        let p = Path::new(name);
        (
            p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default(),
        )
    };
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
