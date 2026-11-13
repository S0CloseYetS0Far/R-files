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
    let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let list = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
    let text = files
        .iter()
        .map(|f| f.parse_name().to_string())
        .collect::<Vec<_>>()
        .join("\n");
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
    let text = String::from_utf8_lossy(&data);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    let cut = if mime == GNOME_MIME {
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
fn copy_recursive(src: &Path, dst: &Path, progress: &CopyProgress, skipped: &mut usize) -> io::Result<()> {
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

pub struct Outcome {
    pub done: usize,
    pub errors: Vec<String>,
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

/// Copies or moves `sources` into `dest`. Runs on a worker thread. Counts the total first, then reports bytes in
/// `progress`; on cancel it stops after removing the file being written and sets `progress.stopped`.
pub fn transfer(sources: Vec<PathBuf>, dest: PathBuf, cut: bool, progress: &CopyProgress) -> Outcome {
    let total = sources.iter().map(|s| bytes_to_copy(s, &dest, cut, progress)).sum();
    progress.total.store(total, Ordering::Relaxed);
    progress.counted.store(true, Ordering::Relaxed);
    let mut out = Outcome { done: 0, errors: Vec::new() };
    for src in sources {
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
            continue; // moving into the same folder: nothing to do
        }
        let target = free_name(&dest, name);
        let mut skipped = 0;
        let res = if cut {
            match std::fs::rename(&src, &target) {
                Ok(()) => Ok(()),
                // Different drive: copy, then remove the original. If something was left out, the
                // original is kept so nothing is lost; a cancelled copy fails here and keeps it too.
                Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                    copy_recursive(&src, &target, progress, &mut skipped).and_then(|_| {
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
            copy_recursive(&src, &target, progress, &mut skipped)
        };
        match res {
            Ok(()) if skipped == 0 => out.done += 1,
            Ok(()) => {
                // Nothing at the target means the item itself was the special file
                let top_level = target.symlink_metadata().is_err();
                out.errors.push(skipped_msg(&label, skipped, top_level, cut));
            }
            Err(_) if progress.cancel.load(Ordering::Relaxed) => {
                progress.stopped.store(true, Ordering::Relaxed);
                break;
            }
            Err(e) => out.errors.push(format!("{label}: {e}")),
        }
    }
    out
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
    s
}

/// Copies or moves `files` into `dest` on a worker thread, with a progress row (text, bar, Cancel) above the
/// status bar, then leaves the result in the status bar and calls `then` with it and whether it was cancelled.
/// Used by paste and drag-and-drop.
/// Items with no local path (the trash) are copied through GIO and can't be cancelled or moved.
/// The window is held weakly and the application kept running until the job ends, so closing the window
/// doesn't cut the copy short; it then just has no row to update.
pub fn start_transfer(
    app: &Rc<App>,
    files: Vec<gio::File>,
    dest: PathBuf,
    cut: bool,
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
                transfer(srcs, dest, cut, &p)
            }
        })
        .await;
        finished.store(true, Ordering::Relaxed);
        drop(hold);
        let cancelled = progress.stopped.load(Ordering::Relaxed);
        if let Ok(o) = &out {
            then(o, cancelled);
        }
        let Some(app) = weak.upgrade() else { return };
        if let Some(row) = wrow.upgrade() {
            app.transfers.remove(&row);
        }
        app.status.set_text(&match &out {
            Ok(o) => result_text(cut, o, cancelled),
            Err(_) => if cut { "Move failed" } else { "Copy failed" }.into(),
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
        let o = Outcome { done: 2, errors: vec![] };
        assert_eq!(result_text(true, &o, false), "Moved 2 items");
        assert_eq!(result_text(false, &o, true), "Cancelled after 2 items");
        let o = Outcome { done: 1, errors: vec!["x: denied".into()] };
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
