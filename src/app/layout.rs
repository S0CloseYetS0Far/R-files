use super::*;

impl App {
    /// The preview pane's width can only be applied once the window has a
    /// real size, so wait for the first frame, then place the divider.
    pub(super) fn restore_layout(self: &Rc<Self>, preview_width: i32) {
        let a = self.clone();
        self.inner.add_tick_callback(move |p, _| {
            let w = p.width();
            if w <= 0 {
                return glib::ControlFlow::Continue;
            }
            p.set_position((w - preview_width).max(250));
            // Start saving changes only after the restore is applied
            let a = a.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                a.layout_ready.set(true)
            });
            glib::ControlFlow::Break
        });

        // Save shortly after any layout change, not only when closing, so
        // sizes survive however the app is closed.
        let watch = |obj: &glib::Object, prop: &str| {
            let a = Rc::downgrade(self);
            obj.connect_notify_local(Some(prop), move |_, _| {
                if let Some(a) = a.upgrade() {
                    a.schedule_save()
                }
            });
        };
        watch(self.outer.upcast_ref(), "position");
        watch(self.inner.upcast_ref(), "position");
        watch(self.window.upcast_ref(), "default-width");
        watch(self.window.upcast_ref(), "default-height");
        watch(self.window.upcast_ref(), "maximized");
    }

    fn schedule_save(self: &Rc<Self>) {
        if !self.layout_ready.get() || self.save_pending.replace(true) {
            return;
        }
        let a = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            a.save_pending.set(false);
            a.save_layout();
        });
    }

    pub(super) fn save_layout(&self) {
        let mut c = self.config.borrow_mut();
        let maximized = self.window.is_maximized() || self.window.is_fullscreen();
        c.maximized = maximized;
        if !maximized {
            let (w, h) = self.window.default_size();
            if w > 0 && h > 0 {
                c.width = w;
                c.height = h;
            }
        }
        if self.layout_ready.get() {
            self.note_widths(&mut c);
        }
        // List or grid, hidden files and the panes are saved when they are changed, so resizing or closing
        // one window doesn't put back what another window last chose
        drop(c);
        self.save_config();
    }

    /// Puts the pane widths into `c`. A hidden pane keeps the width it had.
    fn note_widths(&self, c: &mut Config) {
        let sidebar = self.side_scroll.get_visible().then(|| self.outer.position());
        let iw = self.inner.width();
        let inner = (self.preview.root.get_visible() && iw > 0).then(|| (iw, self.inner.position()));
        (c.sidebar_pos, c.preview_width, c.files_pos) =
            widths_to_save((c.sidebar_pos, c.preview_width, c.files_pos), sidebar, inner);
    }

    /// Moves keyboard focus to the file view if it is inside `pane`, which is about to be hidden.
    fn focus_files_if_in(&self, pane: &gtk::Widget) {
        let inside = GtkWindowExt::focus(&self.window).is_some_and(|f| &f == pane || f.is_ancestor(pane));
        if inside {
            if self.view_btn.is_active() {
                self.list.grab_focus();
            } else {
                self.grid.grab_focus();
            }
        }
    }

    /// Saves the shown/hidden state of the panes, once the window is up.
    fn save_pane_state(&self) {
        {
            let mut c = self.config.borrow_mut();
            c.show_sidebar = self.sidebar_btn.is_active();
            c.show_preview = self.preview_btn.is_active();
        }
        if self.window.is_mapped() {
            self.save_config();
        }
    }

    /// Shows or hides the sidebar (F9). GtkPaned gives the rest all the room and keeps its
    /// position (the sidebar width) while the sidebar is hidden.
    pub(super) fn set_sidebar_shown(&self, show: bool) {
        if self.side_scroll.get_visible() == show {
            return;
        }
        if !show {
            if self.layout_ready.get() {
                self.note_widths(&mut self.config.borrow_mut());
            }
            self.focus_files_if_in(self.side_scroll.upcast_ref());
        }
        self.side_scroll.set_visible(show);
        self.save_pane_state();
    }

    /// Shows or hides the preview pane (F4). While hidden nothing is previewed and a playing
    /// video stops; showing it again previews the selection at the width it had.
    // `get_visible()` (the widget's own flag), not `is_visible()`: the latter is also false while the window
    // is not shown yet, which made start-up take the "show" path before the first tab existed and crash.
    pub(super) fn set_preview_shown(self: &Rc<Self>, show: bool) {
        let root = &self.preview.root;
        if root.get_visible() == show {
            return;
        }
        if show {
            root.set_visible(true);
            let w = self.inner.width();
            // Not laid out yet: `restore_layout` places the divider
            if w > 0 {
                let pw = self.config.borrow().preview_width;
                self.inner.set_position(preview_show_position(self.preview_hidden_at.get(), w, pw));
            }
            self.on_selection_changed();
        } else {
            // Note its width now: a later save can't see it any more
            if self.layout_ready.get() {
                self.note_widths(&mut self.config.borrow_mut());
            }
            self.preview_hidden_at.set(self.inner.width());
            self.focus_files_if_in(root.upcast_ref());
            root.set_visible(false);
            *self.previewed.borrow_mut() = None;
            self.preview.clear();
        }
        self.save_pane_state();
    }
}

/// The (sidebar, preview, files) widths to save. `sidebar` is the outer divider position and
/// `inner` the inner paned's (width, divider position), each None while that pane is hidden:
/// its paned position then says nothing about it, so the width saved for it is kept.
fn widths_to_save(saved: (i32, i32, i32), sidebar: Option<i32>, inner: Option<(i32, i32)>) -> (i32, i32, i32) {
    let (mut side, mut preview, mut files) = saved;
    if let Some(pos) = sidebar {
        side = pos;
    }
    if let Some((width, pos)) = inner {
        preview = (width - pos).max(150);
        files = pos;
    }
    (side, preview, files)
}

#[cfg(test)]
mod pane_width_tests {
    use super::*;

    #[test]
    fn shown_panes_are_measured() {
        assert_eq!(widths_to_save((220, 420, 700), Some(250), Some((1000, 600))), (250, 400, 600));
        // A divider pushed to the end still saves a usable preview width
        assert_eq!(widths_to_save((220, 420, 700), Some(250), Some((1000, 990))), (250, 150, 990));
    }

    #[test]
    fn hidden_panes_keep_their_width() {
        assert_eq!(widths_to_save((220, 420, 700), None, Some((1200, 800))), (220, 400, 800));
        assert_eq!(widths_to_save((220, 420, 700), Some(180), None), (180, 420, 700));
        assert_eq!(widths_to_save((220, 420, 700), None, None), (220, 420, 700));
    }

    #[test]
    fn show_position_keeps_the_preview_width() {
        // GtkPaned later adds (width now - width when it last laid out both children);
        // the preview must come back `preview` wide either way.
        let pos = preview_show_position(1000, 1600, 420);
        assert_eq!(pos + 1600 - 1000, 1600 - 420);
        assert_eq!(preview_show_position(1600, 1000, 420) + 1000 - 1600, 1000 - 420);
        // Never laid out with the preview (started hidden): the position is used as is
        assert_eq!(preview_show_position(0, 1600, 420), 1180);
        // Never negative, which would unset the position
        assert!(preview_show_position(300, 1600, 420) > 0);
    }
}

/// Divider position that brings the preview pane back `preview` wide in an inner paned now
/// `width` wide. `hidden_at` is the paned's width when the preview was hidden, 0 if never:
/// GtkPaned shifts a set position by how much its width changed since it last laid out both
/// children (start side resizes, end doesn't), and that was at `hidden_at`.
fn preview_show_position(hidden_at: i32, width: i32, preview: i32) -> i32 {
    let base = if hidden_at > 0 { hidden_at } else { width };
    (base - preview).max(1)
}
