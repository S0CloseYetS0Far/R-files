//! Batch rename: rename several selected items at once, either by plain
//! text find & replace or with a numbered template ("Holiday ###").
//!
//! The planning and ordering logic is pure (no GTK, no IO) and unit tested;
//! the dialog only feeds it names and shows the result.

use crate::util::{is_dir_like, item_file, split_ext};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::time::Duration;

// ======================= pure planning =======================

/// Longest file name (in bytes) most Linux file systems accept.
const NAME_MAX: usize = 255;

/// How new names are made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Plain text find & replace over the whole name. An empty `find`
    /// changes nothing.
    Replace { find: String, with: String, case_sensitive: bool },
    /// Every run of `#` becomes the item's number, zero-padded to the run's
    /// length; files keep their extension. Without any `#` the number is
    /// appended (after a space unless the template ends with one). An empty
    /// template changes nothing.
    Template { template: String, start: u64 },
}

/// Why an item can't get its new name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    Empty,
    InvalidChar,
    Reserved,
    TooLong,
    Duplicate,
    Exists,
}

impl Problem {
    pub fn message(self) -> &'static str {
        match self {
            Problem::Empty => "Name is empty",
            Problem::InvalidChar => "Contains “/” or a NUL character",
            Problem::Reserved => "“.” and “..” are not allowed",
            Problem::TooLong => "Name is too long",
            Problem::Duplicate => "Same name as another item",
            Problem::Exists => "A file with this name already exists",
        }
    }
}

/// One item of the batch. `dir` indexes the folder the item is in (items
/// from search results can live in different folders).
#[derive(Debug, Clone)]
pub struct Item {
    pub name: String,
    pub is_dir: bool,
    pub dir: usize,
}

/// Length in bytes of the prefix of `s` that equals `needle` (already
/// lowercased) when compared case-insensitively, if any.
fn match_ci(s: &str, needle: &[char]) -> Option<usize> {
    let mut k = 0;
    for (i, c) in s.char_indices() {
        for lc in c.to_lowercase() {
            if needle.get(k) != Some(&lc) {
                return None;
            }
            k += 1;
        }
        if k == needle.len() {
            return Some(i + c.len_utf8());
        }
    }
    None
}

fn replace_ci(hay: &str, needle: &[char], with: &str) -> String {
    let mut out = String::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if let Some(n) = match_ci(&hay[i..], needle) {
            out.push_str(with);
            i += n;
        } else {
            let c = hay[i..].chars().next().unwrap_or_default();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn fill_template(template: &str, n: u64, out: &mut String) {
    use std::fmt::Write;
    let mut run = 0usize;
    let mut numbered = false;
    // Writing to a String can't fail.
    let mut flush = |run: &mut usize, out: &mut String| {
        if *run > 0 {
            let _ = write!(out, "{n:0w$}", w = *run);
            *run = 0;
            numbered = true;
        }
    };
    for c in template.chars() {
        if c == '#' {
            run += 1;
        } else {
            flush(&mut run, out);
            out.push(c);
        }
    }
    flush(&mut run, out);
    if !numbered {
        if !template.ends_with(char::is_whitespace) {
            out.push(' ');
        }
        let _ = write!(out, "{n}");
    }
}

/// Mode-specific preparation done once per plan, not once per item.
enum Prepared<'a> {
    Nothing,
    Replace { find: &'a str, lower: Option<Vec<char>>, with: &'a str },
    Template { template: &'a str, start: u64 },
}

impl<'a> Prepared<'a> {
    fn new(mode: &'a Mode) -> Self {
        match mode {
            Mode::Replace { find, .. } | Mode::Template { template: find, .. } if find.is_empty() => Prepared::Nothing,
            Mode::Replace { find, with, case_sensitive } => Prepared::Replace {
                find,
                lower: (!case_sensitive).then(|| find.chars().flat_map(char::to_lowercase).collect()),
                with,
            },
            Mode::Template { template, start } => Prepared::Template { template, start: *start },
        }
    }

    fn apply(&self, item: &Item, index: usize) -> String {
        match self {
            Prepared::Nothing => item.name.clone(),
            Prepared::Replace { find, lower: None, with } => item.name.replace(find, with),
            Prepared::Replace { lower: Some(l), with, .. } => replace_ci(&item.name, l, with),
            Prepared::Template { template, start } => {
                let ext = if item.is_dir { "" } else { split_ext(&item.name).1 };
                let mut s = String::with_capacity(template.len() + ext.len() + 8);
                fill_template(template, start.saturating_add(index as u64), &mut s);
                s.push_str(ext);
                s
            }
        }
    }
}

fn check_name(name: &str) -> Result<(), Problem> {
    if name.is_empty() {
        Err(Problem::Empty)
    } else if name.contains(['/', '\0']) {
        Err(Problem::InvalidChar)
    } else if name == "." || name == ".." {
        Err(Problem::Reserved)
    } else if name.len() > NAME_MAX {
        Err(Problem::TooLong)
    } else {
        Ok(())
    }
}

/// The name each item would get, before any checks. Items are numbered in
/// slice order.
pub fn new_names(items: &[Item], mode: &Mode) -> Vec<String> {
    let prep = Prepared::new(mode);
    items.iter().enumerate().map(|(i, it)| prep.apply(it, i)).collect()
}

/// Checks the names from [`new_names`]. `existing[dir]` holds all names
/// currently in that folder (batch items included).
pub fn problems(items: &[Item], names: &[String], existing: &[HashSet<String>]) -> Vec<Option<Problem>> {
    // Final names must be unique per folder (unchanged items included).
    let mut count: HashMap<(usize, &str), u32> = HashMap::with_capacity(items.len());
    for (it, n) in items.iter().zip(names) {
        *count.entry((it.dir, n.as_str())).or_default() += 1;
    }
    let old: HashSet<(usize, &str)> = items.iter().map(|it| (it.dir, it.name.as_str())).collect();
    items
        .iter()
        .zip(names)
        .map(|(it, n)| {
            if *n == it.name {
                // Unchanged: skipped, but still clashes with a new duplicate.
                return (count[&(it.dir, n.as_str())] > 1).then_some(Problem::Duplicate);
            }
            if let Err(p) = check_name(n) {
                Some(p)
            } else if count[&(it.dir, n.as_str())] > 1 {
                Some(Problem::Duplicate)
            } else if !old.contains(&(it.dir, n.as_str())) && existing.get(it.dir).is_some_and(|e| e.contains(n)) {
                // Names of other batch items are fine: they move away first.
                Some(Problem::Exists)
            } else {
                None
            }
        })
        .collect()
}

/// Works out the new name of every item: `Ok(name)` equal to the old name
/// means "unchanged, skip".
pub fn plan(items: &[Item], mode: &Mode, existing: &[HashSet<String>]) -> Vec<Result<String, Problem>> {
    let names = new_names(items, mode);
    let probs = problems(items, &names, existing);
    names.into_iter().zip(probs).map(|(n, p)| p.map_or(Ok(n), Err)).collect()
}

/// One rename step; `index` points into the pairs given to [`schedule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub index: usize,
    pub from: String,
    pub to: String,
}

/// Orders the renames of one folder so no step ever targets a name that is
/// still in use. `pairs` are (old, new) with distinct old and distinct new
/// names, where every new name is either free or the old name of another
/// pair. Cycles (a→b, b→a) are broken with a temporary name for which
/// `taken` returns false.
pub fn schedule(pairs: &[(&str, &str)], taken: impl Fn(&str) -> bool) -> Vec<Move> {
    let by_old: HashMap<&str, usize> = pairs.iter().enumerate().map(|(i, p)| (p.0, i)).collect();
    // blocker[i] = the pair whose old name item i wants; it must move first.
    let blocker: Vec<Option<usize>> = pairs
        .iter()
        .enumerate()
        .map(|(i, p)| by_old.get(p.1).copied().filter(|&j| j != i))
        .collect();
    let mut done = vec![false; pairs.len()];
    let mut on_path = vec![false; pairs.len()];
    let mut steps = Vec::with_capacity(pairs.len() + 2);
    let mut temps = 0usize;
    let mut path = Vec::new();
    for start in 0..pairs.len() {
        if done[start] {
            continue;
        }
        // Follow the chain of blockers. New names are unique, so each item
        // blocks at most one other: chains are simple paths or cycles.
        path.clear();
        let mut cur = start;
        let mut cycle = false;
        loop {
            path.push(cur);
            on_path[cur] = true;
            match blocker[cur] {
                Some(j) if on_path[j] => {
                    cycle = true;
                    break;
                }
                Some(j) if !done[j] => cur = j,
                _ => break,
            }
        }
        let mv = |i: usize, from: &str, to: &str| Move { index: i, from: from.into(), to: to.into() };
        if cycle {
            // path[0] wants path[1]'s name, ..., the last wants path[0]'s.
            let first = path[0];
            let tmp = loop {
                let t = format!(".rfiles-rename-{temps}.tmp");
                temps += 1;
                if !taken(&t) {
                    break t;
                }
            };
            steps.push(mv(first, pairs[first].0, &tmp));
            for &i in path[1..].iter().rev() {
                steps.push(mv(i, pairs[i].0, pairs[i].1));
            }
            steps.push(mv(first, &tmp, pairs[first].1));
        } else {
            for &i in path.iter().rev() {
                steps.push(mv(i, pairs[i].0, pairs[i].1));
            }
        }
        for &i in &path {
            done[i] = true;
            on_path[i] = false;
        }
    }
    steps
}

// ======================= preview row object =======================

mod row {
    use gtk::glib;
    use gtk::glib::prelude::*;
    use gtk::glib::subclass::prelude::*;
    use std::cell::{Cell, RefCell};

    pub const SAME: u32 = 0;
    pub const RENAME: u32 = 1;
    pub const BAD: u32 = 2;

    mod imp {
        use super::*;

        #[derive(Default, glib::Properties)]
        #[properties(wrapper_type = super::Row)]
        pub struct Row {
            #[property(get, set)]
            pub(super) old_name: RefCell<String>,
            #[property(get, set)]
            pub(super) new_name: RefCell<String>,
            #[property(get, set)]
            pub(super) problem: RefCell<String>,
            #[property(get, set)]
            pub(super) state: Cell<u32>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for Row {
            const NAME: &'static str = "RFilesBatchRenameRow";
            type Type = super::Row;
        }

        #[glib::derived_properties]
        impl ObjectImpl for Row {}
    }

    glib::wrapper! {
        pub struct Row(ObjectSubclass<imp::Row>);
    }

    impl Row {
        pub fn new(old: &str) -> Self {
            glib::Object::builder()
                .property("old-name", old)
                .property("new-name", old)
                .build()
        }

        /// Sets the values, notifying (and so redrawing) only what changed.
        pub fn update(&self, new: &str, problem: &str, state: u32) {
            let imp = self.imp();
            if *imp.new_name.borrow() != new {
                self.set_new_name(new);
            }
            if *imp.problem.borrow() != problem {
                self.set_problem(problem);
            }
            if imp.state.get() != state {
                self.set_state(state);
            }
        }
    }
}

use row::Row;

// ======================= dialog =======================

/// Above this many items, the preview is recomputed after a short pause in
/// typing instead of on every keystroke.
const DEBOUNCE_ABOVE: usize = 300;
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Told the (old URI, new URI) of each item after a successful batch.
type OnRenamed = Box<dyn Fn(&[(String, String)])>;

struct Dialog {
    win: gtk::Window,
    items: Vec<Item>,
    files: Vec<gio::File>,
    /// Folder of each `dir` index.
    dirs: Vec<gio::File>,
    existing: RefCell<Vec<HashSet<String>>>,
    rows: Vec<Row>,
    /// Set when undoing a failed batch failed too: names on disk no longer
    /// match the items, so renaming again is refused.
    broken: Cell<bool>,
    on_renamed: OnRenamed,
    template_mode: gtk::ToggleButton,
    find: gtk::Entry,
    with: gtk::Entry,
    case: gtk::CheckButton,
    template: gtk::Entry,
    start: gtk::SpinButton,
    summary: gtk::Label,
    error: gtk::Label,
    ok: gtk::Button,
    pending: RefCell<Option<glib::SourceId>>,
}

/// All names in `dir`, hidden ones included.
fn names_in(dir: &gio::File) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(en) = dir.enumerate_children(
        "standard::display-name",
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        None::<&gio::Cancellable>,
    ) {
        while let Ok(Some(info)) = en.next_file(None::<&gio::Cancellable>) {
            set.insert(info.display_name().to_string());
        }
    }
    set
}

fn labeled(text: &str, w: &impl IsA<gtk::Widget>) -> gtk::Box {
    let bx = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_width_chars(12);
    l.add_css_class("dim");
    bx.append(&l);
    w.set_hexpand(true);
    bx.append(w);
    bx
}

fn preview_factory() -> gtk::SignalListItemFactory {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(|_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
        // Two equal columns: "old" | "→ new" with the problem under it.
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bx.set_homogeneous(true);
        let mk = || {
            let l = gtk::Label::new(None);
            l.set_xalign(0.0);
            l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            l.set_width_chars(8);
            l
        };
        let (old, new, problem) = (mk(), mk(), mk());
        let arrow = gtk::Label::new(Some("→"));
        arrow.add_css_class("br-arrow");
        arrow.set_valign(gtk::Align::Start);
        problem.add_css_class("error");
        problem.add_css_class("small");
        let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
        right.append(&new);
        right.append(&problem);
        let rbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        rbox.append(&arrow);
        rbox.append(&right);
        bx.append(&old);
        bx.append(&rbox);
        // Expressions use `this` = the list item, so nothing holds the
        // list item strongly and rows rebind automatically.
        let item = gtk::ListItem::this_expression("item");
        item.chain_property::<Row>("old-name").bind(&old, "label", Some(li));
        item.chain_property::<Row>("new-name").bind(&new, "label", Some(li));
        let p = item.chain_property::<Row>("problem");
        p.bind(&problem, "label", Some(li));
        p.chain_closure::<bool>(glib::closure!(|_: Option<glib::Object>, p: &str| !p.is_empty()))
            .bind(&problem, "visible", Some(li));
        item.chain_property::<Row>("state")
            .chain_closure::<Vec<String>>(glib::closure!(|_: Option<glib::Object>, s: u32| {
                let extra = match s {
                    row::BAD => "br-bad",
                    row::SAME => "br-same",
                    _ => "br-change",
                };
                vec!["br-row".to_string(), extra.to_string()]
            }))
            .bind(&bx, "css-classes", Some(li));
        li.set_child(Some(&bx));
        li.set_activatable(false);
    });
    f
}

/// Opens the batch rename dialog for `infos` (in display order). After a successful batch,
/// `on_renamed` gets the (old URI, new URI) of each renamed item.
pub fn open(
    parent: &impl IsA<gtk::Window>,
    infos: &[gio::FileInfo],
    on_renamed: impl Fn(&[(String, String)]) + 'static,
) {
    // Group items by folder; read each folder's names once for clash checks.
    let mut dir_index: HashMap<String, usize> = HashMap::new();
    let mut dirs = Vec::new();
    let mut items = Vec::with_capacity(infos.len());
    let mut files = Vec::with_capacity(infos.len());
    for info in infos {
        let file = item_file(info);
        let Some(parent_dir) = file.parent() else { continue };
        let dir = *dir_index.entry(parent_dir.uri().to_string()).or_insert_with(|| {
            dirs.push(parent_dir);
            dirs.len() - 1
        });
        items.push(Item { name: info.display_name().to_string(), is_dir: is_dir_like(info), dir });
        files.push(file);
    }
    if items.is_empty() {
        return;
    }
    let existing: Vec<HashSet<String>> = dirs.iter().map(names_in).collect();

    let win = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(format!("Rename {} Items", items.len()))
        .default_width(680)
        .default_height(540)
        .build();
    win.add_css_class("rfiles");
    win.add_css_class("batch-rename");

    // Mode switch
    let replace_mode = gtk::ToggleButton::with_label("Find & Replace");
    let template_mode = gtk::ToggleButton::with_label("Template");
    template_mode.set_group(Some(&replace_mode));
    replace_mode.set_active(true);
    let switch = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    switch.add_css_class("linked");
    switch.set_halign(gtk::Align::Center);
    switch.append(&replace_mode);
    switch.append(&template_mode);

    // Find & replace page
    let find = gtk::Entry::builder().placeholder_text("Text to find").build();
    let with = gtk::Entry::builder().placeholder_text("Replacement (may be empty)").build();
    let case = gtk::CheckButton::with_label("Match case");
    let rpage = gtk::Box::new(gtk::Orientation::Vertical, 8);
    rpage.append(&labeled("Find", &find));
    rpage.append(&labeled("Replace with", &with));
    rpage.append(&labeled("", &case));

    // Template page
    let template = gtk::Entry::builder().placeholder_text("Holiday ###").build();
    let start = gtk::SpinButton::with_range(0.0, 999_999_999.0, 1.0);
    start.set_value(1.0);
    let hint = gtk::Label::new(Some(
        "Each run of # is replaced by the number, padded to its length (### → 001). Files keep their extension.",
    ));
    hint.add_css_class("dim");
    hint.add_css_class("small");
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    let tpage = gtk::Box::new(gtk::Orientation::Vertical, 8);
    tpage.append(&labeled("Name", &template));
    let start_row = labeled("Start at", &start);
    start.set_hexpand(false);
    start.set_halign(gtk::Align::Start);
    tpage.append(&start_row);
    tpage.append(&hint);

    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false);
    stack.add_named(&rpage, Some("replace"));
    stack.add_named(&tpage, Some("template"));

    // Preview
    let rows: Vec<Row> = items.iter().map(|it| Row::new(&it.name)).collect();
    let store = gio::ListStore::new::<Row>();
    store.extend_from_slice(&rows);
    let list = gtk::ListView::new(Some(gtk::NoSelection::new(Some(store))), Some(preview_factory()));
    list.add_css_class("br-list");
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(220)
        .build();
    scroll.add_css_class("br-scroll");

    let summary = gtk::Label::new(None);
    summary.set_xalign(0.0);
    summary.add_css_class("dim");
    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.set_selectable(true);
    error.set_visible(false);

    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("Rename");
    ok.add_css_class("suggested-action");
    ok.set_sensitive(false);
    let btns = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btns.set_halign(gtk::Align::End);
    btns.append(&cancel);
    btns.append(&ok);

    let bx = gtk::Box::new(gtk::Orientation::Vertical, 12);
    bx.set_margin_top(16);
    bx.set_margin_bottom(16);
    bx.set_margin_start(16);
    bx.set_margin_end(16);
    for w in [switch.upcast_ref::<gtk::Widget>(), stack.upcast_ref(), scroll.upcast_ref(), summary.upcast_ref(), error.upcast_ref(), btns.upcast_ref()] {
        bx.append(w);
    }
    win.set_child(Some(&bx));

    let d = Rc::new(Dialog {
        win: win.clone(),
        items,
        files,
        dirs,
        existing: RefCell::new(existing),
        rows,
        broken: Cell::new(false),
        on_renamed: Box::new(on_renamed),
        template_mode: template_mode.clone(),
        find: find.clone(),
        with: with.clone(),
        case: case.clone(),
        template: template.clone(),
        start: start.clone(),
        summary,
        error,
        ok: ok.clone(),
        pending: RefCell::new(None),
    });

    // Every handler holds the dialog weakly; the only strong reference is
    // dropped when the window closes, so everything is freed then.
    let weak = |f: fn(&Rc<Dialog>)| {
        let w: Weak<Dialog> = Rc::downgrade(&d);
        move || {
            if let Some(d) = w.upgrade() {
                f(&d);
            }
        }
    };
    for e in [&find, &with, &template] {
        let f = weak(Dialog::changed);
        e.connect_changed(move |_| f());
        let f = weak(Dialog::submit);
        e.connect_activate(move |_| f());
    }
    let f = weak(Dialog::changed);
    case.connect_toggled(move |_| f());
    let f = weak(Dialog::changed);
    start.connect_value_changed(move |_| f());
    let f = weak(Dialog::submit);
    start.connect_activate(move |_| f());
    let s = stack.clone();
    let (fe, te) = (find.clone(), template.clone());
    let f = weak(Dialog::changed);
    template_mode.connect_toggled(move |b| {
        let t = b.is_active();
        s.set_visible_child_name(if t { "template" } else { "replace" });
        if t { te.grab_focus() } else { fe.grab_focus() };
        f();
    });
    let f = weak(Dialog::submit);
    ok.connect_clicked(move |_| f());
    let w = win.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(w) = w.upgrade() {
            w.close()
        }
    });

    let keys = gtk::EventControllerKey::new();
    let w = win.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(w) = w.upgrade() {
                w.close();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    win.add_controller(keys);

    let keep = RefCell::new(Some(d.clone()));
    win.connect_close_request(move |_| {
        if let Some(d) = keep.borrow_mut().take() {
            if let Some(id) = d.pending.borrow_mut().take() {
                id.remove();
            }
        }
        glib::Propagation::Proceed
    });

    d.recompute();
    win.present();
    find.grab_focus();
}

impl Dialog {
    fn mode(&self) -> Mode {
        if self.template_mode.is_active() {
            Mode::Template { template: self.template.text().to_string(), start: self.start.value_as_int().max(0) as u64 }
        } else {
            Mode::Replace {
                find: self.find.text().to_string(),
                with: self.with.text().to_string(),
                case_sensitive: self.case.is_active(),
            }
        }
    }

    fn changed(self: &Rc<Self>) {
        self.error.set_visible(false);
        if self.items.len() <= DEBOUNCE_ABOVE {
            self.recompute();
            return;
        }
        if let Some(id) = self.pending.borrow_mut().take() {
            id.remove();
        }
        let w = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(DEBOUNCE, move || {
            if let Some(d) = w.upgrade() {
                d.pending.borrow_mut().take();
                d.recompute();
            }
        });
        *self.pending.borrow_mut() = Some(id);
    }

    fn recompute(&self) {
        let names = new_names(&self.items, &self.mode());
        let probs = problems(&self.items, &names, &self.existing.borrow());
        let (mut changes, mut bad) = (0usize, 0usize);
        for (((row, it), n), p) in self.rows.iter().zip(&self.items).zip(&names).zip(&probs) {
            match p {
                Some(p) => {
                    bad += 1;
                    row.update(n, p.message(), row::BAD);
                }
                None if *n == it.name => row.update(n, "", row::SAME),
                None => {
                    changes += 1;
                    row.update(n, "", row::RENAME);
                }
            }
        }
        let total = self.items.len();
        self.summary.set_text(&if bad > 0 {
            format!("{bad} of {total} items have a problem; fix them to rename")
        } else if changes == 0 {
            "No names change yet".to_string()
        } else {
            format!("{changes} of {total} items will be renamed")
        });
        self.ok.set_sensitive(bad == 0 && changes > 0 && !self.broken.get());
    }

    fn submit(self: &Rc<Self>) {
        // Enter right after typing: don't act on a stale preview.
        if let Some(id) = self.pending.borrow_mut().take() {
            id.remove();
            self.recompute();
        }
        if !self.ok.is_sensitive() {
            return;
        }
        let report = match self.execute() {
            Ok(renames) => {
                (self.on_renamed)(&renames);
                self.win.close();
                return;
            }
            Err(report) => report,
        };
        self.error.set_text(&report);
        self.error.set_visible(true);
        if !self.broken.get() {
            // Everything was undone, but something on disk changed under
            // us: re-read the folders so the preview shows the clash.
            *self.existing.borrow_mut() = self.dirs.iter().map(names_in).collect();
        }
        self.recompute();
    }

    /// Renames everything, or undoes what was done and explains why not.
    fn execute(&self) -> Result<Vec<(String, String)>, String> {
        // Plan again from the current inputs: the single source of truth,
        // whatever state the preview is in.
        let existing = self.existing.borrow();
        let names = plan(&self.items, &self.mode(), &existing)
            .into_iter()
            .collect::<Result<Vec<String>, Problem>>()
            .map_err(|p| format!("Nothing was renamed: {}.", p.message().to_lowercase()))?;
        let steps = build_steps(&self.items, &names, &self.dirs, &existing);
        drop(existing);
        apply_steps(&self.files, &self.items, &steps).map_err(|f| {
            self.broken.set(!f.undone);
            f.report
        })
    }
}

// ======================= execution =======================

/// All rename steps for the batch. `names` must be a valid plan.
fn build_steps(items: &[Item], names: &[String], dirs: &[gio::File], existing: &[HashSet<String>]) -> Vec<Move> {
    let mut by_dir: Vec<Vec<usize>> = vec![Vec::new(); dirs.len()];
    for (i, it) in items.iter().enumerate() {
        if names[i] != it.name {
            by_dir[it.dir].push(i);
        }
    }
    // Deeper folders first, so renaming a folder never invalidates the path
    // of a selected item inside it (search results).
    let mut order: Vec<usize> = (0..dirs.len()).collect();
    order.sort_by_cached_key(|&d| std::cmp::Reverse(dirs[d].uri().len()));
    let mut steps = Vec::with_capacity(items.len());
    for d in order {
        let idx = &by_dir[d];
        let pairs: Vec<(&str, &str)> = idx.iter().map(|&i| (items[i].name.as_str(), names[i].as_str())).collect();
        let new: HashSet<&str> = pairs.iter().map(|p| p.1).collect();
        let taken = &existing[d];
        for mut m in schedule(&pairs, |t| taken.contains(t) || new.contains(t)) {
            m.index = idx[m.index];
            steps.push(m);
        }
    }
    steps
}

struct Failure {
    report: String,
    /// Whether every completed step was reverted.
    undone: bool,
}

/// Runs `steps` on `files` (one per item). On the first failure, reverts
/// the completed steps in reverse order. On success, returns (old URI, new URI) of each
/// renamed item; an item inside a renamed folder gets its new name in the old folder.
fn apply_steps(files: &[gio::File], items: &[Item], steps: &[Move]) -> Result<Vec<(String, String)>, Failure> {
    let mut cur = files.to_vec();
    // (item, file before the step, file after it)
    let mut done: Vec<(usize, gio::File, gio::File)> = Vec::with_capacity(steps.len());
    for m in steps {
        let before = cur[m.index].clone();
        // Refuses (G_IO_ERROR_EXISTS) rather than overwrite.
        match before.set_display_name(&m.to, None::<&gio::Cancellable>) {
            Ok(after) => {
                cur[m.index] = after.clone();
                done.push((m.index, before, after));
            }
            Err(e) => {
                let mut report = format!("Could not rename “{}” to “{}”: {}.", m.from, m.to, e.message());
                if done.is_empty() {
                    report.push_str(" Nothing was renamed.");
                    return Err(Failure { report, undone: true });
                }
                let failed = rollback(items, done);
                let undone = failed.is_empty();
                if undone {
                    report.push_str(" All changes were undone; nothing was renamed.");
                } else {
                    report.push_str(&format!(" Undoing failed for {} item(s): {}.", failed.len(), failed.join("; ")));
                }
                return Err(Failure { report, undone });
            }
        }
    }
    Ok(files
        .iter()
        .zip(&cur)
        .map(|(a, b)| (a.uri().to_string(), b.uri().to_string()))
        .filter(|(a, b)| a != b)
        .collect())
}

#[cfg(test)]
mod rename_pairs_tests {
    use super::*;

    #[test]
    fn success_reports_old_and_new_uris() {
        let d = std::env::temp_dir().join(format!("rfiles-br-pairs-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for n in ["a", "b", "c d"] {
            std::fs::write(d.join(n), n).unwrap();
        }
        let dir = gio::File::for_path(&d);
        let it: Vec<Item> =
            ["a", "b", "c d"].iter().map(|n| Item { name: n.to_string(), is_dir: false, dir: 0 }).collect();
        let names: Vec<String> = ["b", "a", "c d"].iter().map(|s| s.to_string()).collect();
        let existing = vec![names_in(&dir)];
        let files: Vec<gio::File> = it.iter().map(|i| dir.child(&i.name)).collect();
        let steps = build_steps(&it, &names, std::slice::from_ref(&dir), &existing);
        let pairs = apply_steps(&files, &it, &steps).unwrap_or_else(|f| panic!("{}", f.report));
        let uri = |n: &str| dir.child(n).uri().to_string();
        assert_eq!(pairs, vec![(uri("a"), uri("b")), (uri("b"), uri("a"))], "unchanged items are left out");
        std::fs::remove_dir_all(&d).unwrap();
    }
}

/// Reverts `done` in reverse order; returns a line per item it couldn't.
fn rollback(items: &[Item], done: Vec<(usize, gio::File, gio::File)>) -> Vec<String> {
    // A same-folder move without fallback is a plain rename, and without
    // OVERWRITE it refuses to replace anything. Moving back to the exact
    // original GFile also restores names that aren't valid UTF-8.
    let flags = gio::FileCopyFlags::NOFOLLOW_SYMLINKS | gio::FileCopyFlags::NO_FALLBACK_FOR_MOVE;
    let mut failed = Vec::new();
    for (i, before, after) in done.into_iter().rev() {
        if let Err(e) = after.move_(&before, flags, None::<&gio::Cancellable>, None) {
            let now = after.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
            failed.push(format!("“{}” is now “{}” ({})", items[i].name, now, e.message()));
        }
    }
    failed
}


// ======================= tests =======================

#[cfg(test)]
mod tests {
    use super::*;

    fn items(names: &[&str]) -> Vec<Item> {
        names.iter().map(|n| Item { name: n.to_string(), is_dir: false, dir: 0 }).collect()
    }

    fn dir_with(names: &[&str]) -> Vec<HashSet<String>> {
        vec![names.iter().map(|s| s.to_string()).collect()]
    }

    fn rep(find: &str, with: &str, cs: bool) -> Mode {
        Mode::Replace { find: find.into(), with: with.into(), case_sensitive: cs }
    }

    fn tpl(t: &str, start: u64) -> Mode {
        Mode::Template { template: t.into(), start }
    }

    fn ok(v: &[&str]) -> Vec<Result<String, Problem>> {
        v.iter().map(|s| Ok(s.to_string())).collect()
    }

    #[test]
    fn replace_case_sensitive_and_not() {
        let it = items(&["IMG_1.jpg", "img_2.jpg", "x.png"]);
        let ex = dir_with(&["IMG_1.jpg", "img_2.jpg", "x.png"]);
        assert_eq!(plan(&it, &rep("IMG", "Photo", true), &ex), ok(&["Photo_1.jpg", "img_2.jpg", "x.png"]));
        assert_eq!(plan(&it, &rep("img", "Photo", false), &ex), ok(&["Photo_1.jpg", "Photo_2.jpg", "x.png"]));
    }

    #[test]
    fn replace_all_occurrences_and_unicode() {
        let it = items(&["aXaXa", "ÄrgerÄ.txt", "İx"]);
        let ex = dir_with(&[]);
        assert_eq!(
            plan(&it, &rep("ä", "ae", false), &ex),
            ok(&["aXaXa", "aergerae.txt", "İx"])
        );
        assert_eq!(plan(&it, &rep("X", "-", true), &ex), ok(&["a-a-a", "ÄrgerÄ.txt", "İx"]));
        // "İ" lowercases to two chars; matching must respect char bounds.
        assert_eq!(plan(&it, &rep("i̇", "I", false), &ex), ok(&["aXaXa", "ÄrgerÄ.txt", "Ix"]));
        assert_eq!(plan(&it, &rep("i", "I", false), &ex), ok(&["aXaXa", "ÄrgerÄ.txt", "İx"]));
    }

    #[test]
    fn empty_find_or_template_changes_nothing() {
        let it = items(&["a", "b"]);
        assert_eq!(plan(&it, &rep("", "x", true), &dir_with(&["a", "b"])), ok(&["a", "b"]));
        assert_eq!(plan(&it, &tpl("", 1), &dir_with(&["a", "b"])), ok(&["a", "b"]));
    }

    #[test]
    fn template_numbering_padding_and_start() {
        let it = items(&["a.jpg", "b.PNG", "c"]);
        let ex = dir_with(&["a.jpg", "b.PNG", "c"]);
        assert_eq!(
            plan(&it, &tpl("Holiday ###", 1), &ex),
            ok(&["Holiday 001.jpg", "Holiday 002.PNG", "Holiday 003"])
        );
        assert_eq!(plan(&it, &tpl("#-##", 98), &ex), ok(&["98-98.jpg", "99-99.PNG", "100-100"]));
        // No # at all: the number is appended.
        assert_eq!(plan(&it, &tpl("Trip", 0), &ex), ok(&["Trip 0.jpg", "Trip 1.PNG", "Trip 2"]));
        assert_eq!(plan(&it, &tpl("Trip_ ", 0), &ex), ok(&["Trip_ 0.jpg", "Trip_ 1.PNG", "Trip_ 2"]));
    }

    #[test]
    fn template_extensions() {
        let mut it = items(&[".bashrc", "x.tar.gz", "photo.final.jpg", "folder.d"]);
        it[3].is_dir = true;
        assert_eq!(
            plan(&it, &tpl("f#", 1), &dir_with(&[])),
            ok(&["f1", "f2.tar.gz", "f3.jpg", "f4"])
        );
        assert_eq!(split_ext(".tar.gz"), (".tar", ".gz"));
        assert_eq!(split_ext("a."), ("a", "."));
    }

    #[test]
    fn invalid_names() {
        let it = items(&["abc", "xbx", "cab", "long"]);
        let ex = dir_with(&["abc", "xbx", "cab", "long"]);
        let r = plan(&it, &rep("b", "/", true), &ex);
        assert_eq!(r[0], Err(Problem::InvalidChar));
        assert_eq!(r[3], Ok("long".into()));
        assert_eq!(plan(&items(&["abc"]), &rep("abc", "", true), &ex), vec![Err(Problem::Empty)]);
        assert_eq!(plan(&items(&["abc"]), &rep("b", "\0", true), &ex), vec![Err(Problem::InvalidChar)]);
        assert_eq!(plan(&items(&["a.."]), &rep("a", "", true), &ex), vec![Err(Problem::Reserved)]);
        let long = "x".repeat(300);
        assert_eq!(plan(&items(&["long"]), &rep("long", &long, true), &ex), vec![Err(Problem::TooLong)]);
    }

    #[test]
    fn duplicates_within_batch() {
        let it = items(&["a1", "a2", "b"]);
        let ex = dir_with(&["a1", "a2", "b"]);
        let r = plan(&it, &rep("1", "2", true), &ex);
        // "a1" → "a2" while "a2" stays: both end up as "a2".
        assert_eq!(r, vec![Err(Problem::Duplicate), Err(Problem::Duplicate), Ok("b".into())]);
        let r = plan(&items(&["x1", "y1"]), &tpl("same", 1), &dir_with(&[]));
        assert_eq!(r, ok(&["same 1", "same 2"]));
        let r = plan(&items(&["x1", "y1"]), &rep("1", "", true), &dir_with(&[]));
        assert_eq!(r, ok(&["x", "y"]));
        let r = plan(&items(&["ax", "bx"]), &rep("x", "", true), &dir_with(&[]));
        assert_eq!(r, ok(&["a", "b"]));
        let r = plan(&items(&["a1", "a_1"]), &rep("_", "", true), &dir_with(&[]));
        assert_eq!(r, vec![Err(Problem::Duplicate), Err(Problem::Duplicate)]);
    }

    #[test]
    fn clash_with_files_outside_batch() {
        let it = items(&["a.txt", "b.txt"]);
        let ex = dir_with(&["a.txt", "b.txt", "a.md"]);
        let r = plan(&it, &rep(".txt", ".md", true), &ex);
        assert_eq!(r, vec![Err(Problem::Exists), Ok("b.md".into())]);
    }

    #[test]
    fn swaps_are_allowed_and_dirs_are_separate() {
        let it = items(&["a", "b"]);
        let ex = dir_with(&["a", "b"]);
        let r = plan(&it, &tpl("#", 0), &ex);
        assert_eq!(r, ok(&["0", "1"]));
        // Swap via template numbering of names "1","0".
        let it = items(&["1", "0"]);
        assert_eq!(plan(&it, &tpl("#", 0), &dir_with(&["0", "1"])), ok(&["0", "1"]));
        // Same new name in different folders is fine.
        let it = vec![
            Item { name: "x1".into(), is_dir: false, dir: 0 },
            Item { name: "x2".into(), is_dir: false, dir: 1 },
        ];
        let ex = vec![HashSet::from(["x1".to_string()]), HashSet::from(["x2".to_string(), "y".to_string()])];
        assert_eq!(
            plan(&it, &rep("x1", "y", true), &ex),
            ok(&["y", "x2"])
        );
        let r = plan(&it, &Mode::Replace { find: "x".into(), with: "".into(), case_sensitive: true }, &ex);
        assert_eq!(r, ok(&["1", "2"]));
    }

    /// Applies the steps to a fake folder, checking nothing is overwritten.
    fn simulate(pairs: &[(&str, &str)], others: &[&str]) -> Vec<Move> {
        let mut fs: HashMap<String, usize> = pairs.iter().enumerate().map(|(i, p)| (p.0.to_string(), i)).collect();
        for o in others {
            fs.insert(o.to_string(), usize::MAX);
        }
        let taken: HashSet<String> = fs.keys().cloned().chain(pairs.iter().map(|p| p.1.to_string())).collect();
        let steps = schedule(pairs, |t| taken.contains(t));
        for m in &steps {
            let id = fs.remove(&m.from).expect("source exists");
            assert_eq!(id, m.index);
            assert!(!fs.contains_key(&m.to), "step {m:?} would overwrite");
            fs.insert(m.to.clone(), id);
        }
        for (i, p) in pairs.iter().enumerate() {
            assert_eq!(fs.get(p.1), Some(&i), "{} ended in the wrong place", p.1);
        }
        assert_eq!(fs.len(), pairs.len() + others.len(), "no temporary files left");
        steps
    }

    #[test]
    fn schedule_simple_chain_and_cycles() {
        let s = simulate(&[("a", "x"), ("b", "y")], &[]);
        assert_eq!(s.len(), 2);
        // Chain: a→b needs b→c first.
        let s = simulate(&[("a", "b"), ("b", "c")], &[]);
        assert_eq!(s[0].from, "b");
        // Swap and a 3-cycle each need exactly one temporary step.
        assert_eq!(simulate(&[("a", "b"), ("b", "a")], &[]).len(), 3);
        assert_eq!(simulate(&[("a", "b"), ("b", "c"), ("c", "a")], &[]).len(), 4);
        // Mixed: chain, two cycles and a plain rename; temp name avoids
        // an existing file with the first candidate name.
        let s = simulate(
            &[("1", "2"), ("2", "3"), ("3", "1"), ("x", "y"), ("y", "x"), ("p", "q"), ("q", "r"), ("z", "zz")],
            &[".rfiles-rename-0.tmp"],
        );
        assert!(s.iter().all(|m| m.to != ".rfiles-rename-0.tmp"));
    }

    #[test]
    fn schedule_large_rotation() {
        // Renumbering 1..=1000 to 2..=1001 is one long chain, no temporaries.
        let names: Vec<(String, String)> = (1..=1000).map(|i| (i.to_string(), (i + 1).to_string())).collect();
        let pairs: Vec<(&str, &str)> = names.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        assert_eq!(simulate(&pairs, &[]).len(), 1000);
        // Reversing the order is 500 swaps.
        let names: Vec<(String, String)> = (0..1000).map(|i| (i.to_string(), (999 - i).to_string())).collect();
        let pairs: Vec<(&str, &str)> = names.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        assert_eq!(simulate(&pairs, &[]).len(), 1500);
    }

    /// A fresh folder under the system temp dir holding `names`, each file
    /// containing its own name.
    fn scratch(tag: &str, names: &[&str]) -> (std::path::PathBuf, gio::File) {
        let d = std::env::temp_dir().join(format!("rfiles-batch-rename-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for n in names {
            std::fs::write(d.join(n), n).unwrap();
        }
        let g = gio::File::for_path(&d);
        (d, g)
    }

    fn content(d: &std::path::Path, n: &str) -> String {
        std::fs::read_to_string(d.join(n)).unwrap_or_default()
    }

    #[test]
    fn executes_cycles_and_chains_on_disk() {
        let (d, dir) = scratch("ok", &["a", "b", "c", "p", "q", "other"]);
        let it = items(&["a", "b", "c", "p", "q"]);
        let names: Vec<String> = ["b", "c", "a", "q", "r"].iter().map(|s| s.to_string()).collect();
        let existing = vec![names_in(&dir)];
        assert!(problems(&it, &names, &existing).iter().all(Option::is_none));
        let files: Vec<gio::File> = it.iter().map(|i| dir.child(&i.name)).collect();
        let steps = build_steps(&it, &names, std::slice::from_ref(&dir), &existing);
        assert!(apply_steps(&files, &it, &steps).is_ok());
        assert_eq!(
            ["a", "b", "c", "q", "r", "other"].map(|n| content(&d, n)),
            ["c", "a", "b", "p", "q", "other"]
        );
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 6, "no temporary files left");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn failure_is_rolled_back() {
        let (d, dir) = scratch("fail", &["a", "b", "x"]);
        let it = items(&["a", "b", "x"]);
        // a↔b swap, then x→y, which fails: "y" appears after planning.
        let names: Vec<String> = ["b", "a", "y"].iter().map(|s| s.to_string()).collect();
        let existing = vec![names_in(&dir)];
        let files: Vec<gio::File> = it.iter().map(|i| dir.child(&i.name)).collect();
        let steps = build_steps(&it, &names, std::slice::from_ref(&dir), &existing);
        std::fs::write(d.join("y"), "intruder").unwrap();
        let f = apply_steps(&files, &it, &steps).unwrap_err();
        assert!(f.undone, "{}", f.report);
        assert!(f.report.contains("“x” to “y”"), "{}", f.report);
        assert_eq!(["a", "b", "x", "y"].map(|n| content(&d, n)), ["a", "b", "x", "intruder"]);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 4, "no temporary files left");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
