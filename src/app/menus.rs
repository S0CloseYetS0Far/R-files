use super::*;

/// One entry of a right-click menu; `None` in a menu list is a separator.
pub(super) struct MItem {
    pub(super) label: String,
    pub(super) action: &'static str,
    pub(super) target: Option<glib::Variant>,
    pub(super) accel: &'static str,
}

fn mi(label: impl Into<String>, action: &'static str, accel: &'static str) -> Option<MItem> {
    Some(MItem { label: label.into(), action, target: None, accel })
}

/// Menu key or Shift+F10: the keyboard's way to ask for a right-click menu.
pub(super) fn is_menu_key(key: gdk::Key, mods: gdk::ModifierType) -> bool {
    let other = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK;
    match key {
        gdk::Key::Menu => !mods.intersects(other),
        gdk::Key::F10 => mods.contains(gdk::ModifierType::SHIFT_MASK) && !mods.intersects(other),
        _ => false,
    }
}

/// A focus move inside a popup menu.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MenuNav {
    Up,
    Down,
    Home,
    End,
}

/// Where focus goes in a popup menu. `usable[i]` is false for separators and disabled
/// entries; Up and Down wrap at the ends. None when nothing can take focus.
fn menu_nav_target(usable: &[bool], current: Option<usize>, nav: MenuNav) -> Option<usize> {
    let n = usable.len();
    let first = usable.iter().position(|&u| u)?;
    let last = usable.iter().rposition(|&u| u)?;
    let cur = current.filter(|&c| c < n);
    match (nav, cur) {
        (MenuNav::Home, _) | (MenuNav::Down, None) => Some(first),
        (MenuNav::End, _) | (MenuNav::Up, None) => Some(last),
        (MenuNav::Down, Some(c)) => (1..=n).map(|d| (c + d) % n).find(|&i| usable[i]),
        (MenuNav::Up, Some(c)) => (1..=n).map(|d| (c + n - d) % n).find(|&i| usable[i]),
    }
}

#[cfg(test)]
mod menu_nav_tests {
    use super::{is_menu_key, menu_nav_target, MenuNav::*};
    use gtk::gdk;

    // entry, separator, entry, disabled entry, entry
    const M: [bool; 5] = [true, false, true, false, true];

    #[test]
    fn down_skips_separators_and_wraps() {
        assert_eq!(menu_nav_target(&M, Some(0), Down), Some(2));
        assert_eq!(menu_nav_target(&M, Some(2), Down), Some(4));
        assert_eq!(menu_nav_target(&M, Some(4), Down), Some(0));
        assert_eq!(menu_nav_target(&M, None, Down), Some(0));
    }

    #[test]
    fn up_skips_separators_and_wraps() {
        assert_eq!(menu_nav_target(&M, Some(4), Up), Some(2));
        assert_eq!(menu_nav_target(&M, Some(2), Up), Some(0));
        assert_eq!(menu_nav_target(&M, Some(0), Up), Some(4));
        assert_eq!(menu_nav_target(&M, None, Up), Some(4));
    }

    #[test]
    fn home_and_end_jump_to_usable_ends() {
        let m = [false, true, true, false];
        assert_eq!(menu_nav_target(&m, Some(2), Home), Some(1));
        assert_eq!(menu_nav_target(&m, Some(1), End), Some(2));
        assert_eq!(menu_nav_target(&m, None, Home), Some(1));
    }

    #[test]
    fn single_entry_and_out_of_range_current() {
        assert_eq!(menu_nav_target(&[true], Some(0), Down), Some(0));
        assert_eq!(menu_nav_target(&[true], Some(0), Up), Some(0));
        assert_eq!(menu_nav_target(&M, Some(9), Down), Some(0));
    }

    #[test]
    fn nothing_focusable() {
        for nav in [Up, Down, Home, End] {
            assert_eq!(menu_nav_target(&[], None, nav), None);
            assert_eq!(menu_nav_target(&[false, false], Some(0), nav), None);
        }
    }

    #[test]
    fn menu_keys() {
        let none = gdk::ModifierType::empty();
        let shift = gdk::ModifierType::SHIFT_MASK;
        assert!(is_menu_key(gdk::Key::Menu, none));
        assert!(is_menu_key(gdk::Key::F10, shift));
        assert!(!is_menu_key(gdk::Key::F10, none));
        assert!(!is_menu_key(gdk::Key::F10, shift | gdk::ModifierType::CONTROL_MASK));
        assert!(!is_menu_key(gdk::Key::F2, shift));
    }
}

/// Moves focus between the entries of a `popup_menu` box.
fn menu_focus(bx: &gtk::Box, nav: MenuNav) {
    let kids: Vec<gtk::Widget> =
        std::iter::successors(bx.first_child(), |w| w.next_sibling()).collect();
    let usable: Vec<bool> =
        kids.iter().map(|w| w.is::<gtk::Button>() && w.is_visible() && w.is_sensitive()).collect();
    let current = bx.focus_child().and_then(|f| kids.iter().position(|w| *w == f));
    if let Some(i) = menu_nav_target(&usable, current, nav) {
        kids[i].grab_focus();
    }
}

impl App {
    /// A compact popup menu built from plain buttons: every option is
    /// visible at once, no scrolling.
    pub(super) fn popup_menu(&self, parent: &impl IsA<gtk::Widget>, x: f64, y: f64, items: Vec<Option<MItem>>) {
        let pop = gtk::Popover::new();
        pop.add_css_class("ctx-menu");
        pop.set_has_arrow(false);
        pop.set_position(gtk::PositionType::Bottom);
        // The role can only be set at construction
        let bx = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .accessible_role(gtk::AccessibleRole::Menu)
            .build();
        let mut last_sep = true;
        for it in items {
            match it {
                None => {
                    if !last_sep {
                        bx.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                        last_sep = true;
                    }
                }
                Some(m) => {
                    let b = gtk::Button::builder().accessible_role(gtk::AccessibleRole::MenuItem).build();
                    b.update_property(&[gtk::accessible::Property::Label(&m.label)]);
                    b.add_css_class("flat");
                    b.add_css_class("ctx-item");
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 28);
                    let l = gtk::Label::new(Some(&m.label));
                    l.set_xalign(0.0);
                    l.set_hexpand(true);
                    row.append(&l);
                    if !m.accel.is_empty() {
                        let k = gtk::Label::new(Some(m.accel));
                        k.add_css_class("accel");
                        row.append(&k);
                    }
                    b.set_child(Some(&row));
                    b.set_action_name(Some(m.action));
                    if let Some(t) = &m.target {
                        b.set_action_target_value(Some(t));
                    }
                    let p = pop.downgrade();
                    b.connect_clicked(move |_| {
                        if let Some(p) = p.upgrade() {
                            p.popdown()
                        }
                    });
                    bx.append(&b);
                    last_sep = false;
                }
            }
        }
        if let Some(last) = bx.last_child() {
            if last.is::<gtk::Separator>() {
                bx.remove(&last);
            }
        }
        pop.set_child(Some(&bx));
        pop.set_parent(parent);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });

        // Keyboard: arrows, Home/End and Tab move between entries; Escape goes back to
        // whatever had focus. Capture, so the box's own arrow handling doesn't stop at the ends.
        let prev = parent.root().and_then(|r| r.focus()).map(|w| w.downgrade());
        let k = gtk::EventControllerKey::new();
        k.set_propagation_phase(gtk::PropagationPhase::Capture);
        let b = bx.downgrade();
        let p = pop.downgrade();
        k.connect_key_pressed(move |_, key, _, mods| {
            let Some(bx) = b.upgrade() else { return glib::Propagation::Proceed };
            let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
            let nav = match key {
                gdk::Key::Up | gdk::Key::KP_Up => MenuNav::Up,
                gdk::Key::Down | gdk::Key::KP_Down => MenuNav::Down,
                gdk::Key::Home | gdk::Key::KP_Home => MenuNav::Home,
                gdk::Key::End | gdk::Key::KP_End => MenuNav::End,
                gdk::Key::ISO_Left_Tab => MenuNav::Up,
                gdk::Key::Tab | gdk::Key::KP_Tab => if shift { MenuNav::Up } else { MenuNav::Down },
                gdk::Key::Escape => {
                    // Focus back first, so closing doesn't drop it somewhere else
                    if let Some(w) = prev.as_ref().and_then(|w| w.upgrade()) {
                        w.grab_focus();
                    }
                    if let Some(p) = p.upgrade() {
                        p.popdown();
                    }
                    return glib::Propagation::Stop;
                }
                _ => return glib::Propagation::Proceed,
            };
            menu_focus(&bx, nav);
            glib::Propagation::Stop
        });
        pop.add_controller(k);

        pop.popup();
        menu_focus(&bx, MenuNav::Home);
    }

    /// Menu key / Shift+F10 on a file view: the item menu for the selection, anchored at
    /// the focused item when it is in view, or the background menu when nothing is selected.
    pub(super) fn keyboard_menu(&self, scroll: &gtk::ScrolledWindow) {
        let Some(info) = self.selected_infos().into_iter().next() else {
            self.popup_menu(scroll, 16.0, 16.0, self.background_menu());
            return;
        };
        // Like right-click, attach to the view stack, not the tile
        let stack = &self.view_stack;
        let visible = scroll.compute_bounds(stack);
        let focus = scroll.root().and_then(|r| r.focus()).filter(|f| {
            f.is_ancestor(scroll) && !f.is::<gtk::GridView>() && !f.is::<gtk::ListView>() && !f.is::<gtk::ColumnView>()
        });
        let at = focus
            .and_then(|f| f.compute_bounds(stack))
            .filter(|r| visible.is_none_or(|v| v.intersection(r).is_some()))
            .map(|r| (r.x() + (r.width() / 2.0).min(48.0), r.y() + r.height() * 0.75));
        let (x, y) = at.unwrap_or_else(|| visible.map_or((16.0, 16.0), |v| (v.x() + 16.0, v.y() + 16.0)));
        self.popup_menu(stack, x as f64, y as f64, self.item_menu(&info));
    }

    pub(super) fn item_menu(&self, info: &gio::FileInfo) -> Vec<Option<MItem>> {
        let many = self.selected_infos().len() > 1;
        let dir = is_dir_like(info);
        let trash = self.in_trash();
        let cfg = self.config.borrow();
        let uri = item_file(info).uri().to_string();
        let mut m = Vec::new();
        // Only top-level trash items know where they came from
        let origs: Vec<Option<PathBuf>> = self.selected_infos().iter().map(crate::trash::orig_path).collect();
        if trash && crate::trash::can_restore(&origs) {
            m.push(mi(tr("Restore"), "win.restore", ""));
            m.push(None);
        }
        if !many {
            m.push(mi(tr("Open"), "win.open", "Enter"));
            if dir {
                m.push(mi(tr("Open in New Tab"), "win.open-tab", "Middle-click"));
                if !trash {
                    m.push(mi(tr("Open in Terminal"), "win.terminal-selected", ""));
                }
            } else {
                m.push(mi(tr("Open With…"), "win.open-with", ""));
            }
            m.push(None);
        }
        if !trash {
            m.push(mi(tr("Cut"), "win.cut", "Ctrl+X"));
        }
        m.push(mi(tr("Copy"), "win.copy", "Ctrl+C"));
        if dir && !many && !trash {
            m.push(mi(tr("Paste Into Folder"), "win.paste-into", ""));
        }
        if !trash {
            m.push(None);
            m.push(mi(tr("Compress…"), "win.compress", ""));
            let sel = self.selected_infos();
            if sel.iter().all(|i| !is_dir_like(i) && crate::archive::is_archive(&i.name().to_string_lossy())) {
                m.push(mi(tr("Extract Here"), "win.extract-here", ""));
                m.push(mi(tr("Extract to…"), "win.extract-to", ""));
            }
        }
        m.push(None);
        if !many {
            let starred = cfg.starred.contains(&uri);
            m.push(mi(if starred { tr("Unstar") } else { tr("Star") }, "win.star", ""));
            if dir {
                let pinned = cfg.pins.contains(&uri);
                let label = if pinned { tr("Unpin from Sidebar") } else { tr("Pin to Sidebar") };
                m.push(mi(label, "win.pin-selected", ""));
            }
            if !trash {
                m.push(mi(tr("Change Icon…"), "win.change-icon", ""));
                let has_custom = info
                    .attribute_string("metadata::custom-icon")
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if has_custom {
                    m.push(mi(tr("Reset Icon"), "win.reset-icon", ""));
                }
            }
            m.push(None);
            if !trash {
                m.push(mi(tr("Rename…"), "win.rename", "F2"));
            }
        } else {
            m.push(mi(tr("Star / Unstar"), "win.star", ""));
            m.push(None);
            if !trash {
                m.push(mi(tr("Rename…"), "win.rename", "F2"));
            }
        }
        m.push(mi(tr("Copy Path"), "win.copy-path", ""));
        if trash {
            m.push(mi(tr("Delete Permanently"), "win.trash", "Delete"));
        } else {
            if self.in_recent() {
                m.push(mi(tr("Remove from Recent"), "win.forget-recent", ""));
            }
            m.push(mi(tr("Move to Trash"), "win.trash", "Delete"));
            m.push(mi(tr("Delete Permanently…"), "win.delete-permanently", "Shift+Delete"));
        }
        m.push(None);
        m.push(mi(tr("Properties"), "win.properties", "Alt+Enter"));
        m
    }

    pub(super) fn background_menu(&self) -> Vec<Option<MItem>> {
        let mut m = Vec::new();
        // Left out when there is nothing to undo or redo
        let undo = [(false, "win.undo", "Ctrl+Z"), (true, "win.redo", "Ctrl+Shift+Z")];
        for (redo, action, accel) in undo {
            if let Some(label) = crate::undo::label(redo) {
                m.push(mi(label, action, accel));
            }
        }
        if !m.is_empty() {
            m.push(None);
        }
        if self.in_trash() {
            m.push(mi(tr("Empty Trash"), "win.empty-trash", ""));
        } else if self.current_dir().is_some() {
            m.push(mi(tr("New Folder…"), "win.new-folder", "Ctrl+Shift+N"));
            m.push(mi(tr("Paste"), "win.paste", "Ctrl+V"));
            m.push(mi(tr("Open in Terminal"), "win.terminal-here", "Ctrl+Alt+T"));
        }
        m.push(None);
        m.push(mi(tr("Select All"), "win.select-all", "Ctrl+A"));
        if self.current_dir().is_some() {
            let pinned = self.current_dir().map(|f| self.is_pinned(&f)).unwrap_or(false);
            let label = if pinned { tr("Unpin This Folder") } else { tr("Pin This Folder") };
            m.push(mi(label, "win.pin-current", "Ctrl+D"));
        }
        m.push(mi(
            if self.show_hidden.get() { tr("Hide Hidden Files") } else { tr("Show Hidden Files") },
            "win.toggle-hidden",
            "Ctrl+H",
        ));
        m.push(mi(tr("Reload"), "win.refresh", "F5"));
        m.push(None);
        let (key, desc) = self.sort.get();
        for (k, label) in [
            (SortKey::Name, tr("Sort by Name")),
            (SortKey::Size, tr("Sort by Size")),
            (SortKey::Modified, tr("Sort by Modified")),
            (SortKey::Type, tr("Sort by Type")),
        ] {
            let accel = if k == key { "✓" } else { "" };
            m.push(Some(MItem { label, action: "win.sort-by", target: Some(k.as_str().to_variant()), accel }));
        }
        m.push(mi(tr("Reversed Order"), "win.sort-reversed", if desc { "✓" } else { "" }));
        if self.current_dir().is_some() {
            m.push(None);
            m.push(mi(tr("Properties"), "win.properties", "Alt+Enter"));
        }
        m
    }
}
