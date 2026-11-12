//! Copy / cut / paste, and "Open in Terminal".
//!
//! The clipboard uses the same format as GNOME Files (Nautilus), so you can
//! copy in one app and paste in the other.

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Picks a free name in `dir`: "photo.jpg" → "photo (copy).jpg" → "photo (copy 2).jpg"
fn free_name(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let first = dir.join(name);
    if first.symlink_metadata().is_err() {
        return first;
    }
    let p = Path::new(name);
    let is_dir = first.is_dir();
    let (stem, ext) = if is_dir {
        (name.to_string_lossy().into_owned(), String::new())
    } else {
        (
            p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default(),
        )
    };
    for i in 1.. {
        let n = if i == 1 { format!("{stem} (copy){ext}") } else { format!("{stem} (copy {i}){ext}") };
        let cand = dir.join(n);
        if cand.symlink_metadata().is_err() {
            return cand;
        }
    }
    unreachable!()
}

/// Copies `src` to `dst`. Pipes, sockets and devices are left out and counted in `skipped`: reading a
/// pipe blocks until something writes to it, and a device like /dev/zero never ends.
fn copy_recursive(src: &Path, dst: &Path, done: &AtomicU64, skipped: &mut usize) -> io::Result<()> {
    let md = src.symlink_metadata()?;
    let ft = md.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
    } else if ft.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()), done, skipped)?;
        }
        let _ = std::fs::set_permissions(dst, md.permissions());
        // Last, so creating the children doesn't bump the folder's date again
        keep_times(dst, &md);
    } else if ft.is_file() {
        std::fs::copy(src, dst)?;
        keep_times(dst, &md);
        done.fetch_add(md.len(), Ordering::Relaxed);
    } else {
        *skipped += 1;
    }
    Ok(())
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

/// Copies or moves `sources` into `dest`. Runs on a worker thread.
pub fn transfer(sources: Vec<PathBuf>, dest: PathBuf, cut: bool, progress: Arc<AtomicU64>) -> Outcome {
    let mut out = Outcome { done: 0, errors: Vec::new() };
    for src in sources {
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
                // original is kept so nothing is lost.
                Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                    copy_recursive(&src, &target, &progress, &mut skipped).and_then(|_| {
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
            copy_recursive(&src, &target, &progress, &mut skipped)
        };
        match res {
            Ok(()) if skipped == 0 => out.done += 1,
            Ok(()) => {
                // Nothing at the target means the item itself was the special file
                let top_level = target.symlink_metadata().is_err();
                out.errors.push(skipped_msg(&label, skipped, top_level, cut));
            }
            Err(e) => out.errors.push(format!("{label}: {e}")),
        }
    }
    out
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
    // No directory flag here: it starts the terminal in the working directory of the process
    if installed("xdg-terminal-exec") {
        out.push(("xdg-terminal-exec".into(), vec![]));
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
        assert!(c[0].1.is_empty());
        // folder with spaces stays one argument; the alternative's target picks the flags
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

        let out = transfer(vec![src.clone()], dest.clone(), false, Arc::new(AtomicU64::new(0)));
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let copy = dest.join("src");
        assert_eq!(mtime(&copy.join("photos/a.jpg")), old);
        assert_eq!(mtime(&copy.join("readonly.txt")), old);
        assert_eq!(mtime(&copy.join("photos")), older);
        assert_eq!(mtime(&copy), older);

        // A single file copied next to itself keeps its date too
        let out = transfer(vec![file.clone()], sub.clone(), false, Arc::new(AtomicU64::new(0)));
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(mtime(&sub.join("a (copy).jpg")), old);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Runs `transfer` on a thread so a blocked copy fails the test instead of hanging it.
    fn transfer_with_timeout(sources: Vec<PathBuf>, dest: PathBuf, cut: bool) -> Outcome {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(transfer(sources, dest, cut, Arc::new(AtomicU64::new(0))));
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
