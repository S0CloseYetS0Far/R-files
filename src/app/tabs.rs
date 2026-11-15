use super::*;

/// Arrow and Home/End keys on a focused tab.
#[derive(Clone, Copy)]
enum TabNav {
    Left,
    Right,
    Home,
    End,
}

/// Which tab gets focus after `key` on tab `focused` of `n`. Stops at the first tab; Right on the last tab
/// returns `n`, meaning the + button.
fn tab_focus_target(n: usize, focused: usize, key: TabNav) -> usize {
    match key {
        TabNav::Left => focused.saturating_sub(1),
        TabNav::Right => (focused + 1).min(n),
        TabNav::Home => 0,
        TabNav::End => n.saturating_sub(1),
    }
}

/// The current tab after closing tab `i` of `n` while `cur` was current (`n` is the count before closing).
fn current_after_close(n: usize, cur: usize, i: usize) -> usize {
    if i < cur {
        cur - 1
    } else if i == cur {
        i.min(n - 2)
    } else {
        cur
    }
}

/// Most closed tabs remembered for reopening.
const CLOSED_TABS_MAX: usize = 10;

/// A closed tab: where it was and how to get back and forward from there.
pub(super) struct ClosedTab {
    loc: Loc,
    view: Option<ViewState>,
    back: Vec<(Loc, Option<ViewState>)>,
    fwd: Vec<(Loc, Option<ViewState>)>,
}

/// The last `CLOSED_TABS_MAX` items pushed; the oldest is forgotten first.
pub(super) struct ClosedStack<T>(Vec<T>);

impl<T> Default for ClosedStack<T> {
    fn default() -> Self {
        ClosedStack(Vec::new())
    }
}

impl<T> ClosedStack<T> {
    fn push(&mut self, item: T) {
        if self.0.len() >= CLOSED_TABS_MAX {
            self.0.remove(0);
        }
        self.0.push(item);
    }

    fn pop(&mut self) -> Option<T> {
        self.0.pop()
    }
}

/// Where tab `from` ends up (of `n`) when dropped before or after tab `over`; `None` when it stays put.
fn tab_move_to(n: usize, from: usize, over: usize, before: bool) -> Option<usize> {
    if from >= n || over >= n {
        return None;
    }
    // The gap it goes into, counted in the list as it is now
    let slot = if before { over } else { over + 1 };
    let to = if slot > from { slot - 1 } else { slot };
    (to != from).then_some(to)
}

/// The current tab's index after tab `from` moved to `to`.
fn current_after_move(cur: usize, from: usize, to: usize) -> usize {
    if cur == from {
        to
    } else if from < cur && to >= cur {
        cur - 1
    } else if from > cur && to <= cur {
        cur + 1
    } else {
        cur
    }
}

/// Tab titles with the parent folder added to any that share a title: "src — app".
/// Each entry is (title, parent folder's title if it has one).
fn tab_titles(items: &[(String, Option<String>)]) -> Vec<String> {
    items
        .iter()
        .enumerate()
        .map(|(i, (title, parent))| {
            let clash = items.iter().enumerate().any(|(j, (t, _))| j != i && t == title);
            match parent {
                Some(p) if clash => format!("{title} — {p}"),
                _ => title.clone(),
            }
        })
        .collect()
}

thread_local! {
    /// The tab widget being dragged along the strip.
    static TAB_DRAG: RefCell<Option<glib::WeakRef<gtk::Box>>> = const { RefCell::new(None) };
}

#[cfg(test)]
mod tab_extras_tests {
    use super::*;

    #[test]
    fn closed_stack_keeps_the_newest_ten() {
        let mut s = ClosedStack::default();
        for i in 0..13 {
            s.push(i);
        }
        assert_eq!(s.0.len(), CLOSED_TABS_MAX);
        assert_eq!(s.pop(), Some(12));
        assert_eq!(s.pop(), Some(11));
        assert_eq!(s.0.first(), Some(&3));
        let mut empty: ClosedStack<u8> = ClosedStack::default();
        assert_eq!(empty.pop(), None);
    }

    #[test]
    fn move_target() {
        assert_eq!(tab_move_to(3, 0, 2, false), Some(2));
        assert_eq!(tab_move_to(3, 0, 2, true), Some(1));
        assert_eq!(tab_move_to(3, 2, 0, true), Some(0));
        assert_eq!(tab_move_to(3, 2, 0, false), Some(1));
        assert_eq!(tab_move_to(3, 1, 2, true), None);
        assert_eq!(tab_move_to(3, 1, 0, false), None);
        assert_eq!(tab_move_to(3, 1, 1, true), None);
        assert_eq!(tab_move_to(3, 5, 0, true), None);
    }

    #[test]
    fn current_follows_the_move() {
        // 0 1 2 3 with tab 0 moved to 2 gives 1 2 0 3
        assert_eq!(current_after_move(0, 0, 2), 2);
        assert_eq!(current_after_move(1, 0, 2), 0);
        assert_eq!(current_after_move(2, 0, 2), 1);
        assert_eq!(current_after_move(3, 0, 2), 3);
        // tab 3 moved to 1 gives 0 3 1 2
        assert_eq!(current_after_move(1, 3, 1), 2);
        assert_eq!(current_after_move(2, 3, 1), 3);
        assert_eq!(current_after_move(0, 3, 1), 0);
        assert_eq!(current_after_move(3, 3, 1), 1);
    }

    fn item(t: &str, p: Option<&str>) -> (String, Option<String>) {
        (t.to_string(), p.map(str::to_string))
    }

    #[test]
    fn same_titles_get_the_parent() {
        let items = [item("src", Some("app")), item("docs", Some("app")), item("src", Some("lib"))];
        assert_eq!(tab_titles(&items), ["src — app", "docs", "src — lib"]);
    }

    #[test]
    fn same_titles_without_a_parent_stay_plain() {
        let items = [item("Starred", None), item("Starred", None), item("x", Some("y"))];
        assert_eq!(tab_titles(&items), ["Starred", "Starred", "x"]);
    }
}

#[cfg(test)]
mod tab_focus_tests {
    use super::*;

    #[test]
    fn arrows_stop_at_the_ends() {
        assert_eq!(tab_focus_target(3, 1, TabNav::Left), 0);
        assert_eq!(tab_focus_target(3, 0, TabNav::Left), 0);
        assert_eq!(tab_focus_target(3, 1, TabNav::Right), 2);
        assert_eq!(tab_focus_target(3, 2, TabNav::Right), 3); // the + button
        assert_eq!(tab_focus_target(1, 0, TabNav::Right), 1);
    }

    #[test]
    fn home_and_end() {
        assert_eq!(tab_focus_target(4, 2, TabNav::Home), 0);
        assert_eq!(tab_focus_target(4, 1, TabNav::End), 3);
        assert_eq!(tab_focus_target(1, 0, TabNav::End), 0);
    }

    #[test]
    fn current_after_closing() {
        // closing the current tab: the one that slid into its place, or the new last one
        assert_eq!(current_after_close(4, 1, 1), 1);
        assert_eq!(current_after_close(4, 3, 3), 2);
        assert_eq!(current_after_close(2, 0, 0), 0);
        // closing an earlier tab shifts the current one down, a later one leaves it
        assert_eq!(current_after_close(4, 2, 0), 1);
        assert_eq!(current_after_close(4, 1, 3), 1);
    }
}

pub(super) fn set_tab_title(widget: &gtk::Box, label: &gtk::Label, title: &str) {
    label.set_text(title);
    widget.update_property(&[gtk::accessible::Property::Label(title)]);
}

impl App {
    // ================= tabs =================
    pub fn new_tab(self: &Rc<Self>, loc: Loc) {
        self.open_tab(ClosedTab { loc, view: None, back: vec![], fwd: vec![] });
    }

    /// Ctrl+Shift+T: brings back the most recently closed tab with its history.
    pub(super) fn reopen_tab(self: &Rc<Self>) {
        let closed = self.closed_tabs.borrow_mut().pop();
        match closed {
            Some(c) => self.open_tab(c),
            None => self.status.set_text("No closed tab to reopen"),
        }
    }

    /// Ctrl+W: closes the tab; the last tab closes the window.
    pub(super) fn close_tab_or_window(self: &Rc<Self>) {
        if self.tabs.borrow().len() <= 1 {
            self.window.close();
        } else {
            self.close_tab(self.current.get());
        }
    }

    fn open_tab(self: &Rc<Self>, closed: ClosedTab) {
        // Focusable so the keyboard and screen readers can reach it; every tab is in the Tab order (no roving focus)
        let bx = gtk::Box::builder()
            .spacing(6)
            .accessible_role(gtk::AccessibleRole::Tab)
            .focusable(true)
            .focus_on_click(false)
            .build();
        bx.add_css_class("tab");
        let icon = gtk::Image::from_icon_name("folder-symbolic");
        let label = gtk::Label::builder()
            .ellipsize(pango::EllipsizeMode::End)
            .max_width_chars(18)
            .width_chars(6)
            .xalign(0.0)
            .build();
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.add_css_class("tab-close");
        close.set_focus_on_click(false);
        close.set_tooltip_text(Some("Close tab (Ctrl+W)"));
        close.update_property(&[gtk::accessible::Property::Label("Close tab")]);
        // The keyboard closes a tab with Delete on the tab itself
        close.set_focusable(false);
        bx.append(&icon);
        bx.append(&label);
        bx.append(&close);

        let g = gtk::GestureClick::new();
        g.set_button(0);
        let a = Rc::downgrade(self);
        let w = bx.downgrade();
        g.connect_pressed(move |g, _, _, _| {
            let Some(a) = a.upgrade() else { return };
            let Some(w) = w.upgrade() else { return };
            let Some(i) = a.tab_index(&w) else { return };
            match g.current_button() {
                1 => a.switch_to(i),
                2 => a.close_tab(i),
                _ => {}
            }
        });
        bx.add_controller(g);
        let keys = gtk::EventControllerKey::new();
        let a = Rc::downgrade(self);
        let w = bx.downgrade();
        keys.connect_key_pressed(move |_, key, _, _| {
            let (Some(a), Some(w)) = (a.upgrade(), w.upgrade()) else { return glib::Propagation::Proceed };
            let Some(i) = a.tab_index(&w) else { return glib::Propagation::Proceed };
            a.tab_key(i, key)
        });
        bx.add_controller(keys);
        self.attach_tab_drag(&bx);
        crate::dnd::attach_tab(self, &bx);
        let a = Rc::downgrade(self);
        let w = bx.downgrade();
        close.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            let Some(w) = w.upgrade() else { return };
            if let Some(i) = a.tab_index(&w) {
                a.close_tab(i);
            }
        });

        // New tabs go just before the + button
        bx.insert_before(&self.tab_strip, Some(&self.plus));
        self.save_view();
        let idx = {
            let mut tabs = self.tabs.borrow_mut();
            let ClosedTab { loc, view, back, fwd } = closed;
            tabs.push(Tab { loc, view, back, fwd, widget: bx, label, icon, close });
            tabs.len() - 1
        };
        self.show_tab(idx);
        let adj = self.tab_scroll.hadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper()));
    }

    /// Keys on a focused tab; returns Stop for the ones it handles.
    fn tab_key(self: &Rc<Self>, i: usize, key: gdk::Key) -> glib::Propagation {
        let nav = match key {
            gdk::Key::Left | gdk::Key::KP_Left => Some(TabNav::Left),
            gdk::Key::Right | gdk::Key::KP_Right => Some(TabNav::Right),
            gdk::Key::Home | gdk::Key::KP_Home => Some(TabNav::Home),
            gdk::Key::End | gdk::Key::KP_End => Some(TabNav::End),
            _ => None,
        };
        if let Some(nav) = nav {
            let n = self.tabs.borrow().len();
            let to = tab_focus_target(n, i, nav);
            if to >= n {
                self.plus.grab_focus();
            } else if let Some(t) = self.tabs.borrow().get(to) {
                t.widget.grab_focus();
            }
            return glib::Propagation::Stop;
        }
        match key {
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space => self.switch_to(i),
            gdk::Key::Delete | gdk::Key::KP_Delete => self.close_tab(i),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    fn tab_index(&self, w: &gtk::Box) -> Option<usize> {
        self.tabs.borrow().iter().position(|t| &t.widget == w)
    }

    /// The place shown by the tab widget `w`.
    pub(crate) fn tab_loc_of(&self, w: &gtk::Box) -> Option<Loc> {
        self.tabs.borrow().iter().find(|t| &t.widget == w).map(|t| t.loc.clone())
    }

    /// Whether `w` is the tab widget currently shown.
    pub(crate) fn is_current_tab(&self, w: &gtk::Box) -> bool {
        self.tab_index(w) == Some(self.current.get())
    }

    /// Switches to the tab widget `w` (used while hovering it during a file drag).
    pub(crate) fn switch_to_tab(self: &Rc<Self>, w: &gtk::Box) {
        if let Some(i) = self.tab_index(w) {
            self.switch_to(i);
        }
    }

    /// Titles every tab, adding the parent folder's name where two tabs would look alike.
    pub(super) fn refresh_tab_titles(&self) {
        let tabs = self.tabs.borrow();
        let items: Vec<(String, Option<String>)> = tabs
            .iter()
            .map(|t| {
                let parent = match &t.loc {
                    Loc::Dir(f) => f.parent().map(|p| file_title(&p)),
                    _ => None,
                };
                (t.loc.title(), parent)
            })
            .collect();
        for (t, title) in tabs.iter().zip(tab_titles(&items)) {
            set_tab_title(&t.widget, &t.label, &title);
        }
    }

    /// Drag a tab to move it along the strip.
    fn attach_tab_drag(self: &Rc<Self>, bx: &gtk::Box) {
        let src = gtk::DragSource::new();
        src.set_actions(gdk::DragAction::MOVE);
        // A number type, so the sidebar's text drop targets and file targets never see it
        src.connect_prepare(|_, _, _| Some(gdk::ContentProvider::for_value(&0u32.to_value())));
        let w = bx.downgrade();
        src.connect_drag_begin(move |s, _| {
            if let Some(w) = w.upgrade() {
                s.set_icon(Some(&gtk::WidgetPaintable::new(Some(&w))), 20, 16);
                TAB_DRAG.with(|d| *d.borrow_mut() = Some(w.downgrade()));
            }
        });
        src.connect_drag_end(|_, _, _| TAB_DRAG.with(|d| *d.borrow_mut() = None));
        bx.add_controller(src);

        let drop = gtk::DropTarget::new(u32::static_type(), gdk::DragAction::MOVE);
        // The tab being dragged, if it belongs to this window and isn't the one under the pointer
        let dragged = |a: &App, over: &gtk::Box| {
            let src = TAB_DRAG.with(|d| d.borrow().as_ref().and_then(|w| w.upgrade()))?;
            let (from, to) = (a.tab_index(&src)?, a.tab_index(over)?);
            (from != to).then_some((from, to))
        };
        let (a, w) = (Rc::downgrade(self), bx.downgrade());
        drop.connect_motion(move |_, x, _| {
            let (Some(a), Some(w)) = (a.upgrade(), w.upgrade()) else { return gdk::DragAction::empty() };
            if dragged(&a, &w).is_none() {
                return gdk::DragAction::empty();
            }
            let before = x < w.width() as f64 / 2.0;
            w.remove_css_class(if before { "drop-after" } else { "drop-before" });
            w.add_css_class(if before { "drop-before" } else { "drop-after" });
            gdk::DragAction::MOVE
        });
        let w = bx.downgrade();
        drop.connect_leave(move |_| {
            if let Some(w) = w.upgrade() {
                w.remove_css_class("drop-before");
                w.remove_css_class("drop-after");
            }
        });
        let (a, w) = (Rc::downgrade(self), bx.downgrade());
        drop.connect_drop(move |_, _, x, _| {
            let (Some(a), Some(w)) = (a.upgrade(), w.upgrade()) else { return false };
            w.remove_css_class("drop-before");
            w.remove_css_class("drop-after");
            let Some((from, over)) = dragged(&a, &w) else { return false };
            let before = x < w.width() as f64 / 2.0;
            // Move after the drop finishes, not while GTK is still using these widgets
            glib::idle_add_local_once(move || a.move_tab(from, over, before));
            true
        });
        bx.add_controller(drop);
    }

    fn move_tab(self: &Rc<Self>, from: usize, over: usize, before: bool) {
        let Some(to) = tab_move_to(self.tabs.borrow().len(), from, over, before) else { return };
        {
            let mut tabs = self.tabs.borrow_mut();
            let t = tabs.remove(from);
            tabs.insert(to, t);
            let after = to.checked_sub(1).map(|i| tabs[i].widget.clone());
            self.tab_strip.reorder_child_after(&tabs[to].widget, after.as_ref());
        }
        self.current.set(current_after_move(self.current.get(), from, to));
    }

    fn switch_to(self: &Rc<Self>, i: usize) {
        if i >= self.tabs.borrow().len() {
            return;
        }
        self.save_view();
        self.show_tab(i);
    }

    /// Makes tab `i` current and loads it, without saving the view being left.
    fn show_tab(self: &Rc<Self>, i: usize) {
        {
            let tabs = self.tabs.borrow();
            if i >= tabs.len() {
                return;
            }
            let many = tabs.len() > 1;
            for (j, t) in tabs.iter().enumerate() {
                if j == i {
                    t.widget.add_css_class("active");
                } else {
                    t.widget.remove_css_class("active");
                }
                t.widget.update_state(&[gtk::accessible::State::Selected(Some(j == i))]);
                t.close.set_visible(many);
            }
        }
        self.current.set(i);
        self.load_current();
    }

    fn close_tab(self: &Rc<Self>, i: usize) {
        // Before the indexes shift; harmless when the closed tab is the current one
        self.save_view();
        let (new_cur, had_focus) = {
            let mut tabs = self.tabs.borrow_mut();
            let n = tabs.len();
            if n <= 1 || i >= n {
                return;
            }
            let t = tabs.remove(i);
            let had_focus = t.widget.has_focus();
            self.tab_strip.remove(&t.widget);
            self.closed_tabs.borrow_mut().push(ClosedTab { loc: t.loc, view: t.view, back: t.back, fwd: t.fwd });
            (current_after_close(n, self.current.get(), i), had_focus)
        };
        self.show_tab(new_cur);
        // Keep keyboard focus in the strip, on the tab that became current
        if had_focus {
            if let Some(t) = self.tabs.borrow().get(new_cur) {
                t.widget.grab_focus();
            }
        }
    }

    pub(super) fn cycle_tab(self: &Rc<Self>, delta: i32) {
        let n = self.tabs.borrow().len() as i32;
        let i = (self.current.get() as i32 + delta).rem_euclid(n);
        self.switch_to(i as usize);
    }
}
