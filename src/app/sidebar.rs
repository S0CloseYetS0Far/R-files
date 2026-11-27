use super::*;

/// Where a sidebar row goes.
pub(super) enum SideTarget {
    Place(Loc),
    /// A volume that isn't mounted: activating the row mounts it, then opens it
    Volume(gio::Volume),
}

/// What the Devices section needs to know about a volume; plain data so the choice is testable.
struct VolFacts {
    name: String,
}

/// What the Devices section needs to know about a mount.
struct MountFacts {
    /// Index into the volumes list of the volume this mount belongs to, if it's listed
    volume: Option<usize>,
    shadowed: bool,
}

/// One Devices row: a volume (mounted or not) or a mount that has no listed volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevRow {
    Volume(usize),
    Mount(usize),
}

/// Rows for the Devices section, each drive once: every named volume, then the mounts
/// no shown volume covers (network shares, MTP phones…), leaving out shadowed ones.
fn device_rows(vols: &[VolFacts], mounts: &[MountFacts]) -> Vec<DevRow> {
    let shown: Vec<bool> = vols.iter().map(|v| !v.name.trim().is_empty()).collect();
    let mut rows: Vec<DevRow> = (0..vols.len()).filter(|&i| shown[i]).map(DevRow::Volume).collect();
    rows.extend(
        mounts
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.shadowed && !m.volume.is_some_and(|v| shown.get(v).copied().unwrap_or(false)))
            .map(|(i, _)| DevRow::Mount(i)),
    );
    rows
}

/// Tooltip of a mounted row's eject button, or None when it gets no button.
fn eject_tooltip(can_eject: bool, can_unmount: bool) -> Option<&'static str> {
    if can_eject {
        Some("Eject")
    } else if can_unmount {
        Some("Unmount")
    } else {
        None
    }
}

/// Status bar text once an eject or unmount of `name` has finished.
fn eject_message(name: &str, eject: bool, err: Option<&str>) -> String {
    match (err, eject) {
        (None, true) => format!("You can now remove “{name}”"),
        (None, false) => format!("Unmounted “{name}”"),
        (Some(e), true) => format!("Couldn't eject “{name}”: {e}"),
        (Some(e), false) => format!("Couldn't unmount “{name}”: {e}"),
    }
}

/// Whether `loc` shows a folder on the mount whose root is `root`.
fn loc_on_mount(loc: &Loc, root: &gio::File) -> bool {
    let f = match loc {
        Loc::Dir(f) | Loc::Search { root: f, .. } => f,
        Loc::Starred | Loc::RecentDownloads => return false,
    };
    f.equal(root) || f.has_prefix(root)
}

#[cfg(test)]
mod devices_tests {
    use super::{device_rows, eject_message, eject_tooltip, loc_on_mount, DevRow, Loc, MountFacts, VolFacts};
    use gtk::gio;
    use gtk::prelude::*;

    fn vol(name: &str) -> VolFacts {
        VolFacts { name: name.into() }
    }

    #[test]
    fn volumes_and_their_mounts_show_once() {
        let vols = [vol("USB stick"), vol("Data")];
        // Mount 0 is the USB stick's, mount 1 a network share with no volume
        let mounts = [MountFacts { volume: Some(0), shadowed: false }, MountFacts { volume: None, shadowed: false }];
        assert_eq!(device_rows(&vols, &mounts), vec![DevRow::Volume(0), DevRow::Volume(1), DevRow::Mount(1)]);
    }

    #[test]
    fn nameless_volumes_and_shadowed_mounts_are_left_out() {
        let vols = [vol(""), vol("  "), vol("Backup")];
        let mounts = [
            MountFacts { volume: None, shadowed: true },
            // Its volume is hidden, so the mount shows on its own
            MountFacts { volume: Some(0), shadowed: false },
            MountFacts { volume: Some(2), shadowed: false },
        ];
        assert_eq!(device_rows(&vols, &mounts), vec![DevRow::Volume(2), DevRow::Mount(1)]);
        assert!(device_rows(&[], &[]).is_empty());
    }

    #[test]
    fn eject_button_and_tooltip() {
        assert_eq!(eject_tooltip(true, true), Some("Eject"));
        assert_eq!(eject_tooltip(true, false), Some("Eject"));
        assert_eq!(eject_tooltip(false, true), Some("Unmount"));
        assert_eq!(eject_tooltip(false, false), None);
    }

    #[test]
    fn eject_messages() {
        assert_eq!(eject_message("Stick", true, None), "You can now remove “Stick”");
        assert_eq!(eject_message("Data", false, None), "Unmounted “Data”");
        assert_eq!(eject_message("Stick", true, Some("busy")), "Couldn't eject “Stick”: busy");
        assert_eq!(eject_message("Data", false, Some("busy")), "Couldn't unmount “Data”: busy");
    }

    #[test]
    fn tabs_on_a_mount() {
        let root = gio::File::for_path("/media/me/STICK");
        assert!(loc_on_mount(&Loc::Dir(root.clone()), &root));
        assert!(loc_on_mount(&Loc::Dir(root.child("a/b")), &root));
        assert!(!loc_on_mount(&Loc::Dir(gio::File::for_path("/media/me/STICK2")), &root));
        assert!(!loc_on_mount(&Loc::Dir(gio::File::for_path("/media")), &root));
        assert!(!loc_on_mount(&Loc::Starred, &root));
    }
}

impl App {
    // ================= sidebar =================
    /// Built-in places: (key, icon, name, location)
    fn places(&self) -> Vec<(&'static str, &'static str, String, Loc)> {
        let mut v = vec![
            ("home", "user-home-symbolic", "Home".into(), Loc::Dir(home())),
            ("recent", "document-open-recent-symbolic", "Recent".into(), Loc::Dir(gio::File::for_uri("recent:///"))),
            ("starred", "starred-symbolic", "Starred".into(), Loc::Starred),
        ];
        if crate::util::downloads_dir().is_some() {
            v.push(("recent-downloads", "folder-download-symbolic", "Recent Downloads".into(), Loc::RecentDownloads));
        }
        let specials = [
            ("desktop", glib::UserDirectory::Desktop, "user-desktop-symbolic", "Desktop"),
            ("documents", glib::UserDirectory::Documents, "folder-documents-symbolic", "Documents"),
            ("downloads", glib::UserDirectory::Downloads, "folder-download-symbolic", "Downloads"),
            ("music", glib::UserDirectory::Music, "folder-music-symbolic", "Music"),
            ("pictures", glib::UserDirectory::Pictures, "folder-pictures-symbolic", "Pictures"),
            ("videos", glib::UserDirectory::Videos, "folder-videos-symbolic", "Videos"),
        ];
        for (key, d, icon, name) in specials {
            if let Some(p) = glib::user_special_dir(d) {
                if p != glib::home_dir() && p.is_dir() {
                    // The folder's own name ("Dokumente"), not the English one
                    let name = crate::i18n::special_dir_name(&p, name);
                    v.push((key, icon, name, Loc::Dir(gio::File::for_path(p))));
                }
            }
        }
        v.push(("trash", "user-trash-symbolic", "Trash".into(), Loc::Dir(gio::File::for_uri("trash:///"))));

        // Apply the user's drag-and-drop order
        let order = self.config.borrow().places_order.clone();
        let rank = |k: &str| order.iter().position(|o| o == k).unwrap_or(usize::MAX);
        let mut indexed: Vec<_> = v.into_iter().enumerate().collect();
        indexed.sort_by_key(|(i, p)| (rank(p.0), *i));
        indexed.into_iter().map(|(_, p)| p).collect()
    }

    pub(super) fn rebuild_sidebar(self: &Rc<Self>) {
        // Pins known to be missing stay dimmed until the new check answers, so they don't flicker
        let was_missing: Vec<String> = (0..self.sidebar_targets.borrow().len())
            .filter(|&i| self.sidebar.row_at_index(i as i32).is_some_and(|r| r.has_css_class("pin-missing")))
            .filter_map(|i| self.sidebar_loc(i).map(|l| l.key()))
            .collect();
        while let Some(c) = self.sidebar.first_child() {
            self.sidebar.remove(&c);
        }
        self.sidebar_targets.borrow_mut().clear();

        for (key, icon, name, loc) in self.places() {
            self.side_row(icon, &name, loc, None, Some(format!("place:{key}")));
        }

        self.side_header("Pinned");
        crate::dnd::attach_pin_target(self, &self.sidebar);
        let pins = self.config.borrow().pins.clone();
        if pins.is_empty() {
            self.side_hint("Right-click a folder → Pin to Sidebar");
            crate::dnd::attach_pin_target(self, &self.sidebar);
        }
        let mut check = Vec::new();
        for uri in pins {
            let f = gio::File::for_uri(&uri);
            let key = format!("pin:{uri}");
            let title = file_title(&f);
            let loc = Loc::Dir(f);
            let missing = was_missing.contains(&loc.key());
            self.side_row("folder-symbolic", &title, loc, Some(uri.clone()), Some(key));
            if let Some(row) = self.sidebar.last_child().and_downcast::<gtk::ListBoxRow>() {
                if missing {
                    row.add_css_class("pin-missing");
                }
                check.push((uri, row.downgrade()));
            }
        }
        self.mark_missing_pins(check);

        self.side_header("Devices");
        self.side_row("drive-harddisk-symbolic", "Computer", Loc::Dir(gio::File::for_path("/")), None, None);
        let vols = self.volumes.volumes();
        let mounts = self.volumes.mounts();
        let vol_facts: Vec<VolFacts> = vols.iter().map(|v| VolFacts { name: v.name().to_string() }).collect();
        let mount_facts: Vec<MountFacts> = mounts
            .iter()
            .map(|m| MountFacts {
                // Match by object, or by root in case the monitor hands out different wrappers
                volume: m.volume().and_then(|mv| vols.iter().position(|v| *v == mv)).or_else(|| {
                    vols.iter().position(|v| v.get_mount().is_some_and(|vm| vm.root().equal(&m.root())))
                }),
                shadowed: m.is_shadowed(),
            })
            .collect();
        for row in device_rows(&vol_facts, &mount_facts) {
            let (mount, vol) = match row {
                DevRow::Volume(i) => (vols[i].get_mount(), Some(&vols[i])),
                DevRow::Mount(i) => (Some(mounts[i].clone()), None),
            };
            if let Some(m) = mount {
                self.side_row_gicon(&m.symbolic_icon(), &m.name(), SideTarget::Place(Loc::Dir(m.root())), None, None);
                self.add_eject_button(&m);
            } else if let Some(v) = vol {
                self.side_row_gicon(&v.symbolic_icon(), &v.name(), SideTarget::Volume(v.clone()), None, None);
            }
        }
        self.side_row("network-workgroup-symbolic", "Network", Loc::Dir(gio::File::for_uri("network:///")), None, None);

        let loc = self.tabs.borrow().get(self.current.get()).map(|t| t.loc.clone());
        if let Some(loc) = loc {
            self.highlight_sidebar(&loc);
        }
    }

    fn side_row(self: &Rc<Self>, icon: &str, name: &str, loc: Loc, pin: Option<String>, drag_key: Option<String>) {
        let gicon: gio::Icon = gio::ThemedIcon::new(icon).upcast();
        self.side_row_gicon(&gicon, name, SideTarget::Place(loc), pin, drag_key);
    }

    /// A sidebar row's menu (right-click or Menu key). `drag_key`: the row can be moved; `pin`: its pinned URI.
    fn sidebar_menu(idx: usize, drag_key: Option<&str>, pin: Option<&str>) -> Vec<Option<MItem>> {
        let mut items = vec![Some(MItem {
            label: "Open in New Tab".into(),
            action: "win.sidebar-tab",
            target: Some((idx as i32).to_variant()),
            accel: "",
        })];
        if let Some(k) = drag_key {
            items.push(None);
            let target = Some(k.to_variant());
            items.push(Some(MItem { label: "Move Up".into(), action: "win.side-up", target: target.clone(), accel: "" }));
            items.push(Some(MItem { label: "Move Down".into(), action: "win.side-down", target, accel: "" }));
            items.push(None);
        }
        if let Some(uri) = pin {
            items.push(Some(MItem {
                label: "Unpin".into(),
                action: "win.unpin",
                target: Some(uri.to_variant()),
                accel: "",
            }));
        }
        items
    }

    fn side_row_gicon(
        self: &Rc<Self>,
        icon: &gio::Icon,
        name: &str,
        target: SideTarget,
        pin: Option<String>,
        drag_key: Option<String>,
    ) {
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let img = gtk::Image::from_gicon(icon);
        let lbl = gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        bx.append(&img);
        bx.append(&lbl);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&bx));
        match &target {
            SideTarget::Place(Loc::Dir(f)) => {
                row.set_tooltip_text(Some(&display_path(f)));
                crate::dnd::attach_folder_row(self, &row, f);
            }
            SideTarget::Volume(_) => row.set_tooltip_text(Some("Not mounted: click to mount")),
            SideTarget::Place(_) => {}
        }
        let idx = {
            let mut t = self.sidebar_targets.borrow_mut();
            t.push(Some(target));
            t.len() - 1
        };

        // Menu key / Shift+F10 on the focused row
        let k = gtk::EventControllerKey::new();
        let a = Rc::downgrade(self);
        let r = row.downgrade();
        let (menu_key, menu_pin) = (drag_key.clone(), pin.clone());
        k.connect_key_pressed(move |_, key, _, mods| {
            if !is_menu_key(key, mods) {
                return glib::Propagation::Proceed;
            }
            let (Some(a), Some(r)) = (a.upgrade(), r.upgrade()) else { return glib::Propagation::Proceed };
            let items = Self::sidebar_menu(idx, menu_key.as_deref(), menu_pin.as_deref());
            a.popup_menu(&r, 24.0, r.height() as f64 * 0.75, items);
            glib::Propagation::Stop
        });
        row.add_controller(k);

        // Right-click / middle-click
        let g = gtk::GestureClick::new();
        g.set_button(0);
        let a = Rc::downgrade(self);
        let r = row.downgrade();
        let menu_key = drag_key.clone();
        g.connect_pressed(move |g, _, x, y| {
            let Some(a) = a.upgrade() else { return };
            let Some(r) = r.upgrade() else { return };
            match g.current_button() {
            3 => {
                g.set_state(gtk::EventSequenceState::Claimed);
                a.popup_menu(&r, x, y, Self::sidebar_menu(idx, menu_key.as_deref(), pin.as_deref()));
            }
            2 => {
                g.set_state(gtk::EventSequenceState::Claimed);
                a.open_sidebar_in_tab(idx);
            }
            _ => {}
            }
        });
        row.add_controller(g);

        // Drag to reorder
        if let Some(key) = drag_key {
            let src = gtk::DragSource::new();
            src.set_actions(gdk::DragAction::MOVE);
            let k = key.clone();
            src.connect_prepare(move |_, _, _| Some(gdk::ContentProvider::for_value(&k.to_value())));
            let r = row.downgrade();
            src.connect_drag_begin(move |s, _| {
                if let Some(r) = r.upgrade() {
                    s.set_icon(Some(&gtk::WidgetPaintable::new(Some(&r))), 20, 16);
                }
            });
            row.add_controller(src);

            let drop = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
            crate::dnd::ignore_file_drags(&drop);
            let r = row.downgrade();
            drop.connect_motion(move |_, _, y| {
                let Some(r) = r.upgrade() else { return gdk::DragAction::empty() };
                let above = y < r.height() as f64 / 2.0;
                r.remove_css_class(if above { "drop-below" } else { "drop-above" });
                r.add_css_class(if above { "drop-above" } else { "drop-below" });
                gdk::DragAction::MOVE
            });
            let r = row.downgrade();
            drop.connect_leave(move |_| {
                if let Some(r) = r.upgrade() {
                    r.remove_css_class("drop-above");
                    r.remove_css_class("drop-below");
                }
            });
            let a = Rc::downgrade(self);
            let r = row.downgrade();
            drop.connect_drop(move |_, value, _, y| {
                let Some(a) = a.upgrade() else { return false };
                let Some(r) = r.upgrade() else { return false };
                r.remove_css_class("drop-above");
                r.remove_css_class("drop-below");
                let Ok(src) = value.get::<String>() else { return false };
                let before = y < r.height() as f64 / 2.0;
                let a = a.clone();
                let dst = key.clone();
                // Rebuild after the drop finishes, not while GTK is still using these rows
                glib::idle_add_local_once(move || a.reorder_sidebar(&src, &dst, before));
                true
            });
            row.add_controller(drop);
        }

        self.sidebar.append(&row);
    }

    /// Moves `src` next to `dst` in `list`; false (and the list unchanged) when either is missing.
    fn move_in(list: &mut Vec<String>, src: &str, dst: &str, before: bool) -> bool {
        let Some(si) = list.iter().position(|k| k == src) else { return false };
        let item = list.remove(si);
        let Some(di) = list.iter().position(|k| k == dst) else {
            list.insert(si, item);
            return false;
        };
        list.insert(if before { di } else { di + 1 }, item);
        true
    }

    fn reorder_sidebar(self: &Rc<Self>, src: &str, dst: &str, before: bool) {
        if src == dst {
            return;
        }
        match (src.split_once(':'), dst.split_once(':')) {
            (Some(("place", s)), Some(("place", d))) => {
                let mut order: Vec<String> = self.places().iter().map(|p| p.0.to_string()).collect();
                if Self::move_in(&mut order, s, d, before) {
                    self.config.borrow_mut().places_order = order;
                    self.save_config();
                }
            }
            (Some(("pin", s)), Some(("pin", d))) => {
                let moved = Self::move_in(&mut self.config.borrow_mut().pins, s, d, before);
                if moved {
                    self.save_config();
                }
            }
            _ => {
                self.status.set_text("Places and pinned folders can be reordered within their own section");
                return;
            }
        }
        Self::pins_changed();
    }

    /// "Move Up" / "Move Down" from the sidebar's right-click menu.
    pub(super) fn move_sidebar_item(self: &Rc<Self>, key: &str, up: bool) {
        let keys: Vec<String> = match key.split_once(':') {
            Some(("place", _)) => self.places().iter().map(|p| format!("place:{}", p.0)).collect(),
            Some(("pin", _)) => self.config.borrow().pins.iter().map(|u| format!("pin:{u}")).collect(),
            _ => return,
        };
        let Some(i) = keys.iter().position(|k| k == key) else { return };
        let j = if up { i.checked_sub(1) } else { Some(i + 1).filter(|&j| j < keys.len()) };
        if let Some(j) = j {
            self.reorder_sidebar(key, &keys[j], up);
        }
    }

    fn side_header(&self, text: &str) {
        let lbl = gtk::Label::new(Some(text));
        lbl.set_xalign(0.0);
        lbl.add_css_class("sidebar-header");
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&lbl));
        row.set_activatable(false);
        row.set_selectable(false);
        self.sidebar.append(&row);
        self.sidebar_targets.borrow_mut().push(None);
    }

    fn side_hint(&self, text: &str) {
        let lbl = gtk::Label::new(Some(text));
        lbl.set_xalign(0.0);
        lbl.set_wrap(true);
        lbl.add_css_class("sidebar-hint");
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&lbl));
        row.set_activatable(false);
        row.set_selectable(false);
        self.sidebar.append(&row);
        self.sidebar_targets.borrow_mut().push(None);
    }

    pub(super) fn sidebar_loc(&self, idx: usize) -> Option<Loc> {
        match self.sidebar_targets.borrow().get(idx) {
            Some(Some(SideTarget::Place(loc))) => Some(loc.clone()),
            _ => None,
        }
    }

    /// The unmounted volume at sidebar row `idx`, if that row is one.
    pub(super) fn sidebar_volume(&self, idx: usize) -> Option<gio::Volume> {
        match self.sidebar_targets.borrow().get(idx) {
            Some(Some(SideTarget::Volume(v))) => Some(v.clone()),
            _ => None,
        }
    }

    pub(super) fn open_sidebar_in_tab(self: &Rc<Self>, idx: usize) {
        if self.refuse_missing_pin(idx) {
            return;
        }
        if let Some(v) = self.sidebar_volume(idx) {
            self.mount_volume(v, true);
        } else if let Some(loc) = self.sidebar_loc(idx) {
            self.new_tab(loc);
        }
    }

    /// Mounts `vol` (asking for a password if it needs one), then opens it here or in a new tab.
    pub(super) fn mount_volume(self: &Rc<Self>, vol: gio::Volume, new_tab: bool) {
        let name = vol.name().to_string();
        self.status.set_text("Mounting…");
        let op = gtk::MountOperation::new(Some(&self.window));
        let a = Rc::downgrade(self);
        let v = vol.clone();
        vol.mount(gio::MountMountFlags::NONE, Some(&op), None::<&gio::Cancellable>, move |r| {
            let Some(a) = a.upgrade() else { return };
            match r {
                Ok(()) => match v.get_mount() {
                    Some(m) => {
                        let loc = Loc::Dir(m.root());
                        if new_tab {
                            a.new_tab(loc);
                        } else {
                            a.navigate(loc);
                        }
                    }
                    None => a.status.set_text(&format!("Mounted “{name}”, but can't find where")),
                },
                // The user cancelled the password prompt
                Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => a.update_status(),
                Err(e) => a.notify(crate::banner::Notice {
                    text: format!("Couldn't mount “{name}”: {}", e.message()),
                    errors: vec![e.message().to_string()],
                }),
            }
        });
    }

    /// Puts an eject (or unmount) button at the end of the sidebar row just added for `mount`.
    fn add_eject_button(self: &Rc<Self>, mount: &gio::Mount) {
        let Some(tip) = eject_tooltip(mount.can_eject(), mount.can_unmount()) else { return };
        let Some(bx) = self.sidebar.last_child().and_downcast::<gtk::ListBoxRow>().and_then(|r| r.child()) else {
            return;
        };
        let Ok(bx) = bx.downcast::<gtk::Box>() else { return };
        let btn = gtk::Button::from_icon_name("media-eject-symbolic");
        btn.add_css_class("flat");
        btn.add_css_class("sidebar-eject");
        btn.set_focus_on_click(false);
        btn.set_tooltip_text(Some(tip));
        btn.set_hexpand(true);
        btn.set_halign(gtk::Align::End);
        btn.set_valign(gtk::Align::Center);
        let a = Rc::downgrade(self);
        let m = mount.downgrade();
        // The button claims its click, so the row isn't activated as well
        btn.connect_clicked(move |_| {
            if let (Some(a), Some(m)) = (a.upgrade(), m.upgrade()) {
                a.eject_mount(m);
            }
        });
        bx.append(&btn);
    }

    /// Ejects or unmounts `mount`, first moving this window's tabs off it so we don't keep it busy.
    fn eject_mount(self: &Rc<Self>, mount: gio::Mount) {
        let name = mount.name().to_string();
        self.leave_mount(&mount.root());
        let eject = mount.can_eject();
        self.status.set_text(&format!("{} “{name}”…", if eject { "Ejecting" } else { "Unmounting" }));
        let op = gtk::MountOperation::new(Some(&self.window));
        // "Writing data, don't unplug" notes while the cache is flushed. Read as values: the message may be NULL.
        let a = Rc::downgrade(self);
        op.connect_local("show-unmount-progress", false, move |args| {
            let a = a.upgrade()?;
            let msg = args.get(1).and_then(|v| v.get::<Option<String>>().ok()).flatten()?;
            let time_left = args.get(2).and_then(|v| v.get::<i64>().ok()).unwrap_or(0);
            let bytes_left = args.get(3).and_then(|v| v.get::<i64>().ok()).unwrap_or(0);
            // Zero and zero means done; the finish callback reports that
            if time_left != 0 || bytes_left != 0 {
                a.status.set_text(&msg.lines().collect::<Vec<_>>().join(": "));
            }
            None
        });
        let a = Rc::downgrade(self);
        let done = move |r: Result<(), glib::Error>| {
            let Some(a) = a.upgrade() else { return };
            match r {
                // The user dismissed the "still in use" dialog
                Err(e) if e.matches(gio::IOErrorEnum::FailedHandled) => a.update_status(),
                Err(e) => a.notify(crate::banner::Notice {
                    text: eject_message(&name, eject, Some(e.message())),
                    errors: vec![e.message().to_string()],
                }),
                Ok(()) => a.notify(crate::banner::Notice::ok(eject_message(&name, eject, None))),
            }
        };
        let flags = gio::MountUnmountFlags::NONE;
        if eject {
            mount.eject_with_operation(flags, Some(&op), None::<&gio::Cancellable>, done);
        } else {
            mount.unmount_with_operation(flags, Some(&op), None::<&gio::Cancellable>, done);
        }
    }

    /// Sends every tab of this window that shows a folder under `root` to Home.
    fn leave_mount(self: &Rc<Self>, root: &gio::File) {
        let current = self.current.get();
        let mut leave_current = false;
        for (i, t) in self.tabs.borrow_mut().iter_mut().enumerate() {
            if !loc_on_mount(&t.loc, root) {
                continue;
            }
            if i == current {
                leave_current = true;
                continue;
            }
            let old = std::mem::replace(&mut t.loc, Loc::Dir(home()));
            t.back.push((old, t.view.take()));
            t.fwd.clear();
            set_tab_title(&t.widget, &t.label, &t.loc.title());
            t.icon.set_icon_name(Some(t.loc.icon()));
            t.widget.set_tooltip_text(Some(&display_path(&home())));
        }
        if leave_current {
            self.navigate(Loc::Dir(home()));
        }
        self.refresh_tab_titles();
    }

    pub(super) fn highlight_sidebar(&self, loc: &Loc) {
        let key = loc.key();
        let idx = self
            .sidebar_targets
            .borrow()
            .iter()
            .position(|t| matches!(t, Some(SideTarget::Place(l)) if l.key() == key));
        match idx.and_then(|i| self.sidebar.row_at_index(i as i32)) {
            Some(row) => self.sidebar.select_row(Some(&row)),
            None => self.sidebar.unselect_all(),
        }
    }

    pub(crate) fn set_pinned(self: &Rc<Self>, uri: String, pinned: bool) {
        if pinned && !crate::config::storable(&uri) {
            self.status.set_text("Folders with a line break in their address can't be pinned");
            return;
        }
        {
            let mut c = self.config.borrow_mut();
            c.pins.retain(|u| u != &uri);
            if pinned {
                c.pins.push(uri);
            }
        }
        self.save_config();
        Self::pins_changed();
    }

    /// Keeps pins and stars on items RFiles renamed: `renames` are (old URI, new URI).
    pub(crate) fn note_renames(self: &Rc<Self>, renames: &[(String, String)]) {
        let changed = self.config.borrow_mut().apply_renames(renames);
        if changed {
            self.save_config();
            Self::pins_changed();
            Self::stars_changed();
        }
    }

    /// Pins or the places order changed: every window shows them again.
    fn pins_changed() {
        for_each_window(|a| {
            a.rebuild_sidebar();
            a.update_pin_button();
        });
    }

    /// Stars changed: windows showing Starred show the new list.
    pub(super) fn stars_changed() {
        for_each_window(|a| {
            if matches!(a.current_loc(), Loc::Starred) {
                a.reload();
            }
        });
    }

    /// The folder icon or icon size changed (already applied process-wide): every window redraws its icons, and
    /// the others' settings menus show the new choice.
    pub(super) fn icons_changed(self: &Rc<Self>) {
        for_each_window(|a| {
            if !Rc::ptr_eq(a, self) {
                a.setup_settings_popover();
            }
            a.reload();
        });
    }

    /// Dims pinned folders that aren't there, once a check off the UI thread says so.
    fn mark_missing_pins(&self, rows: Vec<(String, glib::WeakRef<gtk::ListBoxRow>)>) {
        if rows.is_empty() {
            return;
        }
        let uris: Vec<String> = rows.iter().map(|(u, _)| u.clone()).collect();
        glib::spawn_future_local(async move {
            let Ok(missing) =
                gio::spawn_blocking(move || uris.iter().map(|u| crate::config::pin_is_missing(u)).collect::<Vec<_>>())
                    .await
            else {
                return;
            };
            // Rows of a sidebar rebuilt meanwhile are gone and don't upgrade
            for ((uri, row), missing) in rows.into_iter().zip(missing) {
                let Some(row) = row.upgrade() else { continue };
                let path = display_path(&gio::File::for_uri(&uri));
                if missing {
                    row.add_css_class("pin-missing");
                    row.set_tooltip_text(Some(&format!("Missing: {path}")));
                } else {
                    row.remove_css_class("pin-missing");
                    row.set_tooltip_text(Some(&path));
                }
            }
        });
    }

    /// Shows why a missing pinned folder can't be opened; returns whether row `idx` is one.
    pub(super) fn refuse_missing_pin(self: &Rc<Self>, idx: usize) -> bool {
        let Some(row) = self.sidebar.row_at_index(idx as i32) else { return false };
        if !row.has_css_class("pin-missing") {
            return false;
        }
        let name = match self.sidebar_loc(idx) {
            Some(Loc::Dir(f)) => file_title(&f),
            _ => String::new(),
        };
        self.status.set_text(&format!(
            "“{name}” is missing: moved, renamed or deleted, or its drive is not connected. Right-click to unpin."
        ));
        // Look again, so a folder that is back opens on the next click
        let a = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(a) = a.upgrade() {
                a.rebuild_sidebar();
            }
        });
        true
    }

    pub(super) fn is_pinned(&self, f: &gio::File) -> bool {
        let uri = f.uri().to_string();
        self.config.borrow().pins.contains(&uri)
    }

    /// The pin button lights up when the current folder is pinned.
    pub(super) fn update_pin_button(&self) {
        let pinned = self.current_dir().map(|f| self.is_pinned(&f)).unwrap_or(false);
        if pinned {
            self.pin_btn.add_css_class("pinned");
            self.pin_btn.set_tooltip_text(Some("Unpin this folder from the sidebar (Ctrl+D)"));
        } else {
            self.pin_btn.remove_css_class("pinned");
            self.pin_btn.set_tooltip_text(Some("Pin this folder to the sidebar (Ctrl+D)"));
        }
        self.pin_btn.set_sensitive(self.current_dir().is_some());
    }
}

#[cfg(test)]
mod sidebar_move_tests {
    use super::App;

    fn moved(src: &str, dst: &str, before: bool) -> (bool, Vec<String>) {
        let mut list: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
        let ok = App::move_in(&mut list, src, dst, before);
        (ok, list)
    }

    #[test]
    fn moves_before_or_after() {
        assert_eq!(moved("a", "c", false), (true, ["b", "c", "a", "d"].map(String::from).to_vec()));
        assert_eq!(moved("a", "c", true), (true, ["b", "a", "c", "d"].map(String::from).to_vec()));
        assert_eq!(moved("d", "a", true), (true, ["d", "a", "b", "c"].map(String::from).to_vec()));
        assert_eq!(moved("d", "a", false), (true, ["a", "d", "b", "c"].map(String::from).to_vec()));
        assert_eq!(moved("b", "d", false), (true, ["a", "c", "d", "b"].map(String::from).to_vec()));
    }

    #[test]
    fn missing_items_leave_the_list_alone() {
        let same = ["a", "b", "c", "d"].map(String::from).to_vec();
        assert_eq!(moved("x", "a", true), (false, same.clone()));
        assert_eq!(moved("b", "x", true), (false, same.clone()));
        assert_eq!(moved("c", "c", false), (false, same));
    }
}
