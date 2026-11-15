//! The result banner: one row above the status bar that shows how the last operation (copy, move, trash,
//! delete, restore, compress, extract, eject) ended, until it is dismissed or a newer result replaces it.
//! Successes go away by themselves after a few seconds; results with errors stay, with "Details…" listing
//! every error line. The status bar below keeps item counts and short hints.

use crate::i18n::tr;
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// How long a result without errors stays up.
const SUCCESS_TIMEOUT: Duration = Duration::from_secs(6);

/// The result of an operation: one line for the banner, and every error it ran into.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub text: String,
    pub errors: Vec<String>,
}

impl Notice {
    pub fn ok(text: impl Into<String>) -> Self {
        Notice { text: text.into(), errors: Vec::new() }
    }

    /// A job whose worker stopped before it could report anything.
    pub fn stopped(text: &str) -> Self {
        Notice { text: text.into(), errors: vec![format!("{text}: the job stopped unexpectedly")] }
    }

    pub fn failed(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// The banner line for a job that did `done` items and ran into `errors`: `done_text` when all went well,
/// `none_text(failed)` when nothing did, otherwise "<done_text>, N failed". A single error is quoted in full;
/// with more, "Details…" lists them.
pub fn outcome_text(
    done_text: &str,
    none_text: impl FnOnce(usize) -> String,
    done: usize,
    errors: &[String],
) -> String {
    let s = match (done, errors.len()) {
        (_, 0) => return done_text.to_string(),
        (0, n) => none_text(n),
        (_, n) => format!("{done_text}, {}", tr("{n} failed").replace("{n}", &n.to_string())),
    };
    match errors {
        [one] => format!("{s} — {one}"),
        _ => s,
    }
}

#[cfg(test)]
mod outcome_text_tests {
    use super::*;

    fn errs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("f{i}: Permission denied")).collect()
    }

    #[test]
    fn counts_done_and_failed() {
        let none = |n| format!("Couldn't copy {n} item(s)");
        assert_eq!(outcome_text("Copied 3 items", none, 3, &[]), "Copied 3 items");
        assert_eq!(outcome_text("Copied 0 items", none, 0, &[]), "Copied 0 items");
        let one = "Copied 2 items, 1 failed — f0: Permission denied";
        assert_eq!(outcome_text("Copied 2 items", none, 2, &errs(1)), one);
        assert_eq!(outcome_text("Copied 2 items", none, 2, &errs(5)), "Copied 2 items, 5 failed");
        let none_done = "Couldn't copy 1 item(s) — f0: Permission denied";
        assert_eq!(outcome_text("Copied 0 items", none, 0, &errs(1)), none_done);
        assert_eq!(outcome_text("Copied 0 items", none, 0, &errs(4)), "Couldn't copy 4 item(s)");
    }
}

/// What to do with an incoming notice while `current` is showing.
#[derive(Debug, PartialEq)]
pub enum Placement {
    /// Show it in the banner, replacing whatever is there
    Banner,
    /// Keep the error on show and put the incoming success in the status bar, so an error is never
    /// replaced by a success before it was read
    StatusBar,
}

pub fn place(current: Option<&Notice>, incoming: &Notice) -> Placement {
    match current {
        Some(c) if c.failed() && !incoming.failed() => Placement::StatusBar,
        _ => Placement::Banner,
    }
}

#[cfg(test)]
mod place_tests {
    use super::*;

    #[test]
    fn errors_are_not_replaced_by_successes() {
        let ok = Notice::ok("Copied 2 items");
        let bad = Notice { text: "Couldn't delete 1 item".into(), errors: vec!["a: Busy".into()] };
        assert_eq!(place(None, &ok), Placement::Banner);
        assert_eq!(place(None, &bad), Placement::Banner);
        assert_eq!(place(Some(&ok), &ok), Placement::Banner);
        assert_eq!(place(Some(&ok), &bad), Placement::Banner);
        assert_eq!(place(Some(&bad), &bad), Placement::Banner);
        assert_eq!(place(Some(&bad), &ok), Placement::StatusBar);
    }
}

/// The text of the details window and its Copy button: the summary line, then every error on its own line.
pub fn details_text(n: &Notice) -> String {
    let mut s = n.text.clone();
    s.push_str("\n\n");
    s.push_str(&n.errors.join("\n"));
    s
}

#[cfg(test)]
mod details_text_tests {
    use super::*;

    #[test]
    fn lists_every_error() {
        let errors: Vec<String> = (0..40).map(|i| format!("file {i}: No space left on device")).collect();
        let n = Notice { text: "Copied 2 items, 40 failed".into(), errors };
        let d = details_text(&n);
        let lines: Vec<&str> = d.lines().collect();
        assert_eq!(lines.len(), 42);
        assert_eq!(lines[0], "Copied 2 items, 40 failed");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "file 0: No space left on device");
        assert_eq!(lines[41], "file 39: No space left on device");
    }
}

/// The banner row. Its buttons hold it weakly; the window owns it through `App`.
pub struct Banner {
    pub row: gtk::Box,
    label: gtk::Label,
    details: gtk::Button,
    current: RefCell<Option<Notice>>,
    /// Bumped on every change, so a pending auto-dismiss of an older notice does nothing
    generation: Cell<u64>,
}

impl Banner {
    pub fn new() -> Rc<Self> {
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(pango::EllipsizeMode::End);
        let details = gtk::Button::with_label(&tr("Details…"));
        details.add_css_class("flat");
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.set_tooltip_text(Some(&tr("Dismiss")));
        close.update_property(&[gtk::accessible::Property::Label(&tr("Dismiss"))]);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("banner");
        row.append(&label);
        row.append(&details);
        row.append(&close);
        row.set_visible(false);
        let b = Rc::new(Banner { row, label, details, current: RefCell::new(None), generation: Cell::new(0) });

        let w = Rc::downgrade(&b);
        b.details.connect_clicked(move |btn| {
            let Some(b) = w.upgrade() else { return };
            let parent = btn.root().and_downcast::<gtk::Window>();
            let n = b.current.borrow().clone();
            if let Some(n) = n {
                show_details(parent.as_ref(), &n);
            }
        });
        let w = Rc::downgrade(&b);
        close.connect_clicked(move |_| {
            if let Some(b) = w.upgrade() {
                b.dismiss();
            }
        });
        b
    }

    /// Shows `n`, unless an error is on show and `n` is a success: then `n` is handed back for the status bar.
    pub fn show(self: &Rc<Self>, n: Notice) -> Option<Notice> {
        if place(self.current.borrow().as_ref(), &n) == Placement::StatusBar {
            return Some(n);
        }
        let gen = self.generation.get() + 1;
        self.generation.set(gen);
        self.label.set_text(&n.text);
        self.label.set_tooltip_text(Some(&n.text));
        self.details.set_visible(n.failed());
        if n.failed() {
            self.row.add_css_class("banner-error");
        } else {
            self.row.remove_css_class("banner-error");
            let w: Weak<Banner> = Rc::downgrade(self);
            glib::timeout_add_local_once(SUCCESS_TIMEOUT, move || {
                if let Some(b) = w.upgrade() {
                    if b.generation.get() == gen {
                        b.dismiss();
                    }
                }
            });
        }
        *self.current.borrow_mut() = Some(n);
        self.row.set_visible(true);
        None
    }

    /// Hides the banner; returns whether it was showing.
    pub fn dismiss(&self) -> bool {
        if self.current.borrow_mut().take().is_none() {
            return false;
        }
        self.generation.set(self.generation.get() + 1);
        // Keep keyboard focus in the window when it was on one of our buttons
        let had_focus = self.row.focus_child().is_some();
        self.row.set_visible(false);
        if had_focus {
            if let Some(root) = self.row.root() {
                root.child_focus(gtk::DirectionType::TabForward);
            }
        }
        true
    }
}

/// A small window listing every error of `n`, as selectable text with a Copy button.
fn show_details(parent: Option<&gtk::Window>, n: &Notice) {
    let text = details_text(n);
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    view.add_css_class("details-text");
    view.buffer().set_text(&text);
    let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();
    let copy = gtk::Button::with_label(&tr("Copy"));
    let close = gtk::Button::with_label(&tr("Close"));
    let btns = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btns.set_halign(gtk::Align::End);
    btns.append(&copy);
    btns.append(&close);
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 12);
    bx.set_margin_top(12);
    bx.set_margin_bottom(12);
    bx.set_margin_start(12);
    bx.set_margin_end(12);
    bx.append(&scroll);
    bx.append(&btns);
    let win = gtk::Window::builder()
        .title(tr("Details"))
        .default_width(560)
        .default_height(360)
        .destroy_with_parent(true)
        .child(&bx)
        .build();
    win.set_transient_for(parent);
    win.add_css_class("rfiles");

    copy.connect_clicked(move |b| b.clipboard().set_text(&text));
    let w = win.downgrade();
    close.connect_clicked(move |_| {
        if let Some(w) = w.upgrade() {
            w.close();
        }
    });
    let keys = gtk::EventControllerKey::new();
    let w = win.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key != gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        if let Some(w) = w.upgrade() {
            w.close();
        }
        glib::Propagation::Stop
    });
    win.add_controller(keys);
    win.present();
    close.grab_focus();
}
