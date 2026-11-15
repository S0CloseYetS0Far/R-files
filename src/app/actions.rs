use super::*;

impl App {
    // ================= actions & signals =================
    pub(super) fn setup_actions(self: &Rc<Self>, gapp: &gtk::Application) {
        type Handler = Box<dyn Fn(&Rc<App>)>;
        let add = |name: &str, f: Handler| {
            let act = gio::SimpleAction::new(name, None);
            let a = Rc::downgrade(self);
            act.connect_activate(move |_, _| {
                if let Some(a) = a.upgrade() {
                    f(&a)
                }
            });
            self.window.add_action(&act);
        };
        add("new-tab", Box::new(|a| a.new_tab(Loc::Dir(home()))));
        add("close-tab", Box::new(|a| a.close_tab_or_window()));
        add("reopen-tab", Box::new(|a| a.reopen_tab()));
        add("next-tab", Box::new(|a| a.cycle_tab(1)));
        add("prev-tab", Box::new(|a| a.cycle_tab(-1)));
        add("back", Box::new(|a| a.go_back()));
        add("forward", Box::new(|a| a.go_forward()));
        add("up", Box::new(|a| a.go_up()));
        add("refresh", Box::new(|a| a.reload()));
        add("focus-search", Box::new(|a| {
            a.search.grab_focus();
        }));
        add("focus-path", Box::new(|a| {
            a.path_entry.grab_focus();
            a.path_entry.select_region(0, -1);
        }));
        add("toggle-hidden", Box::new(|a| a.hidden_btn.set_active(!a.hidden_btn.is_active())));
        add("grid-view", Box::new(|a| a.view_btn.set_active(false)));
        add("list-view", Box::new(|a| a.view_btn.set_active(true)));
        add("toggle-sidebar", Box::new(|a| a.sidebar_btn.set_active(!a.sidebar_btn.is_active())));
        add("toggle-preview", Box::new(|a| a.preview_btn.set_active(!a.preview_btn.is_active())));
        add("select-all", Box::new(|a| {
            a.selection.select_all();
        }));
        add("select-none", Box::new(|a| {
            a.selection.unselect_all();
        }));
        add("copy", Box::new(|a| a.copy_selected(false)));
        add("cut", Box::new(|a| {
            if !a.in_trash() {
                a.copy_selected(true)
            }
        }));
        add("paste", Box::new(|a| {
            if let Some(d) = a.current_dir().filter(|d| !is_trash(d)) {
                a.paste_into(d);
            }
        }));
        add("paste-into", Box::new(|a| {
            if let Some(i) = a.selected_info().filter(is_dir_like) {
                a.paste_into(item_file(&i));
            }
        }));
        add("terminal-here", Box::new(|a| a.open_terminal(a.current_dir())));
        add("terminal-selected", Box::new(|a| {
            let dir = a.selected_info().filter(is_dir_like).map(|i| item_file(&i));
            a.open_terminal(dir.or_else(|| a.current_dir()));
        }));
        add("pin-current", Box::new(|a| {
            if let Some(f) = a.current_dir() {
                let pinned = a.is_pinned(&f);
                a.set_pinned(f.uri().to_string(), !pinned);
                a.status.set_text(&if pinned {
                    format!("Unpinned “{}”", file_title(&f))
                } else {
                    format!("Pinned “{}” to the sidebar", file_title(&f))
                });
            }
        }));
        add("open", Box::new(|a| match a.selected_infos().len() {
            0 => {}
            1 => {
                if let Some(i) = a.selected_info() {
                    a.open_info(&i);
                }
            }
            _ => a.open_selected(),
        }));
        let gw = gapp.downgrade();
        add("new-window", Box::new(move |a| {
            if let Some(g) = gw.upgrade() {
                build(&g, a.current_dir().unwrap_or_else(home));
            }
        }));
        let gw = gapp.downgrade();
        add("quit", Box::new(move |a| {
            if let Some(g) = gw.upgrade() {
                crate::jobs::quit(&g, &a.window);
            }
        }));
        add("shortcuts", Box::new(|a| show_shortcuts(&a.window)));
        add("about", Box::new(|a| show_about(&a.window)));
        add("open-tab", Box::new(|a| {
            if let Some(i) = a.selected_info() {
                a.new_tab(Loc::Dir(item_file(&i)));
            }
        }));
        add("open-with", Box::new(|a| {
            if let Some(i) = a.selected_info() {
                a.launch(&item_file(&i), true);
            }
        }));
        add("star", Box::new(|a| a.toggle_star()));
        add("pin-selected", Box::new(|a| {
            if let Some(i) = a.selected_info() {
                let uri = item_file(&i).uri().to_string();
                let pinned = a.config.borrow().pins.contains(&uri);
                a.set_pinned(uri, !pinned);
            }
        }));
        add("change-icon", Box::new(|a| {
            let Some(info) = a.selected_info() else { return };
            let target = item_file(&info);
            let a2 = a.clone();
            a.pick_image(&format!("Choose an icon for “{}”", info.display_name()), move |r| {
                if let Some(img) = r {
                    a2.set_custom_icon(&target, &img.uri());
                }
            });
        }));
        add("reset-icon", Box::new(|a| {
            if let Some(info) = a.selected_info() {
                a.set_custom_icon(&item_file(&info), "");
            }
        }));
        add("copy-path", Box::new(|a| {
            let paths: Vec<String> = a.selected_infos().iter().map(|i| item_file(i).parse_name().to_string()).collect();
            if !paths.is_empty() {
                a.window.clipboard().set_text(&paths.join("\n"));
            }
        }));
        add("compress", Box::new(|a| {
            let infos = a.selected_infos();
            let paths: Vec<PathBuf> = infos.iter().filter_map(|i| item_file(i).path()).collect();
            if a.in_trash() || paths.is_empty() {
                return;
            }
            if paths.len() != infos.len() {
                a.status.set_text("Can only compress local files");
                return;
            }
            // Starred / search results have no folder of their own: use the first item's
            let dir = a.current_dir().and_then(|d| d.path()).or_else(|| paths[0].parent().map(Path::to_path_buf));
            if let Some(dir) = dir {
                crate::archive::compress_dialog(&a.window, &a.status, a.notifier(), dir, paths);
            }
        }));
        add("extract-here", Box::new(|a| {
            let infos = a.selected_infos();
            let paths: Vec<PathBuf> = infos.iter().filter_map(|i| item_file(i).path()).collect();
            if a.in_trash() || paths.is_empty() {
                return;
            }
            if paths.len() != infos.len() {
                a.status.set_text("Can only extract local files");
                return;
            }
            crate::archive::extract_here(&a.status, a.notifier(), paths);
        }));
        add("extract-to", Box::new(|a| {
            let infos = a.selected_infos();
            let paths: Vec<PathBuf> = infos.iter().filter_map(|i| item_file(i).path()).collect();
            if a.in_trash() || paths.is_empty() {
                return;
            }
            if paths.len() != infos.len() {
                a.status.set_text("Can only extract local files");
                return;
            }
            let dlg = gtk::FileDialog::builder().title("Extract to").modal(true).accept_label("Extract").build();
            if let Some(dir) = a.current_dir() {
                dlg.set_initial_folder(Some(&dir));
            }
            let (status, notify) = (a.status.downgrade(), a.notifier());
            dlg.select_folder(Some(&a.window), None::<&gio::Cancellable>, move |r| {
                // Cancelling is an error from the dialog: nothing to do
                let (Ok(dir), Some(status)) = (r, status.upgrade()) else { return };
                match dir.path() {
                    Some(dest) => crate::archive::extract_into(&status, notify, paths, Some(dest)),
                    None => status.set_text("Can only extract into a local folder"),
                }
            });
        }));
        add("trash", Box::new(|a| a.delete_selected(false)));
        add("delete-permanently", Box::new(|a| a.delete_selected(true)));
        add("forget-recent", Box::new(|a| a.forget_recent_selected()));
        add("empty-trash", Box::new(crate::trash::empty_trash));
        add("restore", Box::new(|a| a.restore_selected()));
        add("rename", Box::new(|a| {
            if a.in_trash() {
                return;
            }
            let mut infos = a.selected_infos();
            let w = Rc::downgrade(a);
            if infos.len() > 1 {
                crate::batch_rename::open(&a.window, &infos, move |renames| {
                    crate::undo::record_renames(renames);
                    if let Some(a) = w.upgrade() {
                        a.note_renames(renames);
                    }
                });
                return;
            }
            let Some(info) = infos.pop() else { return };
            let file = item_file(&info);
            let name = info.display_name().to_string();
            let sel = crate::util::select_len(&name, is_dir_like(&info));
            a.prompt("Rename", &name, sel, "Rename", move |new| {
                let renamed = file
                    .set_display_name(&new, None::<&gio::Cancellable>)
                    .map_err(|e| e.message().to_string())?;
                let pair = [(file.uri().to_string(), renamed.uri().to_string())];
                crate::undo::record_renames(&pair);
                if let Some(a) = w.upgrade() {
                    a.note_renames(&pair);
                }
                Ok(())
            });
        }));
        // Properties of the selection, or of the current folder when
        // nothing is selected (the background menu clears the selection).
        add("properties", Box::new(|a| {
            let mut files: Vec<gio::File> = a.selected_infos().iter().map(item_file).collect();
            if files.is_empty() {
                files.extend(a.current_dir());
            }
            crate::properties::show(&a.window, files);
        }));
        add("new-folder", Box::new(|a| {
            let Some(dir) = a.current_dir().filter(|d| !is_trash(d)) else { return };
            a.prompt("New Folder", "New Folder", "New Folder".len(), "Create", move |name| {
                if let Some(p) = folder_name_problem(&name) {
                    return Err(p.to_string());
                }
                let new = dir.child(&name);
                new.make_directory(None::<&gio::Cancellable>).map_err(|e| e.message().to_string())?;
                if let Some(p) = new.path() {
                    crate::undo::record(crate::undo::Op::NewFolder(p));
                }
                Ok(())
            });
        }));
        add("undo", Box::new(|a| crate::undo::run(a, false)));
        add("redo", Box::new(|a| crate::undo::run(a, true)));

        let unpin = gio::SimpleAction::new("unpin", Some(glib::VariantTy::STRING));
        let a = Rc::downgrade(self);
        unpin.connect_activate(move |_, v| {
            let Some(a) = a.upgrade() else { return };
            if let Some(uri) = v.and_then(|v| v.get::<String>()) {
                a.set_pinned(uri, false);
            }
        });
        self.window.add_action(&unpin);
        for (name, up) in [("side-up", true), ("side-down", false)] {
            let act = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let a = Rc::downgrade(self);
            act.connect_activate(move |_, v| {
                let Some(a) = a.upgrade() else { return };
                if let Some(k) = v.and_then(|v| v.get::<String>()) {
                    a.move_sidebar_item(&k, up);
                }
            });
            self.window.add_action(&act);
        }
        let stab = gio::SimpleAction::new("sidebar-tab", Some(glib::VariantTy::INT32));
        let a = Rc::downgrade(self);
        stab.connect_activate(move |_, v| {
            let Some(a) = a.upgrade() else { return };
            if let Some(i) = v.and_then(|v| v.get::<i32>()) {
                a.open_sidebar_in_tab(i as usize);
            }
        });
        self.window.add_action(&stab);
        let sort_by = gio::SimpleAction::new("sort-by", Some(glib::VariantTy::STRING));
        let a = Rc::downgrade(self);
        sort_by.connect_activate(move |_, v| {
            let Some(a) = a.upgrade() else { return };
            if let Some(k) = v.and_then(|v| v.get::<String>()).and_then(|k| SortKey::parse(&k)) {
                a.set_sort(k, a.sort.get().1);
            }
        });
        self.window.add_action(&sort_by);
        add("sort-reversed", Box::new(|a| {
            let (key, desc) = a.sort.get();
            a.set_sort(key, !desc);
        }));

        let accels: &[(&str, &[&str])] = &[
            ("win.new-tab", &["<Control>t"]),
            ("win.new-window", &["<Control>n"]),
            ("win.quit", &["<Control>q"]),
            ("win.shortcuts", &["<Control>question", "<Control><Shift>slash", "F1"]),
            ("win.close-tab", &["<Control>w"]),
            ("win.reopen-tab", &["<Control><Shift>t"]),
            ("win.next-tab", &["<Control>Page_Down", "<Control>Tab"]),
            ("win.prev-tab", &["<Control>Page_Up", "<Control><Shift>Tab"]),
            ("win.back", &["<Alt>Left"]),
            ("win.forward", &["<Alt>Right"]),
            ("win.up", &["<Alt>Up"]),
            ("win.refresh", &["F5", "<Control>r"]),
            ("win.focus-search", &["<Control>f"]),
            ("win.focus-path", &["<Control>l"]),
            ("win.toggle-hidden", &["<Control>h"]),
            ("win.pin-current", &["<Control>d"]),
            ("win.new-folder", &["<Control><Shift>n"]),
            ("win.terminal-here", &["<Control><Alt>t"]),
            ("win.grid-view", &["<Control>1"]),
            ("win.list-view", &["<Control>2"]),
            ("win.toggle-sidebar", &["F9"]),
            ("win.toggle-preview", &["F4"]),
            ("win.properties", &["<Alt>Return", "<Control>i"]),
        ];
        for (action, keys) in accels {
            gapp.set_accels_for_action(action, keys);
        }
    }
}

/// Every shortcut, shown in the shortcuts window. Keep in step with the README table.
const SHORTCUTS: &[(&str, &str)] = &[
    ("New tab / close tab", "Ctrl+T / Ctrl+W (the last tab closes the window)"),
    ("Reopen closed tab", "Ctrl+Shift+T"),
    ("Reorder tabs / drop files on a tab", "Drag a tab / drag files onto it (hover to switch)"),
    ("New window", "Ctrl+N"),
    ("Quit", "Ctrl+Q"),
    ("Switch tabs", "Ctrl+PageDown / Ctrl+PageUp"),
    ("Tab keys (tab focused)", "Left / Right / Home / End move, Enter or Space opens, Delete closes"),
    ("Preview / open", "Click / double-click or Enter"),
    ("Copy / cut / paste", "Ctrl+C / Ctrl+X / Ctrl+V"),
    ("Undo / redo a file operation", "Ctrl+Z / Ctrl+Shift+Z"),
    ("Select several", "Ctrl+click, Shift+click, drag a box, Ctrl+A"),
    ("Rename / move to trash", "F2 (several selected = batch rename) / Delete"),
    ("Delete permanently", "Shift+Delete (asks first)"),
    ("Properties", "Alt+Enter or Ctrl+I"),
    ("Drag to copy / move", "Hold Ctrl to copy, Shift to move"),
    ("New folder", "Ctrl+Shift+N"),
    ("Pin / unpin this folder", "Ctrl+D"),
    ("Open in terminal", "Ctrl+Alt+T"),
    ("Search / path bar", "Ctrl+F / Ctrl+L (Tab completes folders, Esc cancels)"),
    ("Back / forward / up", "Alt+Left / Alt+Right / Alt+Up"),
    ("Hidden files", "Ctrl+H"),
    ("Grid / list view", "Ctrl+1 / Ctrl+2"),
    ("Right-click menu from the keyboard", "Menu key or Shift+F10"),
    ("Show / hide sidebar / preview pane", "F9 / F4"),
    ("This list", "Ctrl+? or F1"),
];

fn show_shortcuts(parent: &gtk::ApplicationWindow) {
    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(24)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(20)
        .margin_end(20)
        .build();
    for (row, (what, keys)) in SHORTCUTS.iter().enumerate() {
        let l = gtk::Label::new(Some(what));
        l.set_xalign(0.0);
        let k = gtk::Label::new(Some(keys));
        k.set_xalign(0.0);
        k.add_css_class("dim");
        grid.attach(&l, 0, row as i32, 1, 1);
        grid.attach(&k, 1, row as i32, 1, 1);
    }
    let scroll = gtk::ScrolledWindow::builder().child(&grid).build();
    let win = gtk::Window::builder()
        .title("Keyboard shortcuts")
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .default_width(560)
        .default_height(520)
        .child(&scroll)
        .build();
    // Esc closes it
    let k = gtk::EventControllerKey::new();
    let ww = win.downgrade();
    k.connect_key_pressed(move |_, key, _, _| {
        if key != gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        if let Some(w) = ww.upgrade() {
            w.close();
        }
        glib::Propagation::Stop
    });
    win.add_controller(k);
    win.present();
}

fn show_about(parent: &gtk::ApplicationWindow) {
    gtk::AboutDialog::builder()
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .program_name("RFiles")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("A black, tabbed file explorer with live preview.")
        .website("https://github.com/S0CloseYetS0Far/R-files")
        .build()
        .present();
}
