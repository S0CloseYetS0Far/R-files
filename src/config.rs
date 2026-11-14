use crate::util::SortKey;
use gtk::glib;
use std::path::PathBuf;

/// Small plain-text settings in ~/.config/rfiles/.
pub struct Config {
    pub pins: Vec<String>,
    pub starred: Vec<String>,
    pub width: i32,
    pub height: i32,
    pub sidebar_pos: i32,
    pub files_pos: i32,
    /// Width of the right preview pane, in pixels
    pub preview_width: i32,
    pub maximized: bool,
    pub list_view: bool,
    pub show_hidden: bool,
    /// "blue", "system" or "custom:/path/to/image"
    pub folder_icon: String,
    pub icon_size: i32,
    /// Order of the built-in sidebar places, by key
    pub places_order: Vec<String>,
    /// What the file views are sorted by, and whether reversed
    pub sort: SortKey,
    pub sort_desc: bool,
    /// Whether the sidebar and the preview pane are shown
    pub show_sidebar: bool,
    pub show_preview: bool,
}

fn dir() -> PathBuf {
    glib::user_config_dir().join("rfiles")
}

fn read_lines(dir: &std::path::Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(name))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

impl Config {
    pub fn load() -> Self {
        Self::load_from(&dir())
    }

    /// `load` from a given folder, so tests can use a temp dir.
    fn load_from(dir: &std::path::Path) -> Self {
        let mut c = Config {
            pins: read_lines(dir, "pins"),
            starred: read_lines(dir, "starred"),
            width: 1300,
            height: 820,
            sidebar_pos: 220,
            files_pos: 700,
            preview_width: 420,
            maximized: false,
            list_view: false,
            show_hidden: false,
            folder_icon: "blue".into(),
            icon_size: 40,
            places_order: Vec::new(),
            sort: SortKey::Name,
            sort_desc: false,
            show_sidebar: true,
            show_preview: true,
        };
        for line in read_lines(dir, "settings") {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "folder_icon" => {
                    c.folder_icon = v.trim().to_string();
                    continue;
                }
                "places" => {
                    c.places_order = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    continue;
                }
                "sort" => {
                    c.sort = SortKey::parse(v.trim()).unwrap_or(SortKey::Name);
                    continue;
                }
                _ => {}
            }
            if read_pane_setting(&mut c.show_sidebar, &mut c.show_preview, k.trim(), v) {
                continue;
            }
            let n = v.trim().parse::<i32>().ok();
            match (k.trim(), n) {
                ("width", Some(n)) if n > 300 => c.width = n,
                ("height", Some(n)) if n > 200 => c.height = n,
                ("sidebar", Some(n)) if n > 60 => c.sidebar_pos = n,
                ("files", Some(n)) if n > 150 => c.files_pos = n,
                ("preview", Some(n)) if n > 100 => c.preview_width = n,
                ("maximized", Some(n)) => c.maximized = n != 0,
                ("list_view", Some(n)) => c.list_view = n != 0,
                ("show_hidden", Some(n)) => c.show_hidden = n != 0,
                ("sort_desc", Some(n)) => c.sort_desc = n != 0,
                ("icon_size", Some(n)) if (24..=128).contains(&n) => c.icon_size = n,
                _ => {}
            }
        }
        c
    }

    pub fn save(&self) {
        self.save_to(&dir())
    }

    /// `save` into a given folder, so tests can use a temp dir.
    fn save_to(&self, d: &std::path::Path) {
        let _ = std::fs::create_dir_all(d);
        let _ = std::fs::write(d.join("pins"), self.pins.join("\n"));
        let _ = std::fs::write(d.join("starred"), self.starred.join("\n"));
        let _ = std::fs::write(
            d.join("settings"),
            format!(
                "width={}\nheight={}\nmaximized={}\nsidebar={}\npreview={}\nfiles={}\nlist_view={}\nshow_hidden={}\nfolder_icon={}\nicon_size={}\nplaces={}\nsort={}\nsort_desc={}\n{}",
                self.width,
                self.height,
                self.maximized as i32,
                self.sidebar_pos,
                self.preview_width,
                self.files_pos,
                self.list_view as i32,
                self.show_hidden as i32,
                self.folder_icon,
                self.icon_size,
                self.places_order.join(","),
                self.sort.as_str(),
                self.sort_desc as i32,
                pane_settings(self.show_sidebar, self.show_preview)
            ),
        );
    }
}

#[cfg(test)]
mod load_save_tests {
    use super::Config;
    use crate::util::SortKey;
    use std::path::{Path, PathBuf};

    /// A fresh temp folder that is removed again when the test ends, even on failure.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("rfiles-config-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn assert_defaults(c: &Config) {
        assert!(c.pins.is_empty() && c.starred.is_empty() && c.places_order.is_empty());
        assert_eq!((c.width, c.height, c.sidebar_pos, c.files_pos, c.preview_width), (1300, 820, 220, 700, 420));
        assert!(!c.maximized && !c.list_view && !c.show_hidden && !c.sort_desc);
        assert_eq!((c.folder_icon.as_str(), c.icon_size, c.sort), ("blue", 40, SortKey::Name));
        assert!(c.show_sidebar && c.show_preview);
    }

    fn write(d: &Path, name: &str, text: &str) {
        std::fs::write(d.join(name), text).unwrap();
    }

    #[test]
    fn missing_folder_gives_defaults() {
        let t = TempDir::new("missing");
        assert_defaults(&Config::load_from(&t.0.join("does-not-exist")));
    }

    #[test]
    fn round_trip_keeps_every_field() {
        let t = TempDir::new("round-trip");
        // Saving creates the folder itself.
        let d = t.0.join("rfiles");
        let mut c = Config::load_from(&d);
        c.pins = vec!["file:///home/a/b%20c".into(), "file:///tmp".into()];
        c.starred = vec!["file:///home/a/x.txt".into()];
        (c.width, c.height, c.sidebar_pos, c.files_pos, c.preview_width) = (1024, 700, 180, 500, 300);
        (c.maximized, c.list_view, c.show_hidden, c.sort_desc) = (true, true, true, true);
        c.folder_icon = "custom:/home/a/pic=1.png".into();
        c.icon_size = 96;
        c.places_order = vec!["trash".into(), "home".into(), "downloads".into()];
        c.sort = SortKey::Modified;
        (c.show_sidebar, c.show_preview) = (false, false);
        c.save_to(&d);

        let r = Config::load_from(&d);
        assert_eq!(r.pins, c.pins);
        assert_eq!(r.starred, c.starred);
        assert_eq!((r.width, r.height, r.sidebar_pos, r.files_pos, r.preview_width), (1024, 700, 180, 500, 300));
        assert!(r.maximized && r.list_view && r.show_hidden && r.sort_desc);
        // The value is everything after the first '=', so a path holding '=' survives.
        assert_eq!(r.folder_icon, "custom:/home/a/pic=1.png");
        assert_eq!((r.icon_size, r.sort), (96, SortKey::Modified));
        assert_eq!(r.places_order, c.places_order);
        assert!(!r.show_sidebar && !r.show_preview);

        // Saving again with empty lists empties the files instead of leaving the old entries.
        let mut e = r;
        e.pins.clear();
        e.starred.clear();
        e.places_order.clear();
        e.save_to(&d);
        let r = Config::load_from(&d);
        assert!(r.pins.is_empty() && r.starred.is_empty() && r.places_order.is_empty());
    }

    #[test]
    fn partial_file_keeps_defaults_for_the_rest() {
        let t = TempDir::new("partial");
        write(&t.0, "settings", "width=900\nsort=size\n");
        let c = Config::load_from(&t.0);
        assert_eq!((c.width, c.sort), (900, SortKey::Size));
        assert_eq!((c.height, c.icon_size, c.folder_icon.as_str()), (820, 40, "blue"));
        assert!(c.show_sidebar && c.show_preview && !c.maximized);
    }

    #[test]
    fn bad_lines_and_values_are_ignored() {
        let t = TempDir::new("bad");
        write(
            &t.0,
            "settings",
            "garbage without equals\n\n  width = 1500  \nheight=abc\nsidebar=10\nfiles=-5\npreview=99999999999\n\
             icon_size=500\nsort=bogus\nmaximized=yes\nlist_view=2\nunknown=1\n=3\nplaces= home , ,trash,\n",
        );
        let c = Config::load_from(&t.0);
        // Spaces around keys and values are trimmed.
        assert_eq!(c.width, 1500);
        // Not a number, below the minimum or out of i32 range: default kept.
        assert_eq!((c.height, c.sidebar_pos, c.files_pos, c.preview_width), (820, 220, 700, 420));
        assert_eq!((c.icon_size, c.sort), (40, SortKey::Name));
        assert!(!c.maximized);
        // Any non-zero number turns a flag on.
        assert!(c.list_view);
        // Empty entries in the places list are dropped.
        assert_eq!(c.places_order, vec!["home".to_string(), "trash".to_string()]);
    }

    #[test]
    fn list_files_skip_blank_lines_and_trim() {
        let t = TempDir::new("lists");
        write(&t.0, "pins", "\n  file:///a  \n\n\nfile:///b\r\n");
        write(&t.0, "starred", "");
        let c = Config::load_from(&t.0);
        assert_eq!(c.pins, vec!["file:///a".to_string(), "file:///b".to_string()]);
        assert!(c.starred.is_empty());
    }

    #[test]
    fn unreadable_files_give_defaults() {
        let t = TempDir::new("unreadable");
        // Not UTF-8: read_to_string fails and the whole file is treated as missing.
        std::fs::write(t.0.join("settings"), [b'w', b'=', 0xff, 0xfe, b'\n']).unwrap();
        // A folder where a file is expected.
        std::fs::create_dir(t.0.join("pins")).unwrap();
        assert_defaults(&Config::load_from(&t.0));
    }
}

/// The `show_sidebar=` / `show_preview=` lines of the settings file.
fn pane_settings(sidebar: bool, preview: bool) -> String {
    format!("show_sidebar={}\nshow_preview={}\n", sidebar as i32, preview as i32)
}

/// Reads one of the keys `pane_settings` writes; false for any other key. A value that
/// is not a number leaves the setting as it was.
fn read_pane_setting(sidebar: &mut bool, preview: &mut bool, key: &str, value: &str) -> bool {
    let target = match key {
        "show_sidebar" => sidebar,
        "show_preview" => preview,
        _ => return false,
    };
    if let Ok(n) = value.trim().parse::<i32>() {
        *target = n != 0;
    }
    true
}

#[cfg(test)]
mod pane_setting_tests {
    use super::*;

    fn parse(text: &str) -> (bool, bool) {
        let (mut s, mut p) = (true, true);
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                read_pane_setting(&mut s, &mut p, k.trim(), v);
            }
        }
        (s, p)
    }

    #[test]
    fn round_trip() {
        for flags in [(true, true), (false, true), (true, false), (false, false)] {
            assert_eq!(parse(&pane_settings(flags.0, flags.1)), flags);
        }
    }

    #[test]
    fn defaults_and_bad_values() {
        assert_eq!(parse("width=900\nsort=name\n"), (true, true));
        assert_eq!(parse("show_sidebar=x\nshow_preview=\n"), (true, true));
        assert_eq!(parse(" show_sidebar = 0 \nshow_preview=0"), (false, false));
        let (mut s, mut p) = (true, true);
        assert!(!read_pane_setting(&mut s, &mut p, "preview", "0"));
        assert!((s, p) == (true, true));
    }
}

impl Config {
    /// Points pins and stars at their new place after RFiles renamed things.
    /// `renames` are (old URI, new URI) pairs. Returns whether anything changed.
    pub fn apply_renames(&mut self, renames: &[(String, String)]) -> bool {
        let pins = rename_entries(&mut self.pins, renames);
        let stars = rename_entries(&mut self.starred, renames);
        pins || stars
    }
}

/// `uri` after `old` became `new`, when it is that item or inside it.
/// Only matches on a path-segment boundary: "/a/b" does not contain "/a/bc".
pub fn renamed_uri(uri: &str, old: &str, new: &str) -> Option<String> {
    if uri == old {
        return Some(new.to_string());
    }
    let rest = uri.strip_prefix(old)?;
    rest.starts_with('/').then(|| format!("{new}{rest}"))
}

/// Rewrites `list` for `renames`, which may be nested (a folder and items in it) and may
/// swap names, so each rename is matched against an entry's original URI, deepest first.
fn rename_entries(list: &mut Vec<String>, renames: &[(String, String)]) -> bool {
    let mut order: Vec<&(String, String)> = renames.iter().collect();
    order.sort_by_key(|(old, _)| std::cmp::Reverse(old.len()));
    let mut changed = false;
    for entry in list.iter_mut() {
        let mut cur = entry.clone();
        for (old, new) in &order {
            if renamed_uri(entry, old, new).is_some() {
                // A deeper rename keeps its parent's old prefix, so this still matches
                if let Some(n) = renamed_uri(&cur, old, new) {
                    cur = n;
                }
            }
        }
        if cur != *entry {
            *entry = cur;
            changed = true;
        }
    }
    if changed {
        // A stale entry may already have held the new name
        let mut seen = std::collections::HashSet::new();
        list.retain(|u| seen.insert(u.clone()));
    }
    changed
}

#[cfg(test)]
mod rename_tests {
    use super::*;
    use gtk::prelude::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn item_and_children() {
        assert_eq!(renamed_uri("file:///a/b", "file:///a/b", "file:///a/c").as_deref(), Some("file:///a/c"));
        assert_eq!(renamed_uri("file:///a/b/x/y", "file:///a/b", "file:///a/c").as_deref(), Some("file:///a/c/x/y"));
        assert_eq!(renamed_uri("file:///a", "file:///a/b", "file:///a/c"), None);
    }

    #[test]
    fn segment_boundary() {
        assert_eq!(renamed_uri("file:///a/bc", "file:///a/b", "file:///a/z"), None);
        assert_eq!(renamed_uri("file:///a/b.txt", "file:///a/b", "file:///a/z"), None);
    }

    #[test]
    fn percent_encoded_spaces() {
        let old = "file:///home/u/My%20Docs";
        let new = "file:///home/u/Old%20Docs%202";
        assert_eq!(
            renamed_uri("file:///home/u/My%20Docs/a%20b.txt", old, new).as_deref(),
            Some("file:///home/u/Old%20Docs%202/a%20b.txt")
        );
        assert_eq!(renamed_uri("file:///home/u/My%20Docs%20x", old, new), None);
        // The real encoding comes from GIO, the same way pins and stars are stored
        let f = gtk::gio::File::for_path("/tmp/My Docs/a b.txt");
        assert_eq!(f.uri(), "file:///tmp/My%20Docs/a%20b.txt");
    }

    #[test]
    fn list_rewrite_nested_and_swapped() {
        let mut l = s(&["file:///p/A/y", "file:///p/A", "file:///p/a", "file:///p/b", "file:///q"]);
        // Batch order: deeper first, plus a swap of a and b
        let r = pairs(&[
            ("file:///p/A/y", "file:///p/A/y2"),
            ("file:///p/a", "file:///p/b"),
            ("file:///p/b", "file:///p/a"),
            ("file:///p/A", "file:///p/B"),
        ]);
        assert!(rename_entries(&mut l, &r));
        assert_eq!(l, s(&["file:///p/B/y2", "file:///p/B", "file:///p/b", "file:///p/a", "file:///q"]));
        // The order of the pairs doesn't matter
        let mut l = s(&["file:///p/A/y"]);
        let mut rev = r.clone();
        rev.reverse();
        rename_entries(&mut l, &rev);
        assert_eq!(l, s(&["file:///p/B/y2"]));
    }

    #[test]
    fn list_rewrite_dedups_and_reports_no_change() {
        let mut l = s(&["file:///a", "file:///b"]);
        assert!(rename_entries(&mut l, &pairs(&[("file:///a", "file:///b")])));
        assert_eq!(l, s(&["file:///b"]));
        assert!(!rename_entries(&mut l, &pairs(&[("file:///x", "file:///y")])));
    }
}

/// Local path of a `file://` URI, unless it is a GVfs/portal FUSE mount (network or
/// sandboxed, so slow and its errors mean little). None means: don't check it.
pub fn checkable_path(uri: &str) -> Option<PathBuf> {
    let (path, _) = glib::filename_from_uri(uri).ok()?;
    let c: Vec<_> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let fuse = c.len() >= 5 && c[1] == "run" && c[2] == "user" && (c[4] == "gvfs" || c[4] == "doc");
    (!fuse).then_some(path)
}

/// Whether a pinned folder is plainly not there (deleted, renamed, or its drive unplugged).
/// Blocking: call off the UI thread.
pub fn pin_is_missing(uri: &str) -> bool {
    let Some(path) = checkable_path(uri) else { return false };
    matches!(std::fs::metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Folders one of which must be a mounted drive for "not found" under `path` to be
/// trusted: drives under /media, /run/media and /mnt leave a missing or empty folder when
/// unplugged. None when `path` is not under those (the system drive is always there).
fn drive_mount_candidates(path: &std::path::Path) -> Option<Vec<PathBuf>> {
    let c: Vec<_> = path.components().map(|c| c.as_os_str().to_owned()).collect();
    // (index of the first candidate component, how many levels may be the drive)
    let (skip, take) = match c.get(1).and_then(|s| s.to_str()) {
        // /media/LABEL or /media/USER/LABEL
        Some("media") => (2, 2),
        // /run/media/USER/LABEL
        Some("run") if c.get(2).and_then(|s| s.to_str()) == Some("media") => (4, 1),
        Some("mnt") => (2, 2),
        _ => return None,
    };
    // Only strict ancestors: a drive's own root can't prove its drive is there
    let mut out = Vec::new();
    for n in skip..(skip + take).min(c.len().saturating_sub(1)) {
        out.push(c[..=n].iter().collect());
    }
    Some(out)
}

/// What the folder a starred file lived in looks like now.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Parent {
    /// Missing or unreadable: a drive that isn't there, or a folder deleted as a whole
    Gone,
    /// An existing empty folder, which is what an unmounted mount point looks like
    Empty,
    HasEntries,
}

/// The decision for a starred entry whose file could not be read. Drop it only on positive
/// evidence that the place it lived in is really there: the error is "not found", its
/// folder exists and is not empty, and its drive, if under /media, /run/media or /mnt, is
/// mounted. So stars in a folder deleted as a whole are kept on purpose: that can't be told
/// apart from a drive mounted somewhere else (fstab, a locked encrypted folder) that is absent.
fn may_drop_star(err: std::io::ErrorKind, parent: Parent, drive_mounted: Option<bool>) -> bool {
    err == std::io::ErrorKind::NotFound && parent == Parent::HasEntries && drive_mounted != Some(false)
}

fn is_mount_point(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(m), Some(parent)) = (std::fs::metadata(dir), dir.parent()) else { return false };
    std::fs::metadata(parent).map(|p| p.dev() != m.dev()).unwrap_or(false)
}

/// Whether a starred file is gone for good, so its entry can be dropped (see `may_drop_star`).
/// Anything unsure (other URIs, permission or I/O errors, a missing or empty folder, an
/// unplugged drive) is kept. Blocking.
pub fn star_is_dead(uri: &str) -> bool {
    let Some(path) = checkable_path(uri) else { return false };
    let err = match std::fs::symlink_metadata(&path) {
        Ok(_) => return false,
        Err(e) => e.kind(),
    };
    if err != std::io::ErrorKind::NotFound {
        return false;
    }
    let parent = match path.parent().map(std::fs::read_dir) {
        Some(Ok(mut entries)) => {
            if entries.next().is_some() { Parent::HasEntries } else { Parent::Empty }
        }
        _ => Parent::Gone,
    };
    let mounted = drive_mount_candidates(&path).map(|dirs| dirs.iter().any(|d| is_mount_point(d)));
    may_drop_star(err, parent, mounted)
}

#[cfg(test)]
mod dead_star_tests {
    use super::*;
    use gtk::prelude::*;
    use std::io::ErrorKind;
    use std::path::Path;

    #[test]
    fn decision() {
        use Parent::*;
        assert!(may_drop_star(ErrorKind::NotFound, HasEntries, None), "file gone, folder still has others");
        assert!(!may_drop_star(ErrorKind::NotFound, Gone, None), "folder missing: absent drive or deleted");
        assert!(!may_drop_star(ErrorKind::NotFound, Empty, None), "empty folder: maybe an unmounted mount point");
        // Under /media and friends the drive must be mounted too
        assert!(may_drop_star(ErrorKind::NotFound, HasEntries, Some(true)));
        assert!(!may_drop_star(ErrorKind::NotFound, HasEntries, Some(false)), "unplugged drive");
        assert!(!may_drop_star(ErrorKind::NotFound, Gone, Some(false)));
        assert!(!may_drop_star(ErrorKind::PermissionDenied, HasEntries, None));
        assert!(!may_drop_star(ErrorKind::PermissionDenied, HasEntries, Some(true)));
        assert!(!may_drop_star(ErrorKind::Other, HasEntries, None));
    }

    #[test]
    fn drive_candidates() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(drive_mount_candidates(Path::new("/home/u/x")), None);
        let disk = Some(vec![p("/media/u"), p("/media/u/Disk")]);
        assert_eq!(drive_mount_candidates(Path::new("/media/u/Disk/a/b")), disk);
        assert_eq!(drive_mount_candidates(Path::new("/media/u/Disk")), Some(vec![p("/media/u")]));
        assert_eq!(drive_mount_candidates(Path::new("/run/media/u/Disk/a")), Some(vec![p("/run/media/u/Disk")]));
        assert_eq!(drive_mount_candidates(Path::new("/run/media/u/Disk")), Some(vec![]));
        assert_eq!(drive_mount_candidates(Path::new("/mnt/data/x")), Some(vec![p("/mnt/data")]));
        assert_eq!(drive_mount_candidates(Path::new("/run/user/1000/x")), None);
    }

    #[test]
    fn only_local_uris_are_checked() {
        assert_eq!(checkable_path("file:///home/u/a%20b").as_deref(), Some(Path::new("/home/u/a b")));
        assert_eq!(checkable_path("sftp://host/home/u"), None);
        assert_eq!(checkable_path("file:///run/user/1000/gvfs/smb-share:server=x,share=y/f"), None);
        assert_eq!(checkable_path("file:///run/user/1000/doc/abc/f"), None);
        assert!(!star_is_dead("sftp://host/gone"));
        assert!(!pin_is_missing("smb://host/share"));
    }

    #[test]
    fn on_disk() {
        let d = std::env::temp_dir().join(format!("rfiles-dead-star-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("a b.txt");
        std::fs::write(&f, "x").unwrap();
        let uri = gtk::gio::File::for_path(&f).uri().to_string();
        let dir_uri = gtk::gio::File::for_path(&d).uri().to_string();
        assert!(!star_is_dead(&uri));
        assert!(!pin_is_missing(&dir_uri));
        // Gone from a folder that still has other entries: dead
        std::fs::write(d.join("other"), "y").unwrap();
        std::fs::remove_file(&f).unwrap();
        assert!(star_is_dead(&uri));
        // Gone from a folder that is now empty: kept (may be an unmounted mount point)
        std::fs::remove_file(d.join("other")).unwrap();
        assert!(!star_is_dead(&uri));
        // The folder itself is gone: kept, but the folder's pin is missing
        std::fs::remove_dir_all(&d).unwrap();
        assert!(!star_is_dead(&uri));
        assert!(pin_is_missing(&dir_uri));
        // A file on a drive that isn't mounted is kept
        assert!(!star_is_dead("file:///media/no-such-user-rfiles/Disk/file.txt"));
    }
}
