//! An RFiles window used as a Save / Open dialog (see `file_chooser.rs`):
//! the normal window plus a bar with the file name, Cancel and Save / Open.

use super::*;

pub enum PickKind {
    /// Choose existing files, or with `folders` a folder (the selected one, else the one shown)
    Open { multiple: bool, folders: bool },
    /// Type a name for a new file in the folder shown
    Save { name: String },
}

/// Gets the chosen files, or None when cancelled.
pub type PickDone = Box<dyn FnOnce(Option<Vec<gio::File>>)>;

pub struct Picker {
    kind: PickKind,
    name: Option<gtk::Entry>,
    done: RefCell<Option<PickDone>>,
}

pub fn build_picker(gapp: &gtk::Application, start: gio::File, kind: PickKind, title: &str, accept: &str, done: PickDone) -> Rc<App> {
    let app = build(gapp, start);
    app.window.set_title(Some(title));
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.add_css_class("picker-bar");
    bar.set_margin_top(8);
    bar.set_margin_bottom(8);
    bar.set_margin_start(12);
    bar.set_margin_end(12);
    let name = match &kind {
        PickKind::Save { name } => {
            let label = gtk::Label::new(Some("Name"));
            let entry = gtk::Entry::builder().text(name).hexpand(true).activates_default(true).build();
            label.set_mnemonic_widget(Some(&entry));
            bar.append(&label);
            bar.append(&entry);
            Some(entry)
        }
        PickKind::Open { .. } => {
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            bar.append(&spacer);
            None
        }
    };
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_mnemonic(accept);
    ok.add_css_class("suggested-action");
    bar.append(&cancel);
    bar.append(&ok);
    if let Some(main) = app.window.child().and_downcast::<gtk::Box>() {
        main.append(&bar);
    }
    app.window.set_default_widget(Some(&ok));
    *app.picker.borrow_mut() = Some(Rc::new(Picker { kind, name, done: RefCell::new(Some(done)) }));

    let a = Rc::downgrade(&app);
    cancel.connect_clicked(move |_| {
        if let Some(a) = a.upgrade() {
            a.pick_finish(None);
        }
    });
    let a = Rc::downgrade(&app);
    ok.connect_clicked(move |_| {
        if let Some(a) = a.upgrade() {
            a.pick_accept();
        }
    });
    if let Some(e) = app.picker.borrow().as_ref().and_then(|p| p.name.clone()) {
        // Ready to type over the name, keeping the extension
        e.grab_focus();
        let stem = e.text().rfind('.').filter(|&i| i > 0).map(|i| e.text()[..i].chars().count() as i32);
        e.select_region(0, stem.unwrap_or(-1));
    }
    app
}

impl App {
    fn picker(&self) -> Option<Rc<Picker>> {
        self.picker.borrow().clone()
    }

    pub(super) fn is_picker(&self) -> bool {
        self.picker.borrow().is_some()
    }

    /// Double-click / Enter on an item while picking. Returns false when it isn't a picker window.
    pub(super) fn pick_activate(self: &Rc<Self>, info: &gio::FileInfo) -> bool {
        let Some(p) = self.picker() else { return false };
        if is_dir_like(info) {
            self.navigate(Loc::Dir(item_file(info)));
            return true;
        }
        match (&p.kind, &p.name) {
            (PickKind::Open { folders: true, .. }, _) => {}
            (PickKind::Save { .. }, Some(e)) => {
                e.set_text(&info.display_name());
                self.pick_accept();
            }
            _ => self.pick_accept(),
        }
        true
    }

    /// The Save / Open button.
    pub(super) fn pick_accept(self: &Rc<Self>) {
        let Some(p) = self.picker() else { return };
        let sel = self.selected_infos();
        match &p.kind {
            PickKind::Open { folders: true, .. } => {
                let dir = match sel.as_slice() {
                    [one] if is_dir_like(one) => Some(item_file(one)),
                    _ => self.current_dir(),
                };
                match dir {
                    Some(d) => self.pick_finish(Some(vec![d])),
                    None => self.status.set_text("Open a folder to choose it"),
                }
            }
            PickKind::Open { multiple, .. } => {
                if let [one] = sel.as_slice() {
                    if is_dir_like(one) {
                        self.navigate(Loc::Dir(item_file(one)));
                        return;
                    }
                }
                let mut files: Vec<gio::File> = sel.iter().filter(|i| !is_dir_like(i)).map(item_file).collect();
                if !multiple {
                    files.truncate(1);
                }
                if files.is_empty() {
                    self.status.set_text("Choose a file first");
                } else {
                    self.pick_finish(Some(files));
                }
            }
            PickKind::Save { .. } => {
                let Some(entry) = p.name.as_ref() else { return };
                let name = entry.text().trim().to_string();
                if let Err(why) = check_save_name(&name) {
                    self.status.set_text(why);
                    return;
                }
                let Some(dir) = self.current_dir() else {
                    self.status.set_text("Open a folder to save in");
                    return;
                };
                let target = dir.child(&name);
                match target.query_file_type(gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>) {
                    gio::FileType::Unknown => self.pick_finish(Some(vec![target])),
                    gio::FileType::Directory => {
                        entry.set_text("");
                        self.navigate(Loc::Dir(target));
                    }
                    _ => self.confirm_replace(target, &name),
                }
            }
        }
    }

    fn confirm_replace(self: &Rc<Self>, target: gio::File, name: &str) {
        let d = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Replace “{name}”?"))
            .detail("A file with this name already exists here. Saving replaces it.")
            .buttons(["Cancel", "Replace"])
            .cancel_button(0)
            .default_button(0)
            .build();
        let a = Rc::downgrade(self);
        d.choose(Some(&self.window), None::<&gio::Cancellable>, move |r| {
            if let (Ok(1), Some(a)) = (r, a.upgrade()) {
                a.pick_finish(Some(vec![target]));
            }
        });
    }

    /// Answers the request (once) and closes the window.
    pub(crate) fn pick_finish(&self, result: Option<Vec<gio::File>>) {
        if self.pick_answer(result) {
            self.window.close();
        }
    }

    /// Answers the request if it is still open; true if it was. The window closing unanswered counts as Cancel.
    pub(super) fn pick_answer(&self, result: Option<Vec<gio::File>>) -> bool {
        let done = self.picker().and_then(|p| p.done.borrow_mut().take());
        done.map(|d| d(result)).is_some()
    }
}

/// Why `name` can't be saved as, if it can't.
fn check_save_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        Err("Type a name for the file")
    } else if name.contains('/') {
        Err("A file name can't contain “/”")
    } else if name == "." || name == ".." {
        Err("That name can't be used")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::check_save_name;

    #[test]
    fn save_names() {
        assert!(check_save_name("report.pdf").is_ok());
        assert!(check_save_name(".hidden").is_ok());
        assert!(check_save_name("").is_err());
        assert!(check_save_name("a/b").is_err());
        assert!(check_save_name("..").is_err());
    }
}
