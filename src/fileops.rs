//! Copy / cut / paste, and "Open in Terminal".
//!
//! The clipboard uses the same format as GNOME Files (Nautilus), so you can
//! copy in one app and paste in the other.

use crate::app::{App, Loc};
use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const GNOME_MIME: &str = "x-special/gnome-copied-files";
const URI_LIST: &str = "text/uri-list";

pub fn set_clipboard(clipboard: &gdk::Clipboard, files: &[gio::File], cut: bool) {
    let [gnome, list, text] = clipboard_texts(files, cut);
    let provider = gdk::ContentProvider::new_union(&[
        gdk::ContentProvider::for_bytes(GNOME_MIME, &glib::Bytes::from_owned(gnome.into_bytes())),
        gdk::ContentProvider::for_bytes(URI_LIST, &glib::Bytes::from_owned(list.into_bytes())),
        gdk::ContentProvider::for_bytes(
            "text/plain;charset=utf-8",
            &glib::Bytes::from_owned(text.into_bytes()),
        ),
    ]);
    let _ = clipboard.set_content(Some(&provider));
}

/// The GNOME, uri-list and plain-text forms `set_clipboard` offers.
fn clipboard_texts(files: &[gio::File], cut: bool) -> [String; 3] {
    let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let list = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
    let text = files
        .iter()
        .map(|f| f.parse_name().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    [gnome, list, text]
}

/// Returns (is_cut, files) from the clipboard, or None if it holds no files.
pub async fn read_clipboard(clipboard: &gdk::Clipboard) -> Option<(bool, Vec<gio::File>)> {
    let (stream, mime) = clipboard
        .read_future(&[GNOME_MIME, URI_LIST], glib::Priority::DEFAULT)
        .await
        .ok()?;
    let mut data = Vec::new();
    loop {
        let chunk = stream
            .read_bytes_future(64 * 1024, glib::Priority::DEFAULT)
            .await
            .ok()?;
        if chunk.is_empty() {
            break;
        }
        data.extend_from_slice(&chunk);
    }
    parse_clipboard(&data, mime == GNOME_MIME)
}

/// Reads what `read_clipboard` got: the GNOME form when `gnome`, else a uri-list.
fn parse_clipboard(data: &[u8], gnome: bool) -> Option<(bool, Vec<gio::File>)> {
    let text = String::from_utf8_lossy(data);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    let cut = if gnome {
        lines.next() == Some("cut")
    } else {
        false
    };
    let files: Vec<gio::File> = lines.map(gio::File::for_uri).collect();
    if files.is_empty() {
        None
    } else {
        Some((cut, files))
    }
}

#[cfg(test)]
mod clipboard_format_tests {
    use super::{clipboard_texts, parse_clipboard};
    use gtk::gio;
    use gtk::prelude::*;

    fn uris(files: &[gio::File]) -> Vec<String> {
        files.iter().map(|f| f.uri().to_string()).collect()
    }

    #[test]
    fn texts_in_every_format() {
        let files = [gio::File::for_path("/tmp/a b"), gio::File::for_path("/tmp/é")];
        let [gnome, list, text] = clipboard_texts(&files, false);
        assert_eq!(gnome, "copy\nfile:///tmp/a%20b\nfile:///tmp/%C3%A9");
        assert_eq!(list, "file:///tmp/a%20b\r\nfile:///tmp/%C3%A9\r\n");
        assert_eq!(text, "/tmp/a b\n/tmp/é");
        assert!(clipboard_texts(&files, true)[0].starts_with("cut\n"));
    }

    #[test]
    fn what_is_written_reads_back() {
        let files = [gio::File::for_path("/tmp/a b"), gio::File::for_path("/tmp/é")];
        for cut in [false, true] {
            let [gnome, list, _] = clipboard_texts(&files, cut);
            let (c, got) = parse_clipboard(gnome.as_bytes(), true).unwrap();
            assert_eq!((c, uris(&got)), (cut, uris(&files)));
            // A uri-list never means cut.
            let (c, got) = parse_clipboard(list.as_bytes(), false).unwrap();
            assert_eq!((c, uris(&got)), (false, uris(&files)));
        }
    }

    #[test]
    fn other_apps_and_empty_clipboards() {
        // Comments, blank lines and stray spaces in a uri-list are skipped.
        let (cut, got) = parse_clipboard(b"# from a browser\r\n\r\n  file:///tmp/x  \r\n", false).unwrap();
        assert_eq!((cut, uris(&got)), (false, vec!["file:///tmp/x".to_string()]));
        // In a uri-list the first line is a file, not the copy/cut word.
        assert_eq!(parse_clipboard(b"file:///a\nfile:///b", false).unwrap().1.len(), 2);
        assert!(parse_clipboard(b"", true).is_none());
        assert!(parse_clipboard(b"cut\n", true).is_none());
        assert!(parse_clipboard(b"# only a comment\n", false).is_none());
    }
}

/// Picks a free name in `dir`: "photo.jpg" → "photo (copy).jpg" → "photo (copy 2).jpg";
/// "a.tar.gz" → "a (copy).tar.gz". Folders get the marker at the end.
fn free_name(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let first = dir.join(name);
    if first.symlink_metadata().is_err() {
        return first;
    }
    // Non-UTF-8 names go through lossy conversion, as before: invalid bytes become U+FFFD in the copy's name.
    let full = name.to_string_lossy();
    let (stem, ext) = if first.is_dir() { (&*full, "") } else { crate::util::split_ext(&full) };
    for i in 1.. {
        let n = if i == 1 { format!("{stem} (copy){ext}") } else { format!("{stem} (copy {i}){ext}") };
        let cand = dir.join(n);
        if cand.symlink_metadata().is_err() {
            return cand;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod free_name_tests {
    use super::free_name;
    use std::ffi::OsStr;

    fn touch(d: &std::path::Path, n: &str) {
        std::fs::write(d.join(n), b"").unwrap();
    }

    fn name(d: &std::path::Path, n: &str) -> String {
        free_name(d, OsStr::new(n)).file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn picks_names() {
        let d = std::env::temp_dir().join(format!("rfiles-free-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(name(&d, "new.txt"), "new.txt");
        for n in ["a.txt", "archive.tar.gz", "noext", ".bashrc"] {
            touch(&d, n);
        }
        std::fs::create_dir(d.join("my.folder.d")).unwrap();
        assert_eq!(name(&d, "a.txt"), "a (copy).txt");
        assert_eq!(name(&d, "archive.tar.gz"), "archive (copy).tar.gz");
        assert_eq!(name(&d, "noext"), "noext (copy)");
        assert_eq!(name(&d, ".bashrc"), ".bashrc (copy)");
        assert_eq!(name(&d, "my.folder.d"), "my.folder.d (copy)");
        touch(&d, "archive (copy).tar.gz");
        assert_eq!(name(&d, "archive.tar.gz"), "archive (copy 2).tar.gz");
        touch(&d, "archive (copy 2).tar.gz");
        assert_eq!(name(&d, "archive.tar.gz"), "archive (copy 3).tar.gz");
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// Shared between a copy / move job's worker and its progress row.
#[derive(Default)]
pub struct CopyProgress {
    /// Bytes copied so far
    pub done: Arc<AtomicU64>,
    /// Bytes to copy, once `counted` is set; 0 when unknown
    pub total: AtomicU64,
    pub counted: AtomicBool,
    /// Set by the Cancel button
    pub cancel: AtomicBool,
    /// Set by the worker when it stopped early because of `cancel`
    pub stopped: AtomicBool,
    /// Tests: acts as if Cancel was pressed once this many bytes are done (0: never)
    #[cfg(test)]
    cancel_at: AtomicU64,
}

impl CopyProgress {
    fn check(&self) -> io::Result<()> {
        #[cfg(test)]
        {
            let at = self.cancel_at.load(Ordering::Relaxed);
            if at > 0 && self.done.load(Ordering::Relaxed) >= at {
                self.cancel.store(true, Ordering::Relaxed);
            }
        }
        if self.cancel.load(Ordering::Relaxed) {
            Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
        } else {
            Ok(())
        }
    }
}

/// Bytes of regular files under `p` (symlinks not followed). Unreadable parts count as 0; stops early on cancel.
fn tree_size(p: &Path, progress: &CopyProgress) -> u64 {
    let Ok(md) = p.symlink_metadata() else { return 0 };
    if md.is_file() {
        return md.len();
    }
    if !md.is_dir() {
        return 0;
    }
    let Ok(rd) = std::fs::read_dir(p) else { return 0 };
    let mut sum = 0;
    for e in rd.flatten() {
        if progress.check().is_err() {
            break;
        }
        sum += tree_size(&e.path(), progress);
    }
    sum
}

/// Read / write size per step: big enough to be fast, small enough that progress moves and Cancel is quick.
const CHUNK: u64 = 4 << 20;

/// Copies one regular file in `chunk`-byte steps, adding to `progress.done` and checking for cancel between steps.
/// Never overwrites: the copy is created with `create_new`. On any failure, cancel included, the partly written
/// copy is removed.
fn copy_file_chunked(
    src: &Path,
    dst: &Path,
    md: &std::fs::Metadata,
    progress: &CopyProgress,
    chunk: u64,
) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut from = std::fs::File::open(src)?;
    // Private until the copy is complete; the original's mode is set at the end, as `std::fs::copy` does
    let mut to = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(dst)?;
    let res = (|| {
        loop {
            progress.check()?;
            // `io::copy` between files uses copy_file_range where it can, so this stays fast
            let n = io::copy(&mut (&mut from).take(chunk), &mut to)?;
            progress.done.fetch_add(n, Ordering::Relaxed);
            // A short step is the end of the file; stopping here means Cancel can't drop a finished copy
            if n < chunk {
                break;
            }
        }
        to.set_permissions(md.permissions())
    })();
    drop(to);
    if res.is_err() {
        let _ = std::fs::remove_file(dst);
    }
    res
}

/// Copies `src` to `dst`. Pipes, sockets and devices are left out and counted in `skipped`: reading a
/// pipe blocks until something writes to it, and a device like /dev/zero never ends.
pub(crate) fn copy_recursive(src: &Path, dst: &Path, progress: &CopyProgress, skipped: &mut usize) -> io::Result<()> {
    progress.check()?;
    let md = src.symlink_metadata()?;
    let ft = md.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
    } else if ft.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()), progress, skipped)?;
        }
        let _ = std::fs::set_permissions(dst, md.permissions());
        // Last, so creating the children doesn't bump the folder's date again
        keep_times(dst, &md);
    } else if ft.is_file() {
        copy_file_chunked(src, dst, &md, progress, CHUNK)?;
        keep_times(dst, &md);
    } else {
        *skipped += 1;
    }
    Ok(())
}

#[cfg(test)]
mod chunked_copy_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    fn base(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rfiles-chunked-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn copy_equals_source_around_chunk_boundary() {
        let d = base("sizes");
        let c = 4096u64;
        for len in [0, 1, c - 1, c, c + 1, 3 * c + 7] {
            let (src, dst) = (d.join(format!("s{len}")), d.join(format!("d{len}")));
            std::fs::write(&src, bytes(len as usize)).unwrap();
            let p = CopyProgress::default();
            copy_file_chunked(&src, &dst, &src.metadata().unwrap(), &p, c).unwrap();
            assert_eq!(std::fs::read(&dst).unwrap(), bytes(len as usize), "size {len}");
            assert_eq!(p.done.load(Ordering::Relaxed), len);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keeps_permissions_times_and_symlinks() {
        let d = base("meta");
        let (src, dest) = (d.join("src"), d.join("dest"));
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let (f, ro) = (src.join("sub/script.sh"), src.join("ro.txt"));
        std::fs::write(&f, bytes(CHUNK as usize + 1)).unwrap();
        std::fs::write(&ro, b"ro").unwrap();
        std::os::unix::fs::symlink("sub/script.sh", src.join("link")).unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::new(1_000_000_000, 5);
        std::fs::File::open(&f).unwrap().set_modified(old).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o750)).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(src.join("sub"), std::fs::Permissions::from_mode(0o711)).unwrap();

        let out = transfer(vec![src.clone()], dest.clone(), false, &CopyProgress::default());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let copy = dest.join("src");
        let mode = |p: &Path| p.symlink_metadata().unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&copy.join("sub/script.sh")), 0o750);
        assert_eq!(mode(&copy.join("ro.txt")), 0o444);
        assert_eq!(mode(&copy.join("sub")), 0o711);
        assert_eq!(copy.join("sub/script.sh").metadata().unwrap().modified().unwrap(), old);
        assert_eq!(std::fs::read(copy.join("sub/script.sh")).unwrap(), bytes(CHUNK as usize + 1));
        assert!(copy.join("link").symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(copy.join("link")).unwrap(), Path::new("sub/script.sh"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn never_overwrites_an_existing_file() {
        let d = base("exists");
        let (src, dst) = (d.join("a.txt"), d.join("b.txt"));
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        let err = copy_file_chunked(&src, &dst, &src.metadata().unwrap(), &CopyProgress::default(), CHUNK);
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
        // Pasting next to the original picks a new name and leaves the original alone
        let out = transfer(vec![src.clone()], d.clone(), false, &CopyProgress::default());
        assert_eq!((out.done, out.errors.len()), (1, 0));
        assert_eq!(std::fs::read(&src).unwrap(), b"new");
        assert_eq!(std::fs::read(d.join("a (copy).txt")).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The error line for an item that was copied without some special files.
fn skipped_msg(label: &str, skipped: usize, top_level: bool, cut: bool) -> String {
    if top_level {
        return format!("{label}: is a pipe, socket or device, not copied");
    }
    let what = if skipped == 1 {
        "1 pipe, socket or device".to_string()
    } else {
        format!("{skipped} pipes, sockets or devices")
    };
    let kept = if cut { "; the original was kept" } else { "" };
    format!("{label}: copied without {what}{kept}")
}

/// Gives the copy the original's modified and accessed dates, like `cp -a`. Best effort: some drives
/// (FAT, network shares) can't store them exactly, and that shouldn't fail the copy. Symlinks keep
/// the time they were made, since std has no way to set a link's own times.
fn keep_times(dst: &Path, md: &std::fs::Metadata) {
    let mut times = std::fs::FileTimes::new();
    if let Ok(t) = md.modified() {
        times = times.set_modified(t);
    }
    if let Ok(t) = md.accessed() {
        times = times.set_accessed(t);
    }
    // Read-only open works for folders and for copies of read-only files; the owner may set times
    if let Ok(f) = std::fs::File::open(dst) {
        let _ = f.set_times(times);
    }
}

#[derive(Default)]
pub struct Outcome {
    pub done: usize,
    pub errors: Vec<String>,
    /// One entry per completed top-level item (those counted in `done`), so the operation can be undone
    pub items: Vec<Transferred>,
    /// Clashing items left where they are because the user chose Skip
    pub skipped: usize,
    /// Items of a move that already were in the destination folder
    pub already: usize,
}

/// A top-level item a copy or move completed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transferred {
    pub source: PathBuf,
    /// Where it ended up: the same name, a "name (copy)" one, or the name of the item it replaced
    pub target: PathBuf,
    /// True when an item already at `target` was replaced (and is gone)
    pub replaced: bool,
}

/// What to do with an item whose name is already taken in the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clash {
    /// Give the new one a free name, "name (copy).ext"
    KeepBoth,
    Replace,
    Skip,
}

fn same_item(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.symlink_metadata(), b.symlink_metadata()) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// True when `target` is taken by something other than `src` itself (pasting next to the original isn't a clash).
fn clashes(src: &Path, target: &Path) -> bool {
    target.symlink_metadata().is_ok() && !same_item(src, target)
}

/// A free hidden name in `dir` for a half-done copy or replace. Short, so a long item name can't make it too long.
/// The counter is shared by all jobs, so two jobs running at once into the same folder never pick the same name.
/// Old leftovers (from a crash) are skipped, never deleted: one may hold the only copy of something.
fn temp_name(dir: &Path, tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let i = NEXT.fetch_add(1, Ordering::Relaxed);
        let p = dir.join(format!(".rfiles-{tag}-{}-{i}", std::process::id()));
        if p.symlink_metadata().is_err() {
            return p;
        }
    }
}

pub(crate) fn remove_any(p: &Path) -> io::Result<()> {
    match p.symlink_metadata() {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes `tmp`, a temporary this job created, after `e` stopped the copy into it; returns the error to report.
/// Folders in it are made writable first: a copied read-only folder can't be emptied otherwise. Symlinks are
/// neither followed nor changed. If it can't all be removed, the error says where the rest is.
fn remove_temp(tmp: &Path, e: io::Error) -> io::Error {
    fn open_up(p: &Path) {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // O_NOFOLLOW | O_DIRECTORY: only ever a real folder, so a symlink can't send the chmod elsewhere
        let flags = libc::O_NOFOLLOW | libc::O_DIRECTORY;
        let Ok(f) = std::fs::OpenOptions::new().read(true).custom_flags(flags).open(p) else {
            // Not a folder, or one we can't even open (mode 000): make it readable through its path
            if p.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
                if std::fs::OpenOptions::new().read(true).custom_flags(flags).open(p).is_ok() {
                    open_up(p);
                }
            }
            return;
        };
        if f.metadata().is_ok_and(|m| m.permissions().mode() & 0o700 != 0o700) {
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o700));
        }
        if let Ok(rd) = std::fs::read_dir(p) {
            for c in rd.flatten() {
                if c.file_type().is_ok_and(|t| t.is_dir()) {
                    open_up(&c.path());
                }
            }
        }
    }
    open_up(tmp);
    // `remove_dir_all` doesn't follow symlinks; it removes the link, not what it points at
    match remove_any(tmp) {
        Ok(()) => e,
        Err(r) => {
            let at = tmp.display();
            io::Error::other(format!("{e}; the partial copy couldn't be deleted ({r}), it is at {at}"))
        }
    }
}

/// Copies `src` to `target` through a new hidden temporary next to it, so `target` only ever appears complete: a
/// failure or Cancel part-way removes the temporary and leaves nothing under `target`, and after a crash only a hidden
/// `.rfiles-new-…` item can remain. With `replacing`, the item at `target` is swapped out once the copy is done
/// (and kept if something had to be left out); otherwise `target` must be free and is never overwritten.
/// When `src` itself is a pipe, socket or device nothing is made; without `replacing` that is Ok, see `skipped`.
fn copy_into_place(
    src: &Path,
    target: &Path,
    replacing: bool,
    progress: &CopyProgress,
    skipped: &mut usize,
) -> io::Result<()> {
    let tmp = temp_name(target.parent().unwrap_or(Path::new(".")), "new");
    if let Err(e) = copy_recursive(src, &tmp, progress, skipped) {
        // Everything below `tmp` is in a folder this job just made, so "already exists" can only be `tmp` itself:
        // someone else's (`temp_name`'s counter makes that practically impossible), so it is never deleted
        return Err(if e.kind() == io::ErrorKind::AlreadyExists { e } else { remove_temp(&tmp, e) });
    }
    let res = (|| {
        let Ok(md) = tmp.symlink_metadata() else {
            return if replacing { Err(io::Error::other("is a pipe, socket or device, not copied")) } else { Ok(()) };
        };
        if !replacing {
            return crate::undo::rename_noreplace(&tmp, target);
        }
        if *skipped > 0 {
            return Err(io::Error::other("holds pipes, sockets or devices that can't be copied; not replaced"));
        }
        if md.is_dir() { swap_dir(&tmp, target) } else { std::fs::rename(&tmp, target) }
    })();
    // After a successful rename `tmp` is gone, so a later error (the old folder not deleted) removes nothing
    res.map_err(|e| remove_temp(&tmp, e))
}

#[cfg(test)]
mod partial_copy_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    fn base(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rfiles-partial-{tag}-{}", std::process::id()));
        unlock(&d);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("dest")).unwrap();
        d
    }

    fn write(p: &Path, data: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    fn chmod(p: &Path, m: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }

    /// Makes a test tree removable again, whatever a failed test left locked
    fn unlock(p: &Path) {
        if p.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                unlock(&e.path());
            }
        }
    }

    fn done(d: &Path) {
        unlock(d);
        let _ = std::fs::remove_dir_all(d);
    }

    /// The names in `dir`, sorted
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    /// Root reads anything, so a locked file can't make a copy fail
    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn failed_folder_copy_leaves_nothing_behind() {
        if is_root() {
            return;
        }
        let d = base("fail");
        let (src, dest) = (d.join("src/f"), d.join("dest"));
        write(&src.join("a.txt"), b"a");
        write(&src.join("deep/er/ok.txt"), b"ok");
        write(&src.join("deep/er/secret"), b"s");
        chmod(&src.join("deep/er/secret"), 0o000);
        // Similar names already there, a crash leftover among them: none may be touched
        write(&dest.join("f/old.txt"), b"old");
        write(&dest.join("f (copy) other"), b"x");
        write(&dest.join(".rfiles-new-1-0/only-copy"), b"keep");
        let before = names(&dest);

        // Twice: the first failure mustn't leave anything that changes the second
        for _ in 0..2 {
            let out = transfer(vec![src.clone()], dest.clone(), false, &CopyProgress::default());
            assert_eq!((out.done, out.errors.len()), (0, 1), "{:?}", out.errors);
            assert!(out.errors[0].starts_with("f: "), "{:?}", out.errors);
            assert!(out.items.is_empty());
            assert_eq!(names(&dest), before);
        }
        assert_eq!(std::fs::read(dest.join("f/old.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read(dest.join(".rfiles-new-1-0/only-copy")).unwrap(), b"keep");
        assert_eq!(std::fs::read(src.join("a.txt")).unwrap(), b"a");
        done(&d);
    }

    #[test]
    fn cancel_mid_folder_leaves_nothing_behind() {
        let d = base("cancel");
        let (src, dest) = (d.join("src/f"), d.join("dest"));
        let data: Vec<u8> = (0..2 * CHUNK as usize + 5).map(|i| i as u8).collect();
        write(&src.join("a.txt"), b"a");
        write(&src.join("ro/b.txt"), b"b");
        write(&src.join("big.bin"), &data);
        chmod(&src.join("ro"), 0o555);
        // Past the small files, whichever order the folder lists them in, but inside big.bin
        let p = CopyProgress { cancel_at: AtomicU64::new(3), ..Default::default() };
        let out = transfer(vec![src.clone()], dest.clone(), false, &p);
        assert!(p.stopped.load(Ordering::Relaxed));
        assert_eq!((out.done, out.errors.len()), (0, 0), "{:?}", out.errors);
        assert!(names(&dest).is_empty(), "{:?}", names(&dest));
        assert_eq!(std::fs::read(src.join("big.bin")).unwrap(), data);
        done(&d);
    }

    #[test]
    fn cleanup_opens_locked_folders_and_never_follows_links() {
        let d = base("cleanup");
        let outside = d.join("src/outside");
        write(&outside.join("keep.txt"), b"keep");
        chmod(&outside, 0o555);
        let tmp = d.join("dest/.rfiles-new-test");
        write(&tmp.join("ro/sub/file"), b"x");
        write(&tmp.join("locked/file"), b"x");
        std::os::unix::fs::symlink(&outside, tmp.join("ro/link")).unwrap();
        std::os::unix::fs::symlink(outside.join("keep.txt"), tmp.join("file-link")).unwrap();
        chmod(&tmp.join("ro/sub"), 0o555);
        chmod(&tmp.join("ro"), 0o500);
        chmod(&tmp.join("locked"), 0o000);

        let e = remove_temp(&tmp, io::Error::other("disk full"));
        assert_eq!(e.to_string(), "disk full");
        assert!(tmp.symlink_metadata().is_err());
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(outside.metadata().unwrap().permissions().mode() & 0o777, 0o555);
        // Nothing there: nothing to do
        assert_eq!(remove_temp(&tmp, io::Error::other("x")).to_string(), "x");
        done(&d);
    }

    #[test]
    fn leftover_that_cant_be_deleted_is_reported() {
        if is_root() {
            return;
        }
        let d = base("leftover");
        let dest = d.join("dest");
        let tmp = dest.join(".rfiles-new-test");
        write(&tmp.join("file"), b"x");
        // The destination itself turned read-only: the temporary can't go, so the message says where it is
        chmod(&dest, 0o555);
        let e = remove_temp(&tmp, io::Error::other("disk full"));
        chmod(&dest, 0o755);
        assert!(e.to_string().starts_with("disk full; the partial copy couldn't be deleted ("), "{e}");
        assert!(e.to_string().ends_with(&format!("it is at {}", tmp.display())), "{e}");
        done(&d);
    }

    #[test]
    fn complete_copy_is_the_same_as_before() {
        let d = base("ok");
        let (src, dest) = (d.join("src/f"), d.join("dest"));
        write(&src.join("sub/run.sh"), b"#!/bin/sh");
        std::os::unix::fs::symlink("sub/run.sh", src.join("link")).unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::new(1_000_000_000, 0);
        std::fs::File::open(src.join("sub/run.sh")).unwrap().set_modified(old).unwrap();
        chmod(&src.join("sub/run.sh"), 0o750);
        chmod(&src.join("sub"), 0o555);
        write(&dest.join("f/old.txt"), b"old");

        let out = transfer(vec![src.clone()], dest.clone(), false, &CopyProgress::default());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let copy = dest.join("f (copy)");
        assert_eq!(out.items, vec![Transferred { source: src.clone(), target: copy.clone(), replaced: false }]);
        assert_eq!(names(&dest), ["f", "f (copy)"]);
        let mode = |p: &Path| p.symlink_metadata().unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&copy.join("sub/run.sh")), 0o750);
        assert_eq!(mode(&copy.join("sub")), 0o555);
        assert_eq!(copy.join("sub/run.sh").metadata().unwrap().modified().unwrap(), old);
        assert_eq!(std::fs::read_link(copy.join("link")).unwrap(), Path::new("sub/run.sh"));

        // Replace takes the same way and ends at the same name
        let out = transfer_with(vec![src.clone()], dest.clone(), false, Clash::Replace, &CopyProgress::default());
        assert_eq!(out.items, vec![Transferred { source: src.clone(), target: dest.join("f"), replaced: true }]);
        assert_eq!(names(&dest), ["f", "f (copy)"]);
        assert_eq!(mode(&dest.join("f/sub")), 0o555);
        done(&d);
    }

    #[test]
    fn special_files_are_left_out_and_the_rest_kept() {
        let d = base("special");
        let (src, dest) = (d.join("src/f"), d.join("dest"));
        write(&src.join("a.txt"), b"a");
        let ok = std::process::Command::new("mkfifo").arg(src.join("pipe")).status().unwrap().success();
        assert!(ok, "mkfifo failed");
        let out = transfer(vec![src.clone(), src.join("pipe")], dest.clone(), false, &CopyProgress::default());
        assert_eq!(out.done, 0);
        let errors = ["f: copied without 1 pipe, socket or device", "pipe: is a pipe, socket or device, not copied"];
        assert_eq!(out.errors, errors);
        assert!(out.items.is_empty());
        assert_eq!(names(&dest), ["f"]);
        assert_eq!(names(&dest.join("f")), ["a.txt"]);
        done(&d);
    }
}

/// Puts folder `new` at `target` in place of the folder there: the old one is moved aside, the new one moved in,
/// and only then is the old one deleted. If moving in fails, the old one is put back.
fn swap_dir(new: &Path, target: &Path) -> io::Result<()> {
    let aside = temp_name(target.parent().unwrap_or(Path::new(".")), "old");
    std::fs::rename(target, &aside)?;
    if let Err(e) = std::fs::rename(new, target) {
        let _ = std::fs::rename(&aside, target);
        return Err(e);
    }
    std::fs::remove_dir_all(&aside).map_err(|e| {
        let at = aside.display();
        io::Error::other(format!("replaced, but the old folder couldn't be deleted ({e}); it is at {at}"))
    })
}

/// Puts `src` at `target` in place of the item there. The new item is complete before the old one goes, so a failure
/// or Cancel part-way leaves the old item untouched and no temporary behind. A move on the same drive is a rename
/// (atomic for files); otherwise the item is copied to a temporary name next to `target` and then swapped in.
fn replace(src: &Path, target: &Path, cut: bool, progress: &CopyProgress, skipped: &mut usize) -> io::Result<()> {
    let new_dir = src.symlink_metadata()?.is_dir();
    let old_dir = target.symlink_metadata()?.is_dir();
    if new_dir != old_dir {
        let why = if new_dir { "can't replace a file with a folder" } else { "can't replace a folder with a file" };
        return Err(io::Error::other(why));
    }
    if src.starts_with(target) {
        return Err(io::Error::other("can't replace the folder it is in"));
    }
    progress.check()?;
    if cut {
        let r = if new_dir { swap_dir(src, target) } else { std::fs::rename(src, target) };
        match r {
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
            r => return r,
        }
    }
    copy_into_place(src, target, true, progress, skipped)?;
    if cut {
        remove_any(src)?;
    }
    Ok(())
}

/// Bytes that `transfer` will copy for `src`: none for a move on the same drive, which is a rename.
fn bytes_to_copy(src: &Path, dest: &Path, cut: bool, progress: &CopyProgress) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if cut {
        let dev = |p: &Path| p.symlink_metadata().map(|m| m.dev()).ok();
        if dev(src).is_some() && dev(src) == dev(dest) {
            return 0;
        }
    }
    tree_size(src, progress)
}

/// `transfer_with`, keeping both on a clash (the tests' shorthand).
#[cfg(test)]
fn transfer(sources: Vec<PathBuf>, dest: PathBuf, cut: bool, progress: &CopyProgress) -> Outcome {
    transfer_with(sources, dest, cut, Clash::KeepBoth, progress)
}

/// Copies or moves `sources` into `dest`. Runs on a worker thread. Counts the total first, then reports bytes in
/// `progress`; on cancel it stops after removing the file being written and sets `progress.stopped`.
/// `policy` decides what happens to items whose name is taken in `dest` by another item.
fn transfer_with(sources: Vec<PathBuf>, dest: PathBuf, cut: bool, policy: Clash, progress: &CopyProgress) -> Outcome {
    let flags = clash_flags(&sources, &dest, cut);
    // Skipped items aren't copied, so they don't count
    let copied = sources.iter().zip(&flags).filter(|&(_, &f)| !(f && policy == Clash::Skip));
    let total = copied.map(|(s, _)| bytes_to_copy(s, &dest, cut, progress)).sum();
    progress.total.store(total, Ordering::Relaxed);
    progress.counted.store(true, Ordering::Relaxed);
    let mut out = Outcome::default();
    for (src, flagged) in sources.into_iter().zip(flags) {
        if progress.check().is_err() {
            progress.stopped.store(true, Ordering::Relaxed);
            break;
        }
        let Some(name) = src.file_name() else { continue };
        let label = name.to_string_lossy().into_owned();
        if dest.starts_with(&src) {
            out.errors.push(format!("Can't put “{label}” inside itself"));
            continue;
        }
        if cut && src.parent() == Some(dest.as_path()) {
            out.already += 1; // moving into the same folder: nothing to do
            continue;
        }
        // Still there: something else may have removed it since the check
        let clash = flagged && clashes(&src, &dest.join(name));
        if clash && policy == Clash::Skip {
            out.skipped += 1;
            continue;
        }
        let replacing = clash && policy == Clash::Replace;
        let target = if replacing { dest.join(name) } else { free_name(&dest, name) };
        let mut skipped = 0;
        let res = if replacing {
            replace(&src, &target, cut, progress, &mut skipped)
        } else if cut {
            match std::fs::rename(&src, &target) {
                Ok(()) => Ok(()),
                // Different drive: copy, then remove the original. If something was left out, the
                // original is kept so nothing is lost; a cancelled copy fails here and keeps it too.
                Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                    copy_into_place(&src, &target, false, progress, &mut skipped).and_then(|_| {
                        if skipped > 0 {
                            Ok(())
                        } else if src.symlink_metadata()?.is_dir() {
                            std::fs::remove_dir_all(&src)
                        } else {
                            std::fs::remove_file(&src)
                        }
                    })
                }
                Err(e) => Err(e),
            }
        } else {
            copy_into_place(&src, &target, false, progress, &mut skipped)
        };
        match res {
            Ok(()) if skipped == 0 => {
                out.done += 1;
                out.items.push(Transferred { source: src, target, replaced: replacing });
            }
            Ok(()) => {
                // Nothing at the target means the item itself was the special file
                let top_level = target.symlink_metadata().is_err();
                out.errors.push(skipped_msg(&label, skipped, top_level, cut));
            }
            Err(e) if progress.cancel.load(Ordering::Relaxed) => {
                // Only Cancel's own error goes unsaid: a partial copy that couldn't be deleted is reported
                if e.kind() != io::ErrorKind::Interrupted {
                    out.errors.push(format!("{label}: {e}"));
                }
                progress.stopped.store(true, Ordering::Relaxed);
                break;
            }
            Err(e) => out.errors.push(format!("{label}: {e}")),
        }
    }
    out
}

#[cfg(test)]
mod clash_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh `<tmp>/rfiles-clash-<tag>-<pid>` with `src/` and `dest/` folders.
    fn base(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rfiles-clash-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("dest")).unwrap();
        d
    }

    fn write(p: &Path, data: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    fn read(p: &Path) -> Vec<u8> {
        std::fs::read(p).unwrap()
    }

    /// Names of the temporaries a replace left behind in `dir`
    fn temps(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".rfiles-"))
            .collect()
    }

    fn run(srcs: &[PathBuf], dest: &Path, cut: bool, policy: Clash) -> Outcome {
        transfer_with(srcs.to_vec(), dest.to_path_buf(), cut, policy, &CopyProgress::default())
    }

    #[test]
    fn finds_clashes() {
        let d = base("find");
        let (src, dest) = (d.join("src"), d.join("dest"));
        write(&src.join("a.txt"), b"new a");
        write(&src.join("b.txt"), b"b");
        write(&src.join("f/x"), b"x");
        write(&dest.join("a.txt"), b"old");
        write(&dest.join("f/y"), b"y");
        write(&dest.join("f/z"), b"z");
        write(&dest.join("here.txt"), b"h");
        let srcs = [src.join("a.txt"), src.join("b.txt"), src.join("f")];
        let plan = find_clashes(&srcs, &dest, false);
        assert_eq!(plan.names, ["a.txt", "f"]);
        assert!(plan.already == 0 && plan.single.is_none());

        // One clash: both sides are described
        let plan = find_clashes(&srcs[1..], &dest, false);
        assert_eq!(plan.names, ["f"]);
        let (old, new) = plan.single.unwrap();
        assert_eq!((old.is_dir, old.size, new.is_dir, new.size), (true, 2, true, 1));
        assert!(old.modified.is_some());
        let plan = find_clashes(&srcs[..1], &dest, false);
        let (old, new) = plan.single.unwrap();
        assert_eq!((old.is_dir, old.size, new.size), (false, 3, 5));

        // An item pasted next to itself is no clash; one moved into its own folder is "already there"
        let plan = find_clashes(&[dest.join("here.txt")], &dest, false);
        assert!(plan.names.is_empty());
        let plan = find_clashes(&[dest.join("here.txt"), src.join("b.txt")], &dest, true);
        assert_eq!((plan.names.len(), plan.already), (0, 1));
        // Into itself: refused later, not asked about
        assert!(find_clashes(&[d.clone()], &dest, false).names.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn keep_both_records_the_new_names() {
        let d = base("keep");
        let (src, dest) = (d.join("src"), d.join("dest"));
        write(&src.join("a.txt"), b"new");
        write(&src.join("f/x"), b"x");
        write(&src.join("free.txt"), b"free");
        write(&dest.join("a.txt"), b"old");
        write(&dest.join("f/y"), b"y");
        let srcs = [src.join("a.txt"), src.join("f"), src.join("free.txt")];
        let out = run(&srcs, &dest, false, Clash::KeepBoth);
        assert_eq!((out.done, out.errors.len(), out.skipped), (3, 0, 0));
        assert_eq!(read(&dest.join("a.txt")), b"old");
        assert_eq!(read(&dest.join("a (copy).txt")), b"new");
        assert_eq!(read(&dest.join("f (copy)/x")), b"x");
        assert!(dest.join("f/y").exists());
        let t = |s: &str, t: &str| Transferred { source: src.join(s), target: dest.join(t), replaced: false };
        assert_eq!(out.items, [t("a.txt", "a (copy).txt"), t("f", "f (copy)"), t("free.txt", "free.txt")]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn replace_files_and_folders() {
        let d = base("replace");
        let (src, dest) = (d.join("src"), d.join("dest"));
        write(&src.join("a.txt"), b"new");
        write(&src.join("f/x"), b"new x");
        write(&dest.join("a.txt"), b"old old");
        write(&dest.join("f/x"), b"old x");
        write(&dest.join("f/only-old"), b"o");
        let srcs = [src.join("a.txt"), src.join("f")];
        let out = run(&srcs, &dest, false, Clash::Replace);
        assert_eq!((out.done, out.errors.len()), (2, 0), "{:?}", out.errors);
        assert_eq!(read(&dest.join("a.txt")), b"new");
        // The whole folder is replaced, not merged
        assert_eq!(read(&dest.join("f/x")), b"new x");
        assert!(!dest.join("f/only-old").exists());
        assert!(src.join("a.txt").exists() && src.join("f/x").exists());
        assert!(temps(&dest).is_empty(), "{:?}", temps(&dest));
        let t = |s: &str| Transferred { source: src.join(s), target: dest.join(s), replaced: true };
        assert_eq!(out.items, [t("a.txt"), t("f")]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn move_with_replace_on_the_same_drive() {
        let d = base("move-replace");
        let (src, dest) = (d.join("src"), d.join("dest"));
        write(&src.join("a.txt"), b"new");
        write(&src.join("f/x"), b"new x");
        write(&dest.join("a.txt"), b"old");
        write(&dest.join("f/only-old"), b"o");
        let out = run(&[src.join("a.txt"), src.join("f")], &dest, true, Clash::Replace);
        assert_eq!((out.done, out.errors.len()), (2, 0), "{:?}", out.errors);
        assert_eq!(read(&dest.join("a.txt")), b"new");
        assert_eq!(read(&dest.join("f/x")), b"new x");
        assert!(!dest.join("f/only-old").exists());
        assert!(!src.join("a.txt").exists() && !src.join("f").exists());
        assert!(temps(&dest).is_empty());
        assert!(out.items.iter().all(|i| i.replaced));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn skip_leaves_clashing_items_alone() {
        let d = base("skip");
        let (src, dest) = (d.join("src"), d.join("dest"));
        write(&src.join("a.txt"), b"new");
        write(&src.join("f/x"), b"new x");
        write(&src.join("free.txt"), b"free");
        write(&dest.join("a.txt"), b"old");
        write(&dest.join("f/y"), b"y");
        let srcs = [src.join("a.txt"), src.join("f"), src.join("free.txt")];
        let out = run(&srcs, &dest, false, Clash::Skip);
        assert_eq!((out.done, out.skipped, out.errors.len()), (1, 2, 0));
        assert_eq!(read(&dest.join("a.txt")), b"old");
        assert!(!dest.join("f/x").exists() && !dest.join("a (copy).txt").exists());
        assert_eq!(read(&dest.join("free.txt")), b"free");
        assert_eq!(result_text(false, &out, false), "Copied 1 item, skipped 2");

        // A move: the skipped sources stay where they are; the rest is moved
        std::fs::remove_file(dest.join("free.txt")).unwrap();
        let out = run(&srcs, &dest, true, Clash::Skip);
        assert_eq!((out.done, out.skipped), (1, 2));
        assert_eq!(read(&src.join("a.txt")), b"new");
        assert_eq!(read(&src.join("f/x")), b"new x");
        assert!(!src.join("free.txt").exists());
        let moved = Transferred { source: src.join("free.txt"), target: dest.join("free.txt"), replaced: false };
        assert_eq!(out.items, [moved]);
        assert_eq!(result_text(true, &out, false), "Moved 1 item, skipped 2");
        let out = run(&srcs[..2], &dest, true, Clash::Skip);
        assert_eq!(result_text(true, &out, false), "Skipped 2 items");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn failed_replace_keeps_the_old_item() {
        let d = base("replace-fail");
        let (src, dest) = (d.join("src"), d.join("dest"));
        write(&src.join("a.txt"), b"new");
        write(&src.join("f/ok"), b"ok");
        write(&src.join("f/secret"), b"s");
        write(&dest.join("a.txt"), b"old");
        write(&dest.join("f/old"), b"old");
        let lock = |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        lock(&src.join("a.txt"), 0o000);
        lock(&src.join("f/secret"), 0o000);
        // Root can read anything; then there is nothing to test
        if std::fs::File::open(src.join("a.txt")).is_err() {
            let out = run(&[src.join("a.txt"), src.join("f")], &dest, false, Clash::Replace);
            assert_eq!((out.done, out.errors.len()), (0, 2));
            assert_eq!(read(&dest.join("a.txt")), b"old");
            assert_eq!(read(&dest.join("f/old")), b"old");
            assert!(!dest.join("f/ok").exists());
            assert!(temps(&dest).is_empty(), "{:?}", temps(&dest));
            assert!(out.items.is_empty());
        }
        lock(&src.join("a.txt"), 0o644);
        lock(&src.join("f/secret"), 0o644);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cancelled_replace_keeps_the_old_item() {
        let d = base("replace-cancel");
        let (src, dest) = (d.join("src"), d.join("dest"));
        let data: Vec<u8> = (0..2 * CHUNK as usize + 5).map(|i| i as u8).collect();
        write(&src.join("big.bin"), &data);
        write(&src.join("f/big.bin"), &data);
        write(&dest.join("big.bin"), b"old");
        write(&dest.join("f/old"), b"old");
        for s in [src.join("big.bin"), src.join("f")] {
            let p = CopyProgress { cancel_at: AtomicU64::new(1), ..Default::default() };
            let out = transfer_with(vec![s], dest.clone(), false, Clash::Replace, &p);
            assert!(p.stopped.load(Ordering::Relaxed));
            assert_eq!((out.done, out.errors.len()), (0, 0));
        }
        assert_eq!(read(&dest.join("big.bin")), b"old");
        assert_eq!(read(&dest.join("f/old")), b"old");
        assert!(!dest.join("f/big.bin").exists());
        assert!(temps(&dest).is_empty(), "{:?}", temps(&dest));
        assert_eq!(read(&src.join("big.bin")), data);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_and_folder_never_replace_each_other() {
        let d = base("kinds");
        let (src, dest) = (d.join("src"), d.join("dest"));
        write(&src.join("a"), b"file");
        write(&src.join("b/x"), b"x");
        write(&dest.join("a/keep"), b"k");
        write(&dest.join("b"), b"file b");
        for cut in [false, true] {
            let out = run(&[src.join("a"), src.join("b")], &dest, cut, Clash::Replace);
            assert_eq!(out.errors, ["a: can't replace a folder with a file", "b: can't replace a file with a folder"]);
            assert_eq!(read(&dest.join("a/keep")), b"k");
            assert_eq!(read(&dest.join("b")), b"file b");
            assert!(src.join("a").exists() && src.join("b/x").exists());
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn replacing_an_item_with_itself_keeps_both() {
        let d = base("self");
        let dest = d.join("dest");
        write(&dest.join("a.txt"), b"a");
        let out = run(&[dest.join("a.txt")], &dest, false, Clash::Replace);
        assert_eq!(out.done, 1);
        assert_eq!(read(&dest.join("a.txt")), b"a");
        assert_eq!(read(&dest.join("a (copy).txt")), b"a");
        let copied = Transferred { source: dest.join("a.txt"), target: dest.join("a (copy).txt"), replaced: false };
        assert_eq!(out.items, [copied]);

        // Never replaces the folder the source is inside
        write(&d.join("x/x/f"), b"f");
        let out = run(&[d.join("x/x")], &d, false, Clash::Replace);
        assert_eq!(out.errors, ["x: can't replace the folder it is in"]);
        assert_eq!(read(&d.join("x/x/f")), b"f");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn same_named_sources_never_replace_each_other() {
        let d = base("same-names");
        let dest = d.join("dest");
        let srcs = [d.join("a/report.txt"), d.join("b/report.txt")];
        let fill = || {
            write(&srcs[0], b"from a");
            write(&srcs[1], b"from b");
        };
        let both = || {
            assert_eq!(read(&dest.join("report.txt")), b"from a");
            assert_eq!(read(&dest.join("report (copy).txt")), b"from b");
        };
        for policy in [Clash::Replace, Clash::Skip] {
            for cut in [false, true] {
                // Empty destination: both arrive, nothing is replaced or skipped
                let _ = std::fs::remove_dir_all(&dest);
                std::fs::create_dir_all(&dest).unwrap();
                fill();
                let plan = find_clashes(&srcs, &dest, cut);
                assert!(plan.names.is_empty(), "{policy:?} {cut}");
                let out = run(&srcs, &dest, cut, policy);
                assert_eq!((out.done, out.skipped, out.errors.len()), (2, 0, 0), "{policy:?} {cut}");
                both();
                assert!(out.items.iter().all(|i| !i.replaced));
                assert_eq!(srcs[0].exists() || srcs[1].exists(), !cut);

                // One pre-existing: it is replaced (or skipped) once; the second source is kept alongside
                let _ = std::fs::remove_dir_all(&dest);
                write(&dest.join("report.txt"), b"old");
                fill();
                assert_eq!(find_clashes(&srcs, &dest, cut).names, ["report.txt"]);
                let out = run(&srcs, &dest, cut, policy);
                if policy == Clash::Replace {
                    assert_eq!((out.done, out.errors.len()), (2, 0), "{cut}");
                    assert_eq!(read(&dest.join("report.txt")), b"from a");
                    assert_eq!(read(&dest.join("report (copy).txt")), b"from b");
                    assert_eq!(out.items.iter().map(|i| i.replaced).collect::<Vec<_>>(), [true, false]);
                    assert_eq!(srcs[0].exists() || srcs[1].exists(), !cut);
                } else {
                    assert_eq!((out.done, out.skipped), (1, 1), "{cut}");
                    assert_eq!(read(&dest.join("report.txt")), b"old");
                    assert_eq!(read(&dest.join("report (copy).txt")), b"from b");
                    assert_eq!(read(&srcs[0]), b"from a");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn move_into_its_own_folder_is_already_there() {
        let d = base("already");
        let dest = d.join("dest");
        write(&dest.join("a.txt"), b"a");
        write(&d.join("src/b.txt"), b"b");
        let out = run(&[dest.join("a.txt")], &dest, true, Clash::KeepBoth);
        assert_eq!((out.done, out.already), (0, 1));
        assert_eq!(result_text(true, &out, false), "Already in this folder");
        let out = run(&[dest.join("a.txt"), d.join("src/b.txt")], &dest, true, Clash::KeepBoth);
        assert_eq!(result_text(true, &out, false), "Moved 1 item, 1 already in this folder");
        assert_eq!(read(&dest.join("a.txt")), b"a");
        let _ = std::fs::remove_dir_all(&d);
    }
}

fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}

/// The progress row's text, e.g. "Copying 3 items… 42%  1.2 GB of 2.9 GB". `total` is None while counting.
fn progress_text(cut: bool, n: usize, done: u64, total: Option<u64>) -> String {
    let head = format!("{} {}…", if cut { "Moving" } else { "Copying" }, items(n));
    match total {
        None => head,
        Some(0) if done == 0 => head,
        Some(0) => format!("{head}  {}", glib::format_size(done)),
        Some(t) => {
            let pct = done.min(t) * 100 / t;
            format!("{head} {pct}%  {} of {}", glib::format_size(done.min(t)), glib::format_size(t))
        }
    }
}

/// The status bar line once a job has ended.
fn result_text(cut: bool, o: &Outcome, cancelled: bool) -> String {
    let verb = if cut { "Moved" } else { "Copied" };
    if !cancelled && o.errors.is_empty() && o.done == 0 && o.skipped + o.already > 0 {
        return if o.skipped == 0 { "Already in this folder".into() } else { format!("Skipped {}", items(o.skipped)) };
    }
    let mut s = if cancelled {
        format!("Cancelled after {}", items(o.done))
    } else if o.errors.is_empty() {
        format!("{verb} {}", items(o.done))
    } else {
        format!("{verb} {}, {} failed — {}", o.done, o.errors.len(), o.errors[0])
    };
    if cancelled && !o.errors.is_empty() {
        s += &format!(", {} failed — {}", o.errors.len(), o.errors[0]);
    }
    // Only when nothing failed, so the first error stays at the end where it is easy to read
    if o.errors.is_empty() && o.skipped > 0 {
        s += &format!(", skipped {}", o.skipped);
    }
    if o.errors.is_empty() && o.already > 0 {
        s += &format!(", {} already in this folder", o.already);
    }
    s
}

/// One side of a clash, for the dialog.
#[derive(Clone, Debug, PartialEq)]
struct Side {
    is_dir: bool,
    /// Bytes for a file; items directly inside for a folder (counting a whole tree could take long)
    size: u64,
    /// Seconds since 1970
    modified: Option<i64>,
}

fn side(p: &Path) -> Option<Side> {
    let md = p.symlink_metadata().ok()?;
    let size = if md.is_dir() { std::fs::read_dir(p).map(|r| r.count() as u64).unwrap_or(0) } else { md.len() };
    let modified = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    Some(Side { is_dir: md.is_dir(), size, modified: modified.map(|d| d.as_secs() as i64) })
}

/// Which sources of a paste or drop would land on a taken name.
#[derive(Debug, Default)]
struct Plan {
    /// Names of the clashing sources
    names: Vec<String>,
    /// (existing, incoming) when exactly one source clashes
    single: Option<(Side, Side)>,
    /// Sources of a move that already are in the destination
    already: usize,
}

/// For each source, whether it lands on a name that was taken before this operation, so Replace / Skip apply to it.
/// Only the first source with a given name can clash, and never on a name held by a source of this operation itself
/// (one already in `dest`): later ones keep both, so one source of a paste never replaces or skips another.
fn clash_flags(sources: &[PathBuf], dest: &Path, cut: bool) -> Vec<bool> {
    let at_dest = |src: &Path, name: &std::ffi::OsStr| src.parent() == Some(dest) || same_item(src, &dest.join(name));
    let mut seen: std::collections::HashSet<&std::ffi::OsStr> =
        sources.iter().filter_map(|s| s.file_name().filter(|n| at_dest(s, n))).collect();
    sources
        .iter()
        .map(|src| {
            let Some(name) = src.file_name() else { return false };
            if dest.starts_with(src) || (cut && src.parent() == Some(dest)) {
                return false;
            }
            seen.insert(name) && clashes(src, &dest.join(name))
        })
        .collect()
}

#[cfg(test)]
mod clash_flags_tests {
    use super::*;

    #[test]
    fn only_names_taken_before_count() {
        let d = std::env::temp_dir().join(format!("rfiles-clash-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for f in ["a/r.txt", "b/r.txt", "c/r.txt", "dest/x.txt", "a/x.txt"] {
            std::fs::create_dir_all(d.join(f).parent().unwrap()).unwrap();
            std::fs::write(d.join(f), f).unwrap();
        }
        let dest = d.join("dest");
        let r = |p: &str| d.join(p);
        assert_eq!(clash_flags(&[r("a/r.txt"), r("b/r.txt")], &dest, false), [false, false]);
        std::fs::write(dest.join("r.txt"), b"old").unwrap();
        assert_eq!(clash_flags(&[r("a/r.txt"), r("b/r.txt"), r("c/r.txt")], &dest, true), [true, false, false]);
        // A source already in dest holds its name, wherever it is in the list
        for cut in [false, true] {
            assert_eq!(clash_flags(&[r("a/x.txt"), r("dest/x.txt")], &dest, cut), [false, false]);
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// Finds the clashes before anything is copied, by the same rule as `transfer_with`. Runs on a worker thread.
fn find_clashes(sources: &[PathBuf], dest: &Path, cut: bool) -> Plan {
    let mut plan = Plan::default();
    let mut first = None;
    for (src, flagged) in sources.iter().zip(clash_flags(sources, dest, cut)) {
        let Some(name) = src.file_name() else { continue };
        if dest.starts_with(src) {
            continue; // refused by `transfer` anyway
        }
        if cut && src.parent() == Some(dest) {
            plan.already += 1;
            continue;
        }
        let target = dest.join(name);
        if flagged {
            first.get_or_insert((src.clone(), target));
            plan.names.push(name.to_string_lossy().into_owned());
        }
    }
    if let (1, Some((src, target))) = (plan.names.len(), first) {
        plan.single = side(&target).zip(side(&src));
    }
    plan
}

/// The clash dialog's (message, detail). `total` is how many items the operation acts on; `date` formats a time.
fn clash_text(plan: &Plan, total: usize, cut: bool, date: &dyn Fn(i64) -> String) -> (String, String) {
    let n = plan.names.len();
    let describe = |s: &Side| {
        let what = if s.is_dir {
            format!("folder with {}", items(s.size as usize))
        } else {
            glib::format_size(s.size).to_string()
        };
        match s.modified {
            Some(t) => format!("{what}, modified {}", date(t)),
            None => what,
        }
    };
    let mut parts = Vec::new();
    let msg = if n == 1 {
        if let Some((old, new)) = &plan.single {
            parts.push(format!("Existing: {}\nNew: {}", describe(old), describe(new)));
        }
        format!("“{}” already exists in this folder", plan.names[0])
    } else {
        let mut shown = plan.names.iter().take(3).map(|s| format!("“{s}”")).collect::<Vec<_>>().join(", ");
        if n > 3 {
            shown += &format!(" and {} more", n - 3);
        }
        parts.push(format!("{shown}. Your choice applies to all of them."));
        format!("{n} items already exist in this folder")
    };
    let verb = if cut { "moved" } else { "copied" };
    match total.saturating_sub(n) {
        0 => {}
        1 => parts.push(format!("The other item is {verb} either way.")),
        r => parts.push(format!("The other {r} items are {verb} either way.")),
    }
    parts.push(REPLACE_NOTE.into());
    (msg, parts.join("\n\n"))
}

#[cfg(test)]
mod clash_text_tests {
    use super::*;

    fn text(plan: &Plan, total: usize, cut: bool) -> (String, String) {
        let (m, d) = clash_text(plan, total, cut, &|t| format!("<{t}>"));
        (m, d.replace('\u{a0}', " "))
    }

    const REPLACE: &str = REPLACE_NOTE;

    #[test]
    fn one_clash_shows_both_items() {
        let old = Side { is_dir: false, size: 1_200_000, modified: Some(10) };
        let new = Side { is_dir: false, size: 1_300_000, modified: Some(20) };
        let plan = Plan { names: vec!["report.pdf".into()], single: Some((old, new)), already: 0 };
        let (m, d) = text(&plan, 1, false);
        assert_eq!(m, "“report.pdf” already exists in this folder");
        assert_eq!(d, format!("Existing: 1.2 MB, modified <10>\nNew: 1.3 MB, modified <20>\n\n{REPLACE}"));
        let (_, d) = text(&plan, 3, true);
        assert!(d.contains("\n\nThe other 2 items are moved either way.\n\n"), "{d}");

        let old = Side { is_dir: true, size: 1, modified: None };
        let new = Side { is_dir: true, size: 4, modified: Some(5) };
        let plan = Plan { names: vec!["Photos".into()], single: Some((old, new)), already: 0 };
        let (_, d) = text(&plan, 2, false);
        assert_eq!(
            d,
            format!(
                "Existing: folder with 1 item\nNew: folder with 4 items, modified <5>\n\n\
                 The other item is copied either way.\n\n{REPLACE}"
            )
        );
    }

    #[test]
    fn several_clashes_are_counted() {
        let names = |n: usize| (1..=n).map(|i| format!("f{i}")).collect();
        let plan = Plan { names: names(2), single: None, already: 0 };
        assert_eq!(
            text(&plan, 2, false),
            (
                "2 items already exist in this folder".into(),
                format!("“f1”, “f2”. Your choice applies to all of them.\n\n{REPLACE}")
            )
        );
        let plan = Plan { names: names(5), single: None, already: 0 };
        let (m, d) = text(&plan, 6, false);
        assert_eq!(m, "5 items already exist in this folder");
        assert!(d.starts_with("“f1”, “f2”, “f3” and 2 more. Your choice"), "{d}");
        assert!(d.contains("The other item is copied either way."));
    }
}

const REPLACE_NOTE: &str = "Replace puts the new item in place of the old one; folders are replaced whole, not merged.";

fn local_date(secs: i64) -> String {
    glib::DateTime::from_unix_local(secs)
        .ok()
        .and_then(|d| d.format("%Y-%m-%d  %H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Copies or moves `files` into `dest` (see `run_transfer`). First checks on a worker thread which names are taken
/// in `dest`; if any are, one dialog asks whether to keep both, replace or skip them, or cancel the whole operation.
/// Items with no local path (the trash) always keep both. The window is held weakly: if it closes before the copy
/// starts, nothing is copied.
pub fn start_transfer(
    app: &Rc<App>,
    files: Vec<gio::File>,
    dest: PathBuf,
    cut: bool,
    then: impl FnOnce(&Outcome, bool) + 'static,
) {
    let srcs: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
    if srcs.len() < files.len() {
        run_transfer(app, files, dest, cut, Clash::KeepBoth, then);
        return;
    }
    let weak = Rc::downgrade(app);
    glib::spawn_future_local(async move {
        let (s, d) = (srcs.clone(), dest.clone());
        let Ok(plan) = gio::spawn_blocking(move || find_clashes(&s, &d, cut)).await else { return };
        let Some(app) = weak.upgrade() else { return };
        if cut && plan.already == srcs.len() {
            // Nothing to do; the clipboard is kept, so the files can still be pasted elsewhere
            app.status.set_text("Already in this folder");
            return;
        }
        if plan.names.is_empty() {
            run_transfer(&app, files, dest, cut, Clash::KeepBoth, then);
            return;
        }
        let (msg, detail) = clash_text(&plan, srcs.len() - plan.already, cut, &local_date);
        // Keep both is the default, so Enter never loses anything; Escape is Cancel
        let dlg = gtk::AlertDialog::builder()
            .message(msg)
            .detail(detail)
            .buttons(["Cancel", "Skip", "Replace", "Keep both"])
            .cancel_button(0)
            .default_button(3)
            .build();
        // A dialog doesn't close with its parent on its own; this takes it away when the window goes
        let gone = gio::Cancellable::new();
        let c = gone.clone();
        let on_destroy = app.window.connect_destroy(move |_| c.cancel());
        let weak = Rc::downgrade(&app);
        let g = gone.clone();
        dlg.choose(Some(&app.window), Some(&gone), move |r| {
            // Window closed meanwhile (its handlers are gone with it): copy nothing
            let Some(app) = weak.upgrade() else { return };
            if g.is_cancelled() {
                return;
            }
            app.window.disconnect(on_destroy);
            let policy = match r {
                Ok(1) => Clash::Skip,
                Ok(2) => Clash::Replace,
                Ok(3) => Clash::KeepBoth,
                _ => return,
            };
            run_transfer(&app, files, dest, cut, policy, then);
        });
    });
}

/// Copies or moves `files` into `dest` on a worker thread, with a progress row (text, bar, Cancel) above the
/// status bar, then leaves the result in the status bar and calls `then` with it and whether it was cancelled.
/// `policy` says what to do with names taken in `dest`.
/// Items with no local path (the trash) are copied through GIO and can't be cancelled or moved.
/// The window is held weakly and the application kept running until the job ends, so closing the window
/// doesn't cut the copy short; it then just has no row to update.
fn run_transfer(
    app: &Rc<App>,
    files: Vec<gio::File>,
    dest: PathBuf,
    cut: bool,
    policy: Clash,
    then: impl FnOnce(&Outcome, bool) + 'static,
) {
    let srcs: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
    let remote = srcs.len() < files.len();
    if remote && cut {
        app.status.set_text("Can only move local files");
        return;
    }
    let n = files.len();
    let progress = Arc::new(CopyProgress::default());

    let label = gtk::Label::new(Some(&progress_text(cut, n, 0, None)));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(pango::EllipsizeMode::End);
    let bar = gtk::ProgressBar::new();
    bar.set_valign(gtk::Align::Center);
    bar.set_size_request(160, -1);
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("flat");
    // The GIO copy out of the trash has no cancel point
    cancel.set_sensitive(!remote);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("transfer-row");
    row.append(&label);
    row.append(&bar);
    row.append(&cancel);
    app.transfers.append(&row);

    let p = progress.clone();
    cancel.connect_clicked(move |b| {
        p.cancel.store(true, Ordering::Relaxed);
        b.set_sensitive(false);
    });

    // Ends itself when the job is done or the row is gone; never removed from outside
    let finished = Arc::new(AtomicBool::new(false));
    let (p, f, wl, wb) = (progress.clone(), finished.clone(), label.downgrade(), bar.downgrade());
    glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
        let (Some(label), Some(bar)) = (wl.upgrade(), wb.upgrade()) else { return glib::ControlFlow::Break };
        if f.load(Ordering::Relaxed) {
            return glib::ControlFlow::Break;
        }
        let done = p.done.load(Ordering::Relaxed);
        let total = p.counted.load(Ordering::Relaxed).then(|| p.total.load(Ordering::Relaxed));
        label.set_text(&progress_text(cut, n, done, total));
        match total {
            Some(t) if t > 0 => bar.set_fraction(done.min(t) as f64 / t as f64),
            _ => bar.pulse(),
        }
        glib::ControlFlow::Continue
    });

    let hold = app.window.application().map(|g| g.hold());
    let weak = Rc::downgrade(app);
    let wrow = row.downgrade();
    glib::spawn_future_local(async move {
        let p = progress.clone();
        let out = gio::spawn_blocking(move || {
            if remote {
                p.counted.store(true, Ordering::Relaxed);
                crate::trash::copy_into(files, dest, p.done.clone())
            } else {
                transfer_with(srcs, dest, cut, policy, &p)
            }
        })
        .await;
        finished.store(true, Ordering::Relaxed);
        drop(hold);
        let cancelled = progress.stopped.load(Ordering::Relaxed);
        if let Ok(o) = &out {
            then(o, cancelled);
        }
        // Copies out of the trash report no items, so they aren't recorded
        let note = match &out {
            Ok(o) => crate::undo::record_transfer(cut, &o.items),
            Err(_) => String::new(),
        };
        let Some(app) = weak.upgrade() else { return };
        if let Some(row) = wrow.upgrade() {
            app.transfers.remove(&row);
        }
        // In the result banner, so it survives the next click; Details lists every error
        app.notify(match out {
            Ok(o) => crate::banner::Notice { text: result_text(cut, &o, cancelled) + &note, errors: o.errors },
            Err(_) => crate::banner::Notice::stopped(if cut { "Move failed" } else { "Copy failed" }),
        });
        // Folder views update themselves; Starred / search results don't
        if !matches!(app.current_loc(), Loc::Dir(_)) {
            app.reload();
        }
    });
}

#[cfg(test)]
mod transfer_cancel_tests {
    use super::*;

    fn base(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rfiles-cancel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("dest")).unwrap();
        d
    }

    #[test]
    fn counts_the_total_first() {
        let d = base("total");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a"), [0u8; 100]).unwrap();
        std::fs::write(src.join("sub/b"), [0u8; 23]).unwrap();
        std::fs::write(d.join("outside"), [0u8; 5000]).unwrap();
        // Links aren't followed, so the file they point at doesn't count
        std::os::unix::fs::symlink(d.join("outside"), src.join("link")).unwrap();
        assert_eq!(tree_size(&src, &CopyProgress::default()), 123);

        let p = CopyProgress::default();
        let out = transfer(vec![src.clone(), d.join("outside")], d.join("dest"), false, &p);
        assert_eq!((out.done, out.errors.len()), (2, 0));
        assert!(p.counted.load(Ordering::Relaxed));
        assert_eq!(p.total.load(Ordering::Relaxed), 5123);
        assert_eq!(p.done.load(Ordering::Relaxed), 5123);
        assert!(!p.stopped.load(Ordering::Relaxed));

        // A move on the same drive is a rename: nothing to copy
        let p = CopyProgress::default();
        let out = transfer(vec![d.join("dest/outside")], d.clone(), true, &p);
        assert_eq!(out.done, 1);
        assert_eq!(p.total.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cancel_mid_file_removes_the_partial_copy() {
        let d = base("mid");
        let big = d.join("big.bin");
        let data: Vec<u8> = (0..2 * CHUNK as usize + 5).map(|i| i as u8).collect();
        std::fs::write(&big, &data).unwrap();
        let p = CopyProgress { cancel_at: AtomicU64::new(1), ..Default::default() };
        let out = transfer(vec![big.clone()], d.join("dest"), false, &p);
        assert_eq!((out.done, out.errors.len()), (0, 0));
        assert!(p.stopped.load(Ordering::Relaxed));
        let done = p.done.load(Ordering::Relaxed);
        assert!(done > 0 && done < data.len() as u64, "{done}");
        assert!(d.join("dest/big.bin").symlink_metadata().is_err());
        assert_eq!(std::fs::read(&big).unwrap(), data);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cancel_between_items_keeps_the_finished_ones() {
        let d = base("between");
        let (a, b) = (d.join("a.txt"), d.join("b.txt"));
        std::fs::write(&a, b"0123456789").unwrap();
        std::fs::write(&b, b"abcdefghij").unwrap();
        let p = CopyProgress { cancel_at: AtomicU64::new(10), ..Default::default() };
        let out = transfer(vec![a.clone(), b.clone()], d.join("dest"), false, &p);
        assert_eq!((out.done, out.errors.len()), (1, 0));
        assert!(p.stopped.load(Ordering::Relaxed));
        assert_eq!(std::fs::read(d.join("dest/a.txt")).unwrap(), b"0123456789");
        assert!(d.join("dest/b.txt").symlink_metadata().is_err());
        assert!(a.exists() && b.exists());
        assert_eq!(result_text(false, &out, true), "Cancelled after 1 item");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cancelled_move_keeps_the_sources() {
        // Cancelled before the first item: nothing is renamed, copied or deleted
        let d = base("move");
        let src = d.join("folder");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f"), b"x").unwrap();
        let p = CopyProgress::default();
        p.cancel.store(true, Ordering::Relaxed);
        let out = transfer(vec![src.clone()], d.join("dest"), true, &p);
        assert_eq!(out.done, 0);
        assert!(p.stopped.load(Ordering::Relaxed));
        assert_eq!(std::fs::read(src.join("f")).unwrap(), b"x");
        assert!(d.join("dest/folder").symlink_metadata().is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn row_and_result_texts() {
        let gb = 1_000_000_000;
        // glib puts a no-break space between number and unit
        let progress_text = |cut, n, done, total| super::progress_text(cut, n, done, total).replace('\u{a0}', " ");
        assert_eq!(progress_text(false, 3, 0, None), "Copying 3 items…");
        assert_eq!(progress_text(true, 1, 0, Some(0)), "Moving 1 item…");
        let text = "Copying 3 items… 41%  1.2 GB of 2.9 GB";
        assert_eq!(progress_text(false, 3, 1_200_000_000, Some(2_900_000_000)), text);
        // Never past 100%, even if files grew while being copied
        assert_eq!(progress_text(false, 2, 3 * gb, Some(2 * gb)), "Copying 2 items… 100%  2.0 GB of 2.0 GB");
        assert_eq!(progress_text(false, 2, 1_500_000, Some(0)), "Copying 2 items…  1.5 MB");
        let o = Outcome { done: 2, ..Default::default() };
        assert_eq!(result_text(true, &o, false), "Moved 2 items");
        assert_eq!(result_text(false, &o, true), "Cancelled after 2 items");
        let o = Outcome { done: 1, errors: vec!["x: denied".into()], ..Default::default() };
        assert_eq!(result_text(false, &o, false), "Copied 1, 1 failed — x: denied");
        assert_eq!(result_text(false, &o, true), "Cancelled after 1 item, 1 failed — x: denied");
    }
}

/// Directory flags for a terminal we know by file name. Servers like gnome-terminal, ptyxis and kgx hand the
/// window to an already-running process, so the client's working directory alone would be lost.
fn terminal_dir_args(name: &str, d: &str) -> Option<Vec<String>> {
    // Debian ships e.g. gnome-terminal.wrapper behind the alternative
    let name = name.strip_suffix(".wrapper").unwrap_or(name);
    Some(match name {
        "ptyxis" => vec!["--new-window".into(), "--working-directory".into(), d.into()],
        "gnome-terminal" | "kgx" | "xfce4-terminal" | "foot" => vec![format!("--working-directory={d}")],
        "konsole" => vec!["--workdir".into(), d.into()],
        "tilix" => vec!["-w".into(), d.into()],
        "kitty" => vec!["--directory".into(), d.into()],
        "alacritty" => vec!["--working-directory".into(), d.into()],
        "wezterm" => vec!["start".into(), "--cwd".into(), d.into()],
        _ => return None,
    })
}

/// Ordered (program, args) to try for opening a terminal in `d` (an absolute path): the user's choice first
/// (`xdg-terminal-exec`, then `x-terminal-emulator`), then the known list. `installed` says whether a program
/// is in PATH; `alt_target` is what `x-terminal-emulator` resolves to.
fn terminal_candidates(
    d: &str,
    installed: &dyn Fn(&str) -> bool,
    alt_target: Option<&Path>,
) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    // --dir= is one argument (the value may hold spaces or start with "-"); it maps it to the terminal's own
    // directory option, or changes directory itself when the terminal has none
    if installed("xdg-terminal-exec") {
        out.push(("xdg-terminal-exec".into(), vec![format!("--dir={d}")]));
    }
    if installed("x-terminal-emulator") {
        let name = alt_target.and_then(|t| t.file_name()).and_then(|n| n.to_str());
        let known = name.and_then(|n| terminal_dir_args(n, d));
        out.push(("x-terminal-emulator".into(), known.unwrap_or_default()));
    }
    for name in [
        "ptyxis", "gnome-terminal", "kgx", "konsole", "xfce4-terminal", "tilix", "kitty", "alacritty", "wezterm",
        "foot",
    ] {
        if installed(name) {
            out.push((name.into(), terminal_dir_args(name, d).unwrap_or_default()));
        }
    }
    if installed("xterm") {
        out.push(("xterm".into(), vec![]));
    }
    out
}

/// Opens the user's terminal in `dir`.
pub fn open_terminal(dir: &Path) -> Result<(), String> {
    // Absolute, so the flags still point at the folder and a leading "-" can't look like an option
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let d = dir.to_string_lossy().into_owned();
    let alt = glib::find_program_in_path("x-terminal-emulator").and_then(|p| std::fs::canonicalize(p).ok());
    let installed = |prog: &str| glib::find_program_in_path(prog).is_some();
    // The working directory is also set on the process, which is enough for terminals not listed here.
    for (prog, args) in terminal_candidates(&d, &installed, alt.as_deref()) {
        let mut cmd = std::process::Command::new(&prog);
        cmd.args(&args).current_dir(&dir);
        if let Ok(child) = cmd.spawn() {
            // GLib reaps it when it exits (no zombie, no parked thread per terminal)
            glib::child_watch_add(glib::Pid(child.id() as i32), |_, _| {});
            return Ok(());
        }
    }
    Err("No terminal app found".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn terminal_order_and_flags() {
        let all = |_: &str| true;
        let c = terminal_candidates("/home/u/My Docs", &all, Some(Path::new("/usr/bin/gnome-terminal.wrapper")));
        let names: Vec<&str> = c.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names[..3], ["xdg-terminal-exec", "x-terminal-emulator", "ptyxis"]);
        assert_eq!(names.last(), Some(&"xterm"));
        // folder with spaces stays one argument; the alternative's target picks the flags
        assert_eq!(c[0].1, ["--dir=/home/u/My Docs"]);
        assert_eq!(c[1].1, ["--working-directory=/home/u/My Docs"]);
        assert_eq!(c[2].1, ["--new-window", "--working-directory", "/home/u/My Docs"]);

        let only = |n: &str| n == "x-terminal-emulator";
        let c = terminal_candidates("/x", &only, Some(Path::new("/usr/bin/ptyxis")));
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1, ["--new-window", "--working-directory", "/x"]);
        // unknown target: no flags, the working directory has to do
        let c = terminal_candidates("/x", &only, Some(Path::new("/opt/odd-term")));
        assert!(c[0].1.is_empty());
        assert!(terminal_candidates("/x", &|_| false, None).is_empty());
    }

    #[test]
    fn terminal_dash_folder_is_a_value() {
        let c = terminal_candidates("-weird dir", &|_| true, None);
        for (p, args) in &c {
            if let Some(i) = args.iter().position(|a| a == "-weird dir") {
                assert!(i > 0 && args[i - 1].starts_with('-'), "{p}: {args:?}");
            }
        }
        assert!(c.iter().any(|(p, a)| p == "xdg-terminal-exec" && a == &["--dir=-weird dir"]));
        assert!(c.iter().any(|(p, a)| p == "kitty" && a == &["--directory", "-weird dir"]));
        assert!(c.iter().any(|(p, a)| p == "gnome-terminal" && a == &["--working-directory=-weird dir"]));
    }

    fn mtime(p: &Path) -> SystemTime {
        p.symlink_metadata().unwrap().modified().unwrap()
    }

    #[test]
    fn copy_keeps_modified_dates() {
        let base = std::env::temp_dir().join(format!("rfiles-fileops-test-{}", std::process::id()));
        let (src, dest) = (base.join("src"), base.join("dest"));
        let sub = src.join("photos");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let file = sub.join("a.jpg");
        std::fs::write(&file, b"jpeg").unwrap();
        let ro = src.join("readonly.txt");
        std::fs::write(&ro, b"text").unwrap();

        // 2001-09-09, with sub-second part to check it isn't rounded away
        let old = SystemTime::UNIX_EPOCH + Duration::new(1_000_000_000, 123_456_789);
        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(900_000_000);
        let set = |p: &Path, t| std::fs::File::open(p).unwrap().set_modified(t).unwrap();
        set(&file, old);
        set(&ro, old);
        set(&sub, older);
        set(&src, older);
        let mut perms = ro.metadata().unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ro, perms).unwrap();

        let out = transfer(vec![src.clone()], dest.clone(), false, &CopyProgress::default());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let copy = dest.join("src");
        assert_eq!(mtime(&copy.join("photos/a.jpg")), old);
        assert_eq!(mtime(&copy.join("readonly.txt")), old);
        assert_eq!(mtime(&copy.join("photos")), older);
        assert_eq!(mtime(&copy), older);

        // A single file copied next to itself keeps its date too
        let out = transfer(vec![file.clone()], sub.clone(), false, &CopyProgress::default());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(mtime(&sub.join("a (copy).jpg")), old);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Runs `transfer` on a thread so a blocked copy fails the test instead of hanging it.
    fn transfer_with_timeout(sources: Vec<PathBuf>, dest: PathBuf, cut: bool) -> Outcome {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(transfer(sources, dest, cut, &CopyProgress::default()));
        });
        rx.recv_timeout(Duration::from_secs(10)).expect("transfer blocked")
    }

    fn mkfifo(p: &Path) {
        let ok = std::process::Command::new("mkfifo").arg(p).status().unwrap().success();
        assert!(ok, "mkfifo failed");
    }

    #[test]
    fn copy_skips_pipes_and_sockets() {
        let base = std::env::temp_dir().join(format!("rfiles-fileops-special-{}", std::process::id()));
        let (src, dest) = (base.join("src"), base.join("dest"));
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"b").unwrap();
        mkfifo(&src.join("pipe"));
        let _sock = std::os::unix::net::UnixListener::bind(src.join("sub/sock")).unwrap();

        // A folder holding special files: the rest is copied, the special ones are reported
        let out = transfer_with_timeout(vec![src.clone()], dest.clone(), false);
        assert_eq!(out.done, 0);
        assert_eq!(out.errors, ["src: copied without 2 pipes, sockets or devices"]);
        let copy = dest.join("src");
        assert_eq!(std::fs::read(copy.join("a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(copy.join("sub/b.txt")).unwrap(), b"b");
        assert!(copy.join("pipe").symlink_metadata().is_err());
        assert!(copy.join("sub/sock").symlink_metadata().is_err());

        // A pipe on its own is not copied, and the other items still are
        let out = transfer_with_timeout(vec![src.join("pipe"), src.join("a.txt")], dest.clone(), false);
        assert_eq!(out.done, 1);
        assert_eq!(out.errors, ["pipe: is a pipe, socket or device, not copied"]);
        assert!(dest.join("pipe").symlink_metadata().is_err());
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"a");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn skipped_message_says_the_original_was_kept_on_move() {
        let kept = "x: copied without 1 pipe, socket or device; the original was kept";
        assert_eq!(skipped_msg("x", 1, false, true), kept);
        assert_eq!(skipped_msg("x", 3, false, false), "x: copied without 3 pipes, sockets or devices");
    }
}
