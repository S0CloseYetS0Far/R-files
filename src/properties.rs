//! Properties dialog: name, type, location, size (folder sizes are
//! counted on a worker thread and shown live), dates, image dimensions,
//! owner/group and editable permissions. Several selected items get a
//! combined view (count, total size).

use crate::util::*;
use gtk::prelude::*;
use gtk::{gdk_pixbuf, gio, glib};
use std::cell::Cell;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

/// Attributes for the single-item view (one cheap query on the UI thread).
const PROPS_ATTRS: &str = "standard::name,standard::display-name,standard::icon,\
standard::type,standard::size,standard::content-type,standard::is-symlink,\
standard::symlink-target,time::modified,time::access,time::created,unix::mode,\
owner::user,owner::user-real,owner::group,thumbnail::path,metadata::custom-icon,unix::blocks";

/// How often the live folder size is redrawn.
const TICK: Duration = Duration::from_millis(200);

// ================= pure helpers =================

/// Text of the "Link to" row.
fn link_to_text(target: &str, broken: bool) -> String {
    if broken { format!("{target} (broken)") } else { target.to_string() }
}

/// A followed query that still says "symlink" means the target is missing: GIO then reports the link itself.
fn is_broken_link(info: &gio::FileInfo) -> bool {
    info.is_symlink() && info.file_type() == gio::FileType::SymbolicLink
}

/// Query that follows symlinks (the bool is "broken link"). Where GIO fails instead of falling back to
/// the link itself, query again without following.
fn query_following(file: &gio::File) -> Result<(gio::FileInfo, bool), glib::Error> {
    let c = None::<&gio::Cancellable>;
    match file.query_info(PROPS_ATTRS, gio::FileQueryInfoFlags::NONE, c) {
        Ok(i) => Ok((i.clone(), is_broken_link(&i))),
        Err(e) => match file.query_info(PROPS_ATTRS, gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, c) {
            Ok(i) if i.is_symlink() => Ok((i, true)),
            _ => Err(e),
        },
    }
}

/// Owner, group, others: shift of their three bits in a mode.
const SHIFTS: [u32; 3] = [6, 3, 0];
/// Read, write, execute bits (before shifting).
const PERMS: [u32; 3] = [4, 2, 1];

/// `ls -l` style text, e.g. "drwxr-xr-x" (includes setuid/setgid/sticky).
fn symbolic_mode(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o020000 => 'c',
        0o060000 => 'b',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    // (special bit, letter when x is set, letter when x is not set)
    let special = [(0o4000, 's', 'S'), (0o2000, 's', 'S'), (0o1000, 't', 'T')];
    for (i, shift) in SHIFTS.iter().enumerate() {
        let b = (mode >> shift) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        let (bit, on, off) = special[i];
        let x = b & 1 != 0;
        s.push(match (mode & bit != 0, x) {
            (true, true) => on,
            (true, false) => off,
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// Permission bits as octal text: "644", or "4755" with special bits.
fn octal_mode(mode: u32) -> String {
    format!("{:03o}", mode & 0o7777)
}

/// Parses an octal mode typed by the user ("644", "0755", "4755").
fn parse_octal(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    u32::from_str_radix(s, 8).ok()
}

/// "Allow running as program": on adds x wherever r is set (and always for
/// the owner), off clears every x bit.
fn set_exec(mode: u32, on: bool) -> u32 {
    if on {
        mode | 0o100 | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    }
}

/// 1234567 -> "1,234,567"
pub(crate) fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1.2 MB (1,234,567 bytes)", or "512 bytes" for small sizes.
fn size_text(bytes: u64) -> String {
    if bytes < 1000 {
        format!("{bytes} byte{}", if bytes == 1 { "" } else { "s" })
    } else {
        format!("{} ({} bytes)", glib::format_size(bytes), group_digits(bytes))
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

/// Below this difference the "on disk" figure is just block rounding and not worth showing.
const DISK_SLACK: u64 = 4096;

/// "1.3 MB on disk", or None when the space used is within one block of the size.
fn on_disk_text(bytes: u64, disk: u64) -> Option<String> {
    (disk.abs_diff(bytes) > DISK_SLACK).then(|| format!("{} on disk", glib::format_size(disk)))
}

/// `size_text` plus the on-disk figure when it differs noticeably.
fn size_with_disk(bytes: u64, disk: u64) -> String {
    match on_disk_text(bytes, disk) {
        Some(d) => format!("{}, {d}", size_text(bytes)),
        None => size_text(bytes),
    }
}

/// "12.3 GB free of 500.1 GB", or None when the values are missing or inconsistent.
fn free_space_text(free: u64, total: u64) -> Option<String> {
    (total > 0 && free <= total).then(|| format!("{} free of {}", glib::format_size(free), glib::format_size(total)))
}

/// Used share of a file system, 0.0 to 1.0.
fn used_fraction(free: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (total.saturating_sub(free) as f64 / total as f64).clamp(0.0, 1.0)
}

/// Whether `path` is the root of a file system (a mount point).
fn is_mount_point(path: &Path) -> bool {
    let Ok(m) = std::fs::metadata(path) else { return false };
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        None => true,
        Some(parent) => std::fs::metadata(parent).map(|pm| pm.dev() != m.dev()).unwrap_or(false),
    }
}

/// New permission bits for an entry inside a folder whose mode is `folder_mode`.
/// Folders get the folder's read/write/enter bits; files get the same bits with
/// execute removed unless the file already has an execute bit. Setuid, setgid
/// and sticky bits are never taken from the folder: the entry keeps its own.
fn enclosed_mode(folder_mode: u32, entry_mode: u32, is_dir: bool) -> u32 {
    let rwx = folder_mode & 0o777;
    let bits = if is_dir || entry_mode & 0o111 != 0 { rwx } else { rwx & !0o111 };
    (entry_mode & 0o7000) | bits
}

// ================= folder size walk =================

/// Running totals, written by the worker thread and read by the UI.
#[derive(Default)]
struct Totals {
    /// Entries found inside folders (the selected items themselves excluded).
    items: AtomicU64,
    /// Sum of sizes of everything that is not a folder.
    bytes: AtomicU64,
    /// Entries or folders that could not be read.
    errors: AtomicU64,
    /// Space really used by those files (st_blocks * 512), hard links once.
    disk: AtomicU64,
    done: AtomicBool,
}

/// Adds a non-folder's size, counting hard-linked files once.
fn add_file(m: &std::fs::Metadata, t: &Totals, seen: &mut HashSet<(u64, u64)>) {
    // Only inodes with several links are remembered, so memory stays small.
    if m.nlink() > 1 && !seen.insert((m.dev(), m.ino())) {
        return;
    }
    t.bytes.fetch_add(m.len(), Relaxed);
    t.disk.fetch_add(m.blocks().saturating_mul(512), Relaxed);
}

/// Totals the sizes under `roots`. Symlinks inside are never followed
/// (a link to a selected folder is followed when `follow_roots`), folders
/// on other file systems (mount points) are counted but not entered, and
/// the walk stops as soon as `cancel` is set. Memory is bounded by the
/// pending folders, not the tree size.
fn walk(roots: &[PathBuf], follow_roots: bool, t: &Totals, cancel: &AtomicBool) {
    let mut seen = HashSet::new();
    let mut stack: Vec<(PathBuf, u64)> = Vec::new();
    for r in roots {
        let m = if follow_roots { std::fs::metadata(r) } else { std::fs::symlink_metadata(r) };
        match m {
            Ok(m) if m.is_dir() => stack.push((r.clone(), m.dev())),
            Ok(m) => add_file(&m, t, &mut seen),
            Err(_) => {
                t.errors.fetch_add(1, Relaxed);
            }
        }
    }
    while let Some((dir, dev)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            t.errors.fetch_add(1, Relaxed);
            continue;
        };
        for e in rd {
            if cancel.load(Relaxed) {
                return;
            }
            t.items.fetch_add(1, Relaxed);
            // DirEntry::metadata does not follow symlinks.
            let Ok(m) = e.and_then(|e| e.metadata().map(|m| (e, m))) else {
                t.errors.fetch_add(1, Relaxed);
                continue;
            };
            let (e, m) = m;
            if m.is_dir() {
                if m.dev() == dev {
                    stack.push((e.path(), dev));
                }
            } else {
                add_file(&m, t, &mut seen);
            }
        }
    }
}

/// Starts the walk on a worker thread and refreshes `size` / `contents`
/// every TICK until it ends (`contents` is hidden when there is nothing
/// inside and `hide_empty`). Closing the dialog (dropping the labels or
/// setting `cancel`) stops both.
fn start_walk(
    roots: Vec<PathBuf>,
    follow_roots: bool,
    hide_empty: bool,
    size: &gtk::Label,
    contents: &gtk::Label,
    contents_key: &gtk::Label,
    cancel: Arc<AtomicBool>,
) {
    let t = Arc::new(Totals::default());
    {
        let (t, cancel) = (t.clone(), cancel.clone());
        let spawned = std::thread::Builder::new().name("rfiles-size".into()).spawn(move || {
            walk(&roots, follow_roots, &t, &cancel);
            t.done.store(true, Relaxed);
        });
        if spawned.is_err() {
            size.set_text("Unknown");
            return;
        }
    }
    let (size, contents, contents_key) = (size.downgrade(), contents.downgrade(), contents_key.downgrade());
    let update = move || -> glib::ControlFlow {
        let (Some(size), Some(contents), Some(key)) = (size.upgrade(), contents.upgrade(), contents_key.upgrade()) else {
            cancel.store(true, Relaxed);
            return glib::ControlFlow::Break;
        };
        let done = t.done.load(Relaxed);
        let (items, bytes, errors) = (t.items.load(Relaxed), t.bytes.load(Relaxed), t.errors.load(Relaxed));
        let disk = t.disk.load(Relaxed);
        let more = if done { "" } else { " …" };
        size.set_text(&format!("{}{more}", size_with_disk(bytes, disk)));
        let mut c = plural(items, "item", "items");
        if errors > 0 {
            c.push_str(&format!(", {} unreadable", group_digits(errors)));
        }
        c.push_str(more);
        contents.set_text(&c);
        let show = !hide_empty || items > 0 || errors > 0 || !done;
        contents.set_visible(show);
        key.set_visible(show);
        if done {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    };
    if update() == glib::ControlFlow::Continue {
        glib::timeout_add_local(TICK, update);
    }
}

// ================= permissions =================

struct Perms {
    file: gio::File,
    /// Full st_mode (type bits kept for display) as last applied/read.
    mode: Cell<u32>,
    checks: Vec<gtk::CheckButton>,
    exec: Option<gtk::CheckButton>,
    octal: gtk::Entry,
    symbolic: gtk::Label,
    err: gtk::Label,
    /// Set while the controls are updated from code, so their signal
    /// handlers don't apply anything.
    syncing: Cell<bool>,
    /// Latest change; replies to older ones are ignored.
    generation: Cell<u32>,
}

impl Perms {
    fn sync(&self) {
        let mode = self.mode.get();
        self.syncing.set(true);
        for (c, s) in SHIFTS.iter().enumerate() {
            for (p, bit) in PERMS.iter().enumerate() {
                self.checks[c * 3 + p].set_active(mode & (bit << s) != 0);
            }
        }
        if let Some(x) = &self.exec {
            x.set_active(mode & 0o111 != 0);
        }
        self.octal.set_text(&octal_mode(mode));
        self.symbolic.set_text(&symbolic_mode(mode));
        self.syncing.set(false);
    }

    fn show_error(&self, msg: &str) {
        self.err.set_text(msg);
        self.err.set_visible(!msg.is_empty());
    }

    /// Applies `new` right away (async), then reads the mode back so the
    /// controls always show what the file system really has.
    fn apply(self: &Rc<Self>, new: u32) {
        if new & 0o7777 == self.mode.get() & 0o7777 {
            self.sync();
            return;
        }
        self.mode.set(new);
        self.sync();
        self.show_error("");
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let info = gio::FileInfo::new();
        info.set_attribute_uint32("unix::mode", new & 0o7777);
        let weak = Rc::downgrade(self);
        self.file.set_attributes_async(
            &info,
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
            None::<&gio::Cancellable>,
            move |r| {
                let Some(p) = weak.upgrade() else { return };
                let err = r.err().map(|e| format!("Could not change permissions: {}", e.message()));
                p.read_back(generation, new, err);
            },
        );
    }

    fn read_back(self: &Rc<Self>, generation: u32, wanted: u32, err: Option<String>) {
        let weak = Rc::downgrade(self);
        self.file.query_info_async(
            "unix::mode",
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
            None::<&gio::Cancellable>,
            move |r| {
                let Some(p) = weak.upgrade() else { return };
                if p.generation.get() != generation {
                    return;
                }
                let actual = r.ok().filter(|i| i.has_attribute("unix::mode")).map(|i| i.attribute_uint32("unix::mode"));
                if let Some(m) = actual {
                    p.mode.set(m);
                }
                p.sync();
                let msg = match (err, actual) {
                    (Some(e), _) => e,
                    (None, Some(m)) if m & 0o7777 != wanted & 0o7777 => {
                        "The file system did not apply this change.".to_string()
                    }
                    _ => String::new(),
                };
                p.show_error(&msg);
            },
        );
    }
}

fn perms_section(file: &gio::File, info: &gio::FileInfo, owner: &str) -> (gtk::Box, Rc<Perms>) {
    let mode = info.attribute_uint32("unix::mode");
    let is_dir = info.file_type() == gio::FileType::Directory;
    let user = glib::user_name();
    let editable = !owner.is_empty() && (user.to_str() == Some(owner) || user.to_str() == Some("root"));

    let bx = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let head = gtk::Label::new(Some("PERMISSIONS"));
    head.add_css_class("props-head");
    head.set_xalign(0.0);
    bx.append(&head);

    let grid = gtk::Grid::builder().row_spacing(2).column_spacing(18).build();
    let cols = if is_dir { ["Read", "Write", "Enter"] } else { ["Read", "Write", "Execute"] };
    for (i, c) in cols.iter().enumerate() {
        let l = gtk::Label::new(Some(c));
        l.add_css_class("props-key");
        grid.attach(&l, i as i32 + 1, 0, 1, 1);
    }
    let group = info.attribute_string("owner::group").map(|s| s.to_string()).unwrap_or_default();
    let rows = [
        if owner.is_empty() { "Owner".to_string() } else { format!("Owner ({owner})") },
        if group.is_empty() { "Group".to_string() } else { format!("Group ({group})") },
        "Others".to_string(),
    ];
    let mut checks = Vec::with_capacity(9);
    for (r, name) in rows.iter().enumerate() {
        let l = gtk::Label::new(Some(name));
        l.set_xalign(0.0);
        l.set_hexpand(true);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        grid.attach(&l, 0, r as i32 + 1, 1, 1);
        for (p, col) in cols.iter().enumerate() {
            let c = gtk::CheckButton::new();
            c.set_halign(gtk::Align::Center);
            c.set_sensitive(editable);
            c.set_tooltip_text(Some(&format!("{name} – {col}")));
            grid.attach(&c, p as i32 + 1, r as i32 + 1, 1, 1);
            checks.push(c);
        }
    }
    bx.append(&grid);

    let exec = (info.file_type() == gio::FileType::Regular).then(|| {
        let c = gtk::CheckButton::with_label("Allow running as program");
        c.set_sensitive(editable);
        bx.append(&c);
        c
    });

    let mode_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let ml = gtk::Label::new(Some("Mode"));
    ml.add_css_class("props-key");
    let octal = gtk::Entry::builder().width_chars(5).max_width_chars(5).max_length(4).build();
    octal.add_css_class("props-octal");
    octal.set_sensitive(editable);
    octal.set_tooltip_text(Some("Octal mode, e.g. 644 or 755 (press Enter to apply)"));
    let symbolic = gtk::Label::new(None);
    symbolic.add_css_class("props-mono");
    symbolic.set_selectable(true);
    mode_row.append(&ml);
    mode_row.append(&octal);
    mode_row.append(&symbolic);
    bx.append(&mode_row);

    if !editable {
        let hint = gtk::Label::new(Some(&if owner.is_empty() {
            "Only the owner can change permissions.".to_string()
        } else {
            format!("Only {owner} can change these permissions.")
        }));
        hint.add_css_class("dim");
        hint.add_css_class("small");
        hint.set_xalign(0.0);
        bx.append(&hint);
    }
    let apply_btn = (is_dir && editable && file.path().is_some()).then(|| {
        let b = gtk::Button::with_label("Apply to enclosed files…");
        b.set_halign(gtk::Align::Start);
        b.set_tooltip_text(Some("Give everything inside this folder permissions based on the folder's"));
        bx.append(&b);
        b
    });
    let err = gtk::Label::new(None);
    err.add_css_class("error");
    err.set_xalign(0.0);
    err.set_wrap(true);
    err.set_visible(false);
    bx.append(&err);

    let p = Rc::new(Perms {
        file: file.clone(),
        mode: Cell::new(mode),
        checks,
        exec,
        octal,
        symbolic,
        err,
        syncing: Cell::new(false),
        generation: Cell::new(0),
    });
    p.sync();

    // Handlers hold weak references: the dialog owns `p`, not its widgets.
    for (c, s) in SHIFTS.iter().enumerate() {
        for (i, bit) in PERMS.iter().enumerate() {
            let bit = bit << s;
            let w = Rc::downgrade(&p);
            p.checks[c * 3 + i].connect_toggled(move |b| {
                let Some(p) = w.upgrade() else { return };
                if p.syncing.get() {
                    return;
                }
                let m = p.mode.get();
                p.apply(if b.is_active() { m | bit } else { m & !bit });
            });
        }
    }
    if let Some(x) = &p.exec {
        let w = Rc::downgrade(&p);
        x.connect_toggled(move |b| {
            let Some(p) = w.upgrade() else { return };
            if !p.syncing.get() {
                p.apply(set_exec(p.mode.get(), b.is_active()));
            }
        });
    }
    let w = Rc::downgrade(&p);
    p.octal.connect_activate(move |e| {
        let Some(p) = w.upgrade() else { return };
        match parse_octal(&e.text()) {
            Some(v) => p.apply((p.mode.get() & !0o7777) | v),
            None => p.show_error("Enter an octal mode such as 644 or 755."),
        }
    });
    if let (Some(b), Some(path)) = (apply_btn, file.path()) {
        let (w, name) = (Rc::downgrade(&p), file_title(file));
        b.connect_clicked(move |b| {
            let (Some(p), Some(win)) = (w.upgrade(), b.root().and_downcast::<gtk::Window>()) else { return };
            apply_to_enclosed(&win, path.clone(), &name, p.mode.get());
        });
    }
    (bx, p)
}

// ================= free space, open with =================

/// Adds the "Free space" row (and a usage bar for a mount point), filled in
/// asynchronously so a slow mount can't freeze the dialog. Stays hidden when
/// the file system does not report its size.
fn free_space_row(grid: &gtk::Grid, row: &mut i32, path: PathBuf) {
    let (key, value) = add_row(grid, row, "Free space", "");
    let bar = gtk::LevelBar::for_interval(0.0, 1.0);
    bar.set_hexpand(true);
    grid.attach(&bar, 1, *row, 1, 1);
    *row += 1;
    for w in [key.upcast_ref::<gtk::Widget>(), value.upcast_ref(), bar.upcast_ref()] {
        w.set_visible(false);
    }
    let (key, value, bar) = (key.downgrade(), value.downgrade(), bar.downgrade());
    glib::spawn_future_local(async move {
        let info = gio::spawn_blocking(move || {
            let attrs = "filesystem::size,filesystem::free";
            let info = gio::File::for_path(&path).query_filesystem_info(attrs, None::<&gio::Cancellable>).ok()?;
            if !info.has_attribute("filesystem::size") || !info.has_attribute("filesystem::free") {
                return None;
            }
            let (free, total) = (info.attribute_uint64("filesystem::free"), info.attribute_uint64("filesystem::size"));
            Some((free, total, is_mount_point(&path)))
        })
        .await
        .ok()
        .flatten();
        let (Some(key), Some(value), Some(bar)) = (key.upgrade(), value.upgrade(), bar.upgrade()) else { return };
        let Some((free, total, mount)) = info else { return };
        let Some(text) = free_space_text(free, total) else { return };
        value.set_text(&text);
        key.set_visible(true);
        value.set_visible(true);
        if mount {
            bar.set_value(used_fraction(free, total));
            bar.set_visible(true);
        }
    });
}

/// Apps for a content type: the registered ones first, then the others.
fn apps_for_type(ct: &str) -> Vec<gio::AppInfo> {
    let mut seen = HashSet::new();
    let mut apps = gio::AppInfo::all_for_type(ct);
    apps.extend(gio::AppInfo::fallback_for_type(ct));
    apps.retain(|a| a.should_show() && seen.insert(a.id().map(|i| i.to_string()).unwrap_or_else(|| a.name().to_string())));
    apps
}

/// Adds the "Opens with" row: the default app and a "Change…" button that
/// opens a small list of apps (the GTK app chooser widgets are deprecated).
fn open_with_row(grid: &gtk::Grid, row: &mut i32, ct: &str) {
    let (_, value) = add_row(grid, row, "Opens with", "");
    let default_name = gio::AppInfo::default_for_type(ct, false).map(|a| a.name().to_string());
    value.set_text(&default_name.unwrap_or_else(|| "No default app".into()));
    let button = gtk::MenuButton::builder().label("Change…").valign(gtk::Align::Center).build();
    // Put the label and the button side by side in the value cell.
    grid.remove(&value);
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    line.append(&value);
    line.append(&button);
    grid.attach(&line, 1, *row - 1, 1, 1);

    let (ct, value) = (ct.to_string(), value.downgrade());
    button.set_create_popup_func(move |mb| {
        let apps = apps_for_type(&ct);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_activate_on_single_click(true);
        for a in &apps {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.set_margin_top(4);
            line.set_margin_bottom(4);
            line.set_margin_start(6);
            line.set_margin_end(6);
            let img = gtk::Image::new();
            img.set_pixel_size(24);
            match a.icon() {
                Some(i) => img.set_from_gicon(&i),
                None => img.set_icon_name(Some("application-x-executable-symbolic")),
            }
            line.append(&img);
            line.append(&gtk::Label::new(Some(&a.name())));
            list.append(&line);
        }
        let err = gtk::Label::new(None);
        err.add_css_class("error");
        err.set_xalign(0.0);
        err.set_wrap(true);
        err.set_max_width_chars(34);
        err.set_visible(false);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(280)
            .child(&list)
            .build();
        let bx = gtk::Box::new(gtk::Orientation::Vertical, 6);
        if apps.is_empty() {
            let none = gtk::Label::new(Some("No apps found for this file type."));
            none.add_css_class("dim");
            bx.append(&none);
        } else {
            bx.append(&scroll);
        }
        bx.append(&err);
        let pop = gtk::Popover::new();
        pop.set_child(Some(&bx));
        let (ct, value, pop_weak, err) = (ct.clone(), value.clone(), pop.downgrade(), err.downgrade());
        list.connect_row_activated(move |_, r| {
            let (Some(app), Some(err)) = (apps.get(r.index() as usize), err.upgrade()) else { return };
            match app.set_as_default_for_type(&ct) {
                Ok(()) => {
                    if let Some(v) = value.upgrade() {
                        v.set_text(&app.name());
                    }
                    if let Some(p) = pop_weak.upgrade() {
                        p.popdown();
                    }
                }
                Err(e) => {
                    err.set_text(&format!("Could not change the default app: {}", e.message()));
                    err.set_visible(true);
                }
            }
        });
        mb.set_popover(Some(&pop));
    });
}

#[cfg(test)]
mod free_space_tests {
    use super::*;

    #[test]
    fn on_disk_rule() {
        assert_eq!(on_disk_text(1000, 4096), None);
        assert_eq!(on_disk_text(5000, 1000), None);
        assert_eq!(on_disk_text(0, 4096), None);
        assert_eq!(on_disk_text(0, 4097), Some(format!("{} on disk", glib::format_size(4097))));
        assert_eq!(on_disk_text(1_000_000, 1_300_000), Some(format!("{} on disk", glib::format_size(1_300_000))));
        // Sparse files: far less used than the apparent size.
        assert!(on_disk_text(10_000_000, 4096).is_some());
        assert_eq!(size_with_disk(100, 4096), size_text(100));
        let s = size_with_disk(1_234_567, 1_300_000);
        assert!(s.starts_with(&size_text(1_234_567)) && s.ends_with(" on disk") && s.contains(", "), "{s}");
    }

    #[test]
    fn free_space_rules() {
        assert_eq!(
            free_space_text(250, 1000),
            Some(format!("{} free of {}", glib::format_size(250), glib::format_size(1000)))
        );
        assert!(free_space_text(0, 1000).is_some());
        assert_eq!(free_space_text(0, 0), None);
        assert_eq!(free_space_text(2000, 1000), None);
        assert_eq!(used_fraction(250, 1000), 0.75);
        assert_eq!(used_fraction(0, 0), 0.0);
        assert_eq!(used_fraction(2000, 1000), 0.0);
        assert_eq!(used_fraction(0, 1000), 1.0);
    }

    #[test]
    fn mount_points() {
        assert!(is_mount_point(Path::new("/")));
        assert!(!is_mount_point(&std::env::temp_dir().join("rfiles-no-such-dir-xyz")));
    }

    #[test]
    fn walk_counts_disk_use_and_hard_links_once() {
        let root = std::env::temp_dir().join(format!("rfiles-props-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/f"), vec![1u8; 10_000]).unwrap();
        let run = |root: &Path| {
            let t = Totals::default();
            walk(&[root.to_path_buf()], true, &t, &AtomicBool::new(false));
            (t.bytes.load(Relaxed), t.disk.load(Relaxed))
        };
        let (bytes, disk) = run(&root);
        assert_eq!(bytes, 10_000);
        assert!(disk > 0);
        std::fs::hard_link(root.join("d/f"), root.join("link")).unwrap();
        assert_eq!(run(&root), (bytes, disk));
        std::fs::remove_dir_all(&root).unwrap();
    }
}

// ================= apply permissions to enclosed files =================

/// Progress of the apply walk, written by the worker and read by the UI.
#[derive(Default)]
struct ApplyCounts {
    seen: AtomicU64,
    changed: AtomicU64,
    failed: AtomicU64,
    done: AtomicBool,
}

/// Sets the permissions of everything inside `root` from `folder_mode` (see
/// `enclosed_mode`). Symlinks are never touched or followed, and folders on
/// other file systems are skipped. Folders are changed last, deepest first,
/// so a folder without the enter bit can't block the walk.
fn apply_walk(root: &Path, folder_mode: u32, c: &ApplyCounts, cancel: &AtomicBool) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(rm) = std::fs::metadata(root) else {
        c.failed.fetch_add(1, Relaxed);
        return;
    };
    let dev = rm.dev();
    let set = |path: &Path, old: u32, new: u32| {
        if new & 0o7777 == old & 0o7777 {
            return;
        }
        match std::fs::set_permissions(path, std::fs::Permissions::from_mode(new)) {
            Ok(()) => c.changed.fetch_add(1, Relaxed),
            Err(_) => c.failed.fetch_add(1, Relaxed),
        };
    };
    let mut stack = vec![root.to_path_buf()];
    let mut dirs: Vec<(PathBuf, u32)> = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            c.failed.fetch_add(1, Relaxed);
            continue;
        };
        for e in rd {
            if cancel.load(Relaxed) {
                return;
            }
            let Ok((e, m)) = e.and_then(|e| e.metadata().map(|m| (e, m))) else {
                c.failed.fetch_add(1, Relaxed);
                continue;
            };
            c.seen.fetch_add(1, Relaxed);
            let ft = m.file_type();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if m.dev() == dev {
                    stack.push(e.path());
                    dirs.push((e.path(), m.mode()));
                }
            } else if ft.is_file() {
                set(&e.path(), m.mode(), enclosed_mode(folder_mode, m.mode(), false));
            }
        }
    }
    for (path, old) in dirs.into_iter().rev() {
        if cancel.load(Relaxed) {
            return;
        }
        set(&path, old, enclosed_mode(folder_mode, old, true));
    }
}

/// Confirmation and progress window for "Apply to enclosed files…".
fn apply_to_enclosed(parent: &gtk::Window, path: PathBuf, name: &str, folder_mode: u32) {
    let win = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .title("Apply to enclosed files")
        .default_width(400)
        .resizable(false)
        .build();
    win.add_css_class("rfiles");
    let (dir_m, file_m, exec_m) = (
        octal_mode(enclosed_mode(folder_mode, 0, true)),
        octal_mode(enclosed_mode(folder_mode, 0, false)),
        octal_mode(enclosed_mode(folder_mode, 0o100, false)),
    );
    let files = if file_m == exec_m { file_m } else { format!("{file_m} ({exec_m} if already executable)") };
    let text = format!(
        "Set the permissions of everything inside “{name}” to match this folder?\n\nFolders get {dir_m}. Files get {files}. \
         Links are left alone and other file systems are skipped. Owner and group are not changed. \
         This cannot be undone."
    );
    let msg = gtk::Label::new(Some(&text));
    msg.set_wrap(true);
    msg.set_xalign(0.0);
    msg.set_max_width_chars(48);
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_visible(false);
    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.set_action_name(Some("window.close"));
    let apply = gtk::Button::with_label("Apply");
    apply.add_css_class("suggested-action");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&cancel_btn);
    buttons.append(&apply);
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 12);
    bx.set_margin_top(16);
    bx.set_margin_bottom(14);
    bx.set_margin_start(16);
    bx.set_margin_end(16);
    bx.append(&msg);
    bx.append(&status);
    bx.append(&buttons);
    win.set_child(Some(&bx));

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        win.connect_destroy(move |_| cancel.store(true, Relaxed));
    }
    let (status_w, apply_w, cancel_btn_w) = (status.downgrade(), apply.downgrade(), cancel_btn.downgrade());
    apply.connect_clicked(move |b| {
        b.set_sensitive(false);
        let c = Arc::new(ApplyCounts::default());
        {
            let (c, cancel, path) = (c.clone(), cancel.clone(), path.clone());
            let spawned = std::thread::Builder::new().name("rfiles-chmod".into()).spawn(move || {
                apply_walk(&path, folder_mode, &c, &cancel);
                c.done.store(true, Relaxed);
            });
            if spawned.is_err() {
                if let Some(s) = status_w.upgrade() {
                    s.set_text("Could not start.");
                    s.set_visible(true);
                }
                return;
            }
        }
        let (status_w, apply_w, cancel_btn_w, cancel) = (status_w.clone(), apply_w.clone(), cancel_btn_w.clone(), cancel.clone());
        let update = move || -> glib::ControlFlow {
            let (Some(status), Some(apply), Some(close)) = (status_w.upgrade(), apply_w.upgrade(), cancel_btn_w.upgrade())
            else {
                cancel.store(true, Relaxed);
                return glib::ControlFlow::Break;
            };
            let done = c.done.load(Relaxed);
            let (seen, changed, failed) = (c.seen.load(Relaxed), c.changed.load(Relaxed), c.failed.load(Relaxed));
            status.set_visible(true);
            if done {
                status.set_text(&format!(
                    "{} changed, {} failed ({} checked).",
                    group_digits(changed),
                    group_digits(failed),
                    group_digits(seen)
                ));
                apply.set_visible(false);
                close.set_label("Close");
                glib::ControlFlow::Break
            } else {
                status.set_text(&format!("Working… {} checked, {} changed.", group_digits(seen), group_digits(changed)));
                glib::ControlFlow::Continue
            }
        };
        if update() == glib::ControlFlow::Continue {
            glib::timeout_add_local(TICK, update);
        }
    });
    win.present();
}

#[cfg(test)]
mod enclosed_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn enclosed_mode_rules() {
        // 755 -> folders 755, plain files 644, executable files 755
        assert_eq!(enclosed_mode(0o755, 0o600, true), 0o755);
        assert_eq!(enclosed_mode(0o755, 0o600, false), 0o644);
        assert_eq!(enclosed_mode(0o755, 0o644, false), 0o644);
        assert_eq!(enclosed_mode(0o755, 0o700, false), 0o755);
        assert_eq!(enclosed_mode(0o755, 0o001, false), 0o755);
        // 700 -> 600 / 700
        assert_eq!(enclosed_mode(0o700, 0o644, false), 0o600);
        assert_eq!(enclosed_mode(0o700, 0o755, false), 0o700);
        assert_eq!(enclosed_mode(0o700, 0o644, true), 0o700);
        // 775, 770, 750, 000
        assert_eq!(enclosed_mode(0o775, 0o664, false), 0o664);
        assert_eq!(enclosed_mode(0o750, 0o600, false), 0o640);
        assert_eq!(enclosed_mode(0o000, 0o644, false), 0o000);
        assert_eq!(enclosed_mode(0o000, 0o755, true), 0o000);
    }

    #[test]
    fn special_bits_are_never_propagated() {
        for folder in [0o4755, 0o2755, 0o1755, 0o7755, 0o7700] {
            let f = enclosed_mode(folder, 0o600, false);
            assert_eq!(f & 0o7000, 0, "{folder:o}");
            let x = enclosed_mode(folder, 0o700, false);
            assert_eq!(x & 0o7000, 0, "{folder:o}");
            assert_eq!(enclosed_mode(folder, 0o644, true) & 0o7000, 0, "{folder:o}");
        }
        // An entry keeps the special bits it already has.
        assert_eq!(enclosed_mode(0o755, 0o2755, true), 0o2755);
        assert_eq!(enclosed_mode(0o755, 0o4700, false), 0o4755);
        // Result never includes file type bits.
        assert_eq!(enclosed_mode(0o100755, 0o100600, false), 0o644);
    }

    #[test]
    fn apply_walk_on_temp_tree() {
        let root = std::env::temp_dir().join(format!("rfiles-props-chmod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let (plain, exe, deep) = (root.join("plain"), root.join("exe"), sub.join("deep"));
        for f in [&plain, &exe, &deep] {
            std::fs::write(f, b"x").unwrap();
        }
        let chmod = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        chmod(&plain, 0o666);
        chmod(&exe, 0o700);
        chmod(&deep, 0o664);
        chmod(&sub, 0o777);
        // A link outside must not be followed or touched.
        let outside = std::env::temp_dir().join(format!("rfiles-props-chmod-out-{}", std::process::id()));
        std::fs::write(&outside, b"x").unwrap();
        chmod(&outside, 0o666);
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let c = ApplyCounts::default();
        apply_walk(&root, 0o750, &c, &AtomicBool::new(false));
        assert_eq!(mode(&plain), 0o640);
        assert_eq!(mode(&exe), 0o750);
        assert_eq!(mode(&deep), 0o640);
        assert_eq!(mode(&sub), 0o750);
        assert_eq!(mode(&root.join("a")) & 0o777, 0o750);
        assert_eq!(mode(&outside), 0o666);
        assert_eq!(c.failed.load(Relaxed), 0);
        assert_eq!(c.changed.load(Relaxed), 5);

        // Nothing left to change on a second run.
        let c = ApplyCounts::default();
        apply_walk(&root, 0o750, &c, &AtomicBool::new(false));
        assert_eq!(c.changed.load(Relaxed), 0);

        // A folder mode without the enter bit still reaches everything.
        let c = ApplyCounts::default();
        apply_walk(&root, 0o600, &c, &AtomicBool::new(false));
        assert_eq!(c.failed.load(Relaxed), 0);
        // (the folders can't be entered any more, so look one level at a time)
        assert_eq!(mode(&root.join("a")), 0o600);
        chmod(&root.join("a"), 0o755);
        assert_eq!(mode(&sub), 0o600);
        chmod(&sub, 0o755);
        assert_eq!(mode(&deep), 0o600);

        // A cancelled walk changes nothing.
        let c = ApplyCounts::default();
        apply_walk(&root, 0o755, &c, &AtomicBool::new(true));
        assert_eq!(c.changed.load(Relaxed), 0);

        for d in [&sub, &root.join("a")] {
            chmod(d, 0o755);
        }
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_file(&outside).unwrap();
    }
}

// ================= dialog =================

/// Adds "key  value" to the info grid and returns the value label.
fn add_row(grid: &gtk::Grid, row: &mut i32, key: &str, value: &str) -> (gtk::Label, gtk::Label) {
    let k = gtk::Label::new(Some(key));
    k.add_css_class("props-key");
    k.set_xalign(1.0);
    k.set_yalign(0.0);
    let v = gtk::Label::new(Some(value));
    v.set_xalign(0.0);
    v.set_hexpand(true);
    v.set_wrap(true);
    v.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    v.set_max_width_chars(46);
    v.set_selectable(true);
    grid.attach(&k, 0, *row, 1, 1);
    grid.attach(&v, 1, *row, 1, 1);
    *row += 1;
    (k, v)
}

fn time_attr(info: &gio::FileInfo, attr: &str) -> Option<String> {
    if !info.has_attribute(attr) {
        return None;
    }
    let secs = i64::try_from(info.attribute_uint64(attr)).ok()?;
    let dt = glib::DateTime::from_unix_local(secs).ok()?;
    dt.format("%Y-%m-%d  %H:%M:%S").ok().map(|s| s.to_string())
}

/// Opens the Properties window for `files` (one or more).
pub fn show(parent: &impl IsA<gtk::Window>, files: Vec<gio::File>) {
    if files.is_empty() {
        return;
    }
    let win = gtk::Window::builder()
        .transient_for(parent)
        .destroy_with_parent(true)
        .title("Properties")
        .default_width(440)
        .resizable(true)
        .build();
    win.add_css_class("rfiles");
    win.add_css_class("props");
    let keys = gtk::ShortcutController::new();
    keys.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::NamedAction::new("window.close")),
    ));
    win.add_controller(keys);

    let cancel = Arc::new(AtomicBool::new(false));
    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.set_margin_top(16);
    body.set_margin_bottom(12);
    body.set_margin_start(16);
    body.set_margin_end(16);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let icon = gtk::Image::new();
    icon.set_pixel_size(64);
    let name = gtk::Label::new(None);
    name.add_css_class("props-name");
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_wrap(true);
    name.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    name.set_max_width_chars(36);
    name.set_selectable(true);
    header.append(&icon);
    header.append(&name);
    body.append(&header);

    let grid = gtk::Grid::builder().row_spacing(5).column_spacing(14).build();
    body.append(&grid);
    let mut row = 0;
    let mut perms = None;

    if files.len() == 1 {
        let file = &files[0];
        match query_following(file) {
            Err(e) => {
                icon.set_icon_name(Some("dialog-error-symbolic"));
                name.set_text(&file_title(file));
                add_row(&grid, &mut row, "Error", e.message());
            }
            Ok((info, broken)) => {
                set_icon(&icon, &info, false);
                if let Some(p) = info.attribute_byte_string("thumbnail::path").filter(|p| !p.is_empty()) {
                    icon.set_from_gicon(&gio::FileIcon::new(&gio::File::for_path(p.as_str())));
                }
                name.set_text(&info.display_name());
                let ct = info.content_type().map(|c| c.to_string()).unwrap_or_default();
                let mut ty = type_description(&info);
                if !ct.is_empty() && info.file_type() != gio::FileType::Directory {
                    ty = format!("{ty} ({ct})");
                }
                add_row(&grid, &mut row, "Type", &ty);
                if info.is_symlink() {
                    if let Some(t) = info.symlink_target() {
                        add_row(&grid, &mut row, "Link to", &link_to_text(&t.to_string_lossy(), broken));
                    }
                }
                if let Some(parent) = file.parent() {
                    add_row(&grid, &mut row, "Location", &display_path(&parent));
                }
                let is_dir = info.file_type() == gio::FileType::Directory;
                if is_dir {
                    let (_, size) = add_row(&grid, &mut row, "Size", "");
                    let (ck, contents) = add_row(&grid, &mut row, "Contents", "");
                    match file.path() {
                        Some(p) => {
                            free_space_row(&grid, &mut row, p.clone());
                            start_walk(vec![p], true, false, &size, &contents, &ck, cancel.clone());
                        }
                        None => {
                            size.set_text("Unknown (not a local folder)");
                            ck.set_visible(false);
                            contents.set_visible(false);
                        }
                    }
                } else if !is_dir_like(&info) && !broken {
                    let bytes = info.size().max(0) as u64;
                    let disk = info.attribute_uint64("unix::blocks").saturating_mul(512);
                    add_row(&grid, &mut row, "Size", &size_with_disk(bytes, disk));
                }
                // Not for folders, and not when the type is unknown.
                if info.file_type() == gio::FileType::Regular && !broken && !ct.is_empty() && ct != "application/octet-stream" {
                    open_with_row(&grid, &mut row, &ct);
                }
                if ct.starts_with("image/") {
                    if let Some(path) = file.path() {
                        let (k, v) = add_row(&grid, &mut row, "Dimensions", "…");
                        image_dimensions(path, &k, &v);
                    }
                }
                for (key, attr) in [("Modified", "time::modified"), ("Accessed", "time::access"), ("Created", "time::created")] {
                    if let Some(t) = time_attr(&info, attr) {
                        add_row(&grid, &mut row, key, &t);
                    }
                }
                let owner = info.attribute_string("owner::user").map(|s| s.to_string()).unwrap_or_default();
                if !owner.is_empty() {
                    let real = info.attribute_string("owner::user-real").map(|s| s.to_string()).unwrap_or_default();
                    let o = if real.is_empty() || real == owner { owner.clone() } else { format!("{owner} ({real})") };
                    add_row(&grid, &mut row, "Owner", &o);
                }
                if let Some(g) = info.attribute_string("owner::group").filter(|g| !g.is_empty()) {
                    add_row(&grid, &mut row, "Group", &g);
                }
                // Permissions of a broken link are not editable: apply and read-back follow links.
                if info.has_attribute("unix::mode") && !broken {
                    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                    let (section, p) = perms_section(file, &info, &owner);
                    if info.is_symlink() {
                        let note = gtk::Label::new(Some("These are the permissions of the link's target."));
                        note.add_css_class("dim");
                        note.add_css_class("small");
                        note.set_xalign(0.0);
                        section.insert_child_after(&note, section.first_child().as_ref());
                    }
                    body.append(&section);
                    perms = Some(p);
                }
            }
        }
    } else {
        icon.set_icon_name(Some("edit-copy-symbolic"));
        name.set_text(&format!("{} items", group_digits(files.len() as u64)));
        let first_parent = files[0].parent();
        let same = files.iter().all(|f| match (f.parent(), &first_parent) {
            (Some(a), Some(b)) => a.equal(b),
            (None, None) => true,
            _ => false,
        });
        let loc = match first_parent.filter(|_| same) {
            Some(p) => display_path(&p),
            None => "Various locations".into(),
        };
        add_row(&grid, &mut row, "Location", &loc);
        let (_, size) = add_row(&grid, &mut row, "Total size", "");
        let (ck, contents) = add_row(&grid, &mut row, "Inside folders", "");
        let paths: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
        if paths.len() < files.len() {
            add_row(&grid, &mut row, "Note", "Items that are not local files are not counted.");
        }
        start_walk(paths, false, true, &size, &contents, &ck, cancel.clone());
    }

    let close = gtk::Button::with_label("Close");
    close.set_action_name(Some("window.close"));
    close.set_halign(gtk::Align::End);
    body.append(&close);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&body)
        .build();
    win.set_child(Some(&scroll));
    // The window owns the permission state; everything else holds it weakly,
    // so closing the window frees it and stops the size walk.
    win.connect_destroy(move |_| {
        cancel.store(true, Relaxed);
        let _ = &perms;
    });
    win.present();
    close.grab_focus();
    // The first selectable label got focus on present and selected its text.
    name.select_region(0, 0);
}

/// Reads the image size from the file header on a worker thread.
fn image_dimensions(path: PathBuf, key: &gtk::Label, value: &gtk::Label) {
    let (key, value) = (key.downgrade(), value.downgrade());
    glib::spawn_future_local(async move {
        let dims = gio::spawn_blocking(move || read_dimensions(&path)).await.ok().flatten();
        let (Some(k), Some(v)) = (key.upgrade(), value.upgrade()) else { return };
        match dims {
            Some((w, h)) => v.set_text(&format!("{w} × {h} pixels")),
            None => {
                k.set_visible(false);
                v.set_visible(false);
            }
        }
    });
}

fn read_dimensions(path: &Path) -> Option<(i32, i32)> {
    gdk_pixbuf::Pixbuf::file_info(path).map(|(_, w, h)| (w, h)).filter(|&(w, h)| w > 0 && h > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbolic_and_octal() {
        assert_eq!(symbolic_mode(0o100644), "-rw-r--r--");
        assert_eq!(symbolic_mode(0o040755), "drwxr-xr-x");
        assert_eq!(symbolic_mode(0o104755), "-rwsr-xr-x");
        assert_eq!(symbolic_mode(0o041777), "drwxrwxrwt");
        assert_eq!(symbolic_mode(0o102640), "-rw-r-S---");
        assert_eq!(symbolic_mode(0o120777), "lrwxrwxrwx");
        assert_eq!(octal_mode(0o100644), "644");
        assert_eq!(octal_mode(0o104755), "4755");
        assert_eq!(octal_mode(0o100007), "007");
    }

    #[test]
    fn parse_octal_modes() {
        assert_eq!(parse_octal("644"), Some(0o644));
        assert_eq!(parse_octal(" 0755 "), Some(0o755));
        assert_eq!(parse_octal("4755"), Some(0o4755));
        assert_eq!(parse_octal("7"), Some(0o7));
        assert_eq!(parse_octal(""), None);
        assert_eq!(parse_octal("648"), None);
        assert_eq!(parse_octal("12345"), None);
        assert_eq!(parse_octal("-1"), None);
        assert_eq!(parse_octal("rwx"), None);
    }

    #[test]
    fn exec_toggle() {
        assert_eq!(set_exec(0o644, true), 0o755);
        assert_eq!(set_exec(0o600, true), 0o700);
        assert_eq!(set_exec(0o200, true), 0o300);
        assert_eq!(set_exec(0o755, false), 0o644);
        assert_eq!(set_exec(0o104755, false), 0o104644);
    }

    #[test]
    fn digits_and_sizes() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1234567), "1,234,567");
        assert_eq!(size_text(1), "1 byte");
        assert_eq!(size_text(512), "512 bytes");
        assert!(size_text(1_234_567).ends_with("(1,234,567 bytes)"));
    }

    #[test]
    fn link_to_row_text() {
        assert_eq!(link_to_text("/a/b", false), "/a/b");
        assert_eq!(link_to_text("gone", true), "gone (broken)");
    }

    #[test]
    fn broken_symlink_queries() {
        let root = std::env::temp_dir().join(format!("rfiles-props-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink("missing-target", root.join("broken")).unwrap();
        std::os::unix::fs::symlink("real", root.join("good")).unwrap();
        let (c, nofollow) = (None::<&gio::Cancellable>, gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS);
        let broken = gio::File::for_path(root.join("broken"));
        let f = broken.query_info(PROPS_ATTRS, gio::FileQueryInfoFlags::NONE, c);
        // Local files: a followed query on a broken link succeeds and describes the link itself.
        assert!(f.as_ref().map_or(true, is_broken_link));
        let good = gio::File::for_path(root.join("good"));
        assert!(!is_broken_link(&good.query_info(PROPS_ATTRS, gio::FileQueryInfoFlags::NONE, c).unwrap()));
        let i = broken.query_info(PROPS_ATTRS, nofollow, c).unwrap();
        assert!(i.is_symlink());
        assert_eq!(i.symlink_target().unwrap().to_string_lossy(), "missing-target");
        let (i, is_broken) = query_following(&broken).unwrap();
        assert!(is_broken && i.is_symlink());
        let (i, is_broken) = query_following(&gio::File::for_path(root.join("good"))).unwrap();
        assert!(!is_broken && i.is_symlink() && i.size() == 1);
        // A missing plain path stays an error.
        assert!(query_following(&gio::File::for_path(root.join("nope"))).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn walk_temp_tree() {
        let root = std::env::temp_dir().join(format!("rfiles-props-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("one"), vec![0u8; 100]).unwrap();
        std::fs::write(sub.join("two"), vec![0u8; 1000]).unwrap();
        // A hard link is counted once.
        std::fs::hard_link(sub.join("two"), root.join("two-link")).unwrap();
        // A symlink to a big folder outside must not be followed.
        let outside = std::env::temp_dir().join(format!("rfiles-props-out-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("big"), vec![0u8; 50_000]).unwrap();
        let target = outside.to_string_lossy().into_owned();
        std::os::unix::fs::symlink(&target, root.join("link")).unwrap();

        let t = Totals::default();
        walk(std::slice::from_ref(&root), true, &t, &AtomicBool::new(false));
        // one, a, a/b, a/b/two, two-link, link
        assert_eq!(t.items.load(Relaxed), 6);
        assert_eq!(t.bytes.load(Relaxed), 100 + 1000 + target.len() as u64);
        assert_eq!(t.errors.load(Relaxed), 0);

        // Several roots: a file root counts its size, not as an item.
        let t = Totals::default();
        walk(&[root.join("one"), root.join("a")], false, &t, &AtomicBool::new(false));
        assert_eq!(t.items.load(Relaxed), 2);
        assert_eq!(t.bytes.load(Relaxed), 1100);

        // The symlink itself as a root: followed only when asked.
        let t = Totals::default();
        walk(&[root.join("link")], true, &t, &AtomicBool::new(false));
        assert_eq!(t.bytes.load(Relaxed), 50_000);

        // Cancelled walks stop at once.
        let t = Totals::default();
        walk(std::slice::from_ref(&root), true, &t, &AtomicBool::new(true));
        assert_eq!(t.items.load(Relaxed), 0);

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }
}
