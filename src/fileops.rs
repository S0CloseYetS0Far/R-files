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

fn copy_recursive(src: &Path, dst: &Path, done: &AtomicU64) -> io::Result<()> {
    let md = src.symlink_metadata()?;
    let ft = md.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
    } else if ft.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()), done)?;
        }
        let _ = std::fs::set_permissions(dst, md.permissions());
    } else {
        std::fs::copy(src, dst)?;
        done.fetch_add(md.len(), Ordering::Relaxed);
    }
    Ok(())
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
        let res = if cut {
            match std::fs::rename(&src, &target) {
                Ok(()) => Ok(()),
                // Different drive: copy, then remove the original
                Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                    copy_recursive(&src, &target, &progress).and_then(|_| {
                        if src.symlink_metadata()?.is_dir() {
                            std::fs::remove_dir_all(&src)
                        } else {
                            std::fs::remove_file(&src)
                        }
                    })
                }
                Err(e) => Err(e),
            }
        } else {
            copy_recursive(&src, &target, &progress)
        };
        match res {
            Ok(()) => out.done += 1,
            Err(e) => out.errors.push(format!("{label}: {e}")),
        }
    }
    out
}

/// Opens the user's terminal in `dir`.
pub fn open_terminal(dir: &Path) -> Result<(), String> {
    let d = dir.to_string_lossy().into_owned();
    // (program, args) — the working directory is also set on the process,
    // which is enough for terminals not listed here.
    let candidates: Vec<(&str, Vec<String>)> = vec![
        ("ptyxis", vec!["--new-window".into(), "--working-directory".into(), d.clone()]),
        ("gnome-terminal", vec![format!("--working-directory={d}")]),
        ("kgx", vec![format!("--working-directory={d}")]),
        ("konsole", vec!["--workdir".into(), d.clone()]),
        ("xfce4-terminal", vec![format!("--working-directory={d}")]),
        ("tilix", vec!["-w".into(), d.clone()]),
        ("kitty", vec!["--directory".into(), d.clone()]),
        ("alacritty", vec!["--working-directory".into(), d.clone()]),
        ("wezterm", vec!["start".into(), "--cwd".into(), d.clone()]),
        ("foot", vec![format!("--working-directory={d}")]),
        ("x-terminal-emulator", vec![]),
        ("xterm", vec![]),
    ];
    for (prog, args) in candidates {
        if glib::find_program_in_path(prog).is_none() {
            continue;
        }
        let mut cmd = std::process::Command::new(prog);
        cmd.args(&args).current_dir(dir);
        match cmd.spawn() {
            Ok(mut child) => {
                // Reap it when it exits so it doesn't linger as a zombie
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            Err(_) => continue,
        }
    }
    Err("No terminal app found".into())
}
