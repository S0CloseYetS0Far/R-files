use super::*;

/// Most selected names remembered per history entry, so history stays small.
const SAVED_NAMES_MAX: usize = 200;

/// The view of a place as it was when it was left.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ViewState {
    /// Selected file names, at most `SAVED_NAMES_MAX`
    names: Vec<String>,
    /// Grid and list scroll offsets; `None` scrolls to the first selected name instead
    scroll: Option<[f64; 2]>,
}

impl ViewState {
    /// Only selects `name` and scrolls to it (the folder we came from).
    fn select(name: String) -> Self {
        ViewState { names: vec![name], scroll: None }
    }
}

/// A saved view being put back after a load. Each part is cleared once applied.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Restore {
    /// Names to select once the place has loaded
    names: Option<Vec<String>>,
    /// Scroll to the first selected name rather than to saved offsets
    to_item: bool,
    /// Grid and list offsets still to apply, each once its view is laid out
    scroll: [Option<f64>; 2],
}

impl From<ViewState> for Restore {
    fn from(v: ViewState) -> Self {
        let scroll = v.scroll.map_or([None; 2], |s| s.map(Some));
        Restore { names: Some(v.names), to_item: v.scroll.is_none(), scroll }
    }
}

/// How the user got to the place being loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Nav {
    /// Typed path, sidebar, double-click: start at the top
    Fresh,
    Up,
    Back,
    Forward,
}

/// The view to show on arriving at `arriving` from `left`, given the state
/// saved for `arriving`'s history entry (if any).
pub(super) fn arrival_view(nav: Nav, saved: Option<ViewState>, left: &Loc, arriving: &Loc) -> Option<ViewState> {
    match nav {
        Nav::Fresh => None,
        Nav::Up => came_from(left, arriving).map(ViewState::select),
        Nav::Back => saved.or_else(|| came_from(left, arriving).map(ViewState::select)),
        Nav::Forward => saved,
    }
}

/// The name of the folder `left` when it sits directly in `arriving`.
fn came_from(left: &Loc, arriving: &Loc) -> Option<String> {
    let (Loc::Dir(child), Loc::Dir(parent)) = (left, arriving) else { return None };
    if !child.parent().is_some_and(|p| p.equal(parent)) {
        return None;
    }
    child.basename().map(|n| n.to_string_lossy().into_owned())
}

/// What to remember for the place being left: the live selection and
/// scroll, except for the parts of a restore that have not landed yet
/// (leaving again before a load finished must not lose the saved state).
fn saved_view(live_names: Vec<String>, live_scroll: [f64; 2], pending: Option<&Restore>) -> ViewState {
    let mut v = match pending {
        None => ViewState { names: live_names, scroll: Some(live_scroll) },
        Some(p) => ViewState {
            names: p.names.clone().unwrap_or(live_names),
            scroll: if p.to_item && p.names.is_some() {
                None
            } else {
                Some([p.scroll[0].unwrap_or(live_scroll[0]), p.scroll[1].unwrap_or(live_scroll[1])])
            },
        },
    };
    v.names.truncate(SAVED_NAMES_MAX);
    v
}

/// Positions in `items` (the shown names, in order) of the saved names that still exist.
fn positions_of(saved: &[String], items: &[String]) -> Vec<u32> {
    let want: std::collections::HashSet<&str> = saved.iter().map(String::as_str).collect();
    items.iter().enumerate().filter(|(_, n)| want.contains(n.as_str())).map(|(i, _)| i as u32).collect()
}

#[cfg(test)]
mod view_restore_tests {
    use super::*;

    fn dir(p: &str) -> Loc {
        Loc::Dir(gio::File::for_path(p))
    }

    fn state(names: &[&str], scroll: Option<[f64; 2]>) -> ViewState {
        ViewState { names: names.iter().map(|s| s.to_string()).collect(), scroll }
    }

    #[test]
    fn fresh_places_start_at_the_top() {
        let saved = Some(state(&["a"], Some([10.0, 20.0])));
        assert_eq!(arrival_view(Nav::Fresh, saved, &dir("/x/y"), &dir("/x")), None);
    }

    #[test]
    fn up_selects_the_folder_we_came_from() {
        assert_eq!(arrival_view(Nav::Up, None, &dir("/x/y"), &dir("/x")), Some(ViewState::select("y".into())));
        let search = Loc::Search {
            root: gio::File::for_path("/x"),
            query: "q".into(),
            results: Rc::new(vec![]),
            end: SearchEnd::Done,
        };
        assert_eq!(arrival_view(Nav::Up, None, &search, &dir("/x")), None);
    }

    #[test]
    fn back_restores_saved_state_or_selects_the_child() {
        let saved = state(&["a", "b"], Some([5.0, 7.0]));
        assert_eq!(arrival_view(Nav::Back, Some(saved.clone()), &dir("/x/y"), &dir("/x")), Some(saved));
        assert_eq!(arrival_view(Nav::Back, None, &dir("/x/y"), &dir("/x")), Some(ViewState::select("y".into())));
        // Back to somewhere unrelated with nothing saved: top
        assert_eq!(arrival_view(Nav::Back, None, &dir("/x/y"), &dir("/z")), None);
    }

    #[test]
    fn forward_restores_only_saved_state() {
        let saved = state(&["c"], Some([1.0, 2.0]));
        assert_eq!(arrival_view(Nav::Forward, Some(saved.clone()), &dir("/x"), &dir("/x/y")), Some(saved));
        assert_eq!(arrival_view(Nav::Forward, None, &dir("/x/y"), &dir("/x")), None);
    }

    #[test]
    fn came_from_needs_a_direct_child() {
        assert_eq!(came_from(&dir("/x/y"), &dir("/x")), Some("y".into()));
        assert_eq!(came_from(&dir("/x/y/z"), &dir("/x")), None);
        assert_eq!(came_from(&dir("/x"), &dir("/x/y")), None);
        assert_eq!(came_from(&Loc::Starred, &dir("/x")), None);
    }

    #[test]
    fn leaving_saves_the_live_view_capped() {
        let names: Vec<String> = (0..500).map(|i| format!("f{i}")).collect();
        let v = saved_view(names, [3.0, 4.0], None);
        assert_eq!(v.names.len(), SAVED_NAMES_MAX);
        assert_eq!(v.names[0], "f0");
        assert_eq!(v.scroll, Some([3.0, 4.0]));
    }

    #[test]
    fn leaving_before_the_restore_landed_keeps_the_saved_parts() {
        // Nothing applied yet: the saved state survives as it was
        let p = Restore::from(state(&["a"], Some([50.0, 60.0])));
        assert_eq!(saved_view(vec![], [0.0, 0.0], Some(&p)), state(&["a"], Some([50.0, 60.0])));
        // Selection and grid applied, the hidden list view's offset still waits
        let p = Restore { names: None, to_item: false, scroll: [None, Some(60.0)] };
        assert_eq!(saved_view(vec!["b".into()], [55.0, 0.0], Some(&p)), state(&["b"], Some([55.0, 60.0])));
        // A "select the child" restore not yet applied stays one
        let p = Restore::from(ViewState::select("y".into()));
        assert_eq!(saved_view(vec![], [0.0, 0.0], Some(&p)), ViewState::select("y".into()));
    }

    #[test]
    fn restore_selects_only_names_that_still_exist() {
        let saved = vec!["b".to_string(), "gone".into(), "d".into()];
        let items: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(positions_of(&saved, &items), vec![1, 3]);
        assert!(positions_of(&[], &items).is_empty());
        assert!(positions_of(&saved, &[]).is_empty());
    }
}

impl App {
    /// The current view as it should be remembered when leaving it.
    pub(super) fn capture_view(&self) -> ViewState {
        let names = self
            .selected_infos()
            .iter()
            .take(SAVED_NAMES_MAX)
            .map(|i| i.name().to_string_lossy().into_owned())
            .collect();
        let scroll = [self.grid_scroll.vadjustment().value(), self.list_scroll.vadjustment().value()];
        saved_view(names, scroll, self.restore.borrow().as_ref())
    }

    /// Remembers the current view on the current tab, for a tab switch or reload.
    pub(super) fn save_view(&self) {
        let v = self.capture_view();
        if let Some(t) = self.tabs.borrow_mut().get_mut(self.current.get()) {
            t.view = Some(v);
        }
    }

    pub(super) fn drop_restore(&self) {
        self.restore_gen.set(self.restore_gen.get() + 1);
        *self.restore.borrow_mut() = None;
    }

    /// Puts back the selection of a saved view once the place has loaded,
    /// then its scroll offsets once the views are laid out.
    pub(super) fn apply_restore(self: &Rc<Self>) {
        if self.pending_select.borrow().is_some() {
            // "Show in folder" wins
            self.drop_restore();
            return;
        }
        let (names, to_item) = {
            let mut r = self.restore.borrow_mut();
            let Some(r) = r.as_mut() else { return };
            let Some(names) = r.names.take() else { return };
            (names, r.to_item)
        };
        let n = self.selection.n_items();
        if !names.is_empty() {
            let items: Vec<String> = (0..n)
                .map(|i| {
                    self.selection
                        .item(i)
                        .and_downcast::<gio::FileInfo>()
                        .map(|f| f.name().to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            let pos = positions_of(&names, &items);
            if !pos.is_empty() {
                let sel = gtk::Bitset::new_empty();
                for &p in &pos {
                    sel.add(p);
                }
                // One change for the whole selection, not one per name
                self.selection.set_selection(&sel, &gtk::Bitset::new_range(0, n));
                if to_item {
                    let (none, focus) = (gtk::ListScrollFlags::NONE, gtk::ListScrollFlags::FOCUS);
                    let list = self.view_btn.is_active();
                    let (grid_flags, list_flags) = if list { (none, focus) } else { (focus, none) };
                    self.scroll_views(pos[0], grid_flags, list_flags);
                }
            }
        }
        // The shown view now; the other when it is next shown (see setup_view_restore)
        for (i, scroll) in [&self.grid_scroll, &self.list_scroll].into_iter().enumerate() {
            if scroll.is_mapped() {
                self.restore_scroll_when_laid_out(i);
            }
        }
        self.finish_restore_if_done();
    }

    fn finish_restore_if_done(&self) {
        let done = self.restore.borrow().as_ref().is_some_and(|r| r.names.is_none() && r.scroll == [None, None]);
        if done {
            self.drop_restore();
        }
    }

    /// Sets view `i`'s saved offset after a frame has laid it out with the
    /// new contents (a value set before that is clamped to the old size or reset).
    fn restore_scroll_when_laid_out(self: &Rc<Self>, i: usize) {
        if self.restore.borrow().as_ref().and_then(|r| r.scroll[i]).is_none() {
            return;
        }
        let scroll = if i == 0 { &self.grid_scroll } else { &self.list_scroll };
        let gen = self.restore_gen.get();
        let frames = Cell::new(0u32);
        let a = Rc::downgrade(self);
        scroll.add_tick_callback(move |s, _| {
            let Some(a) = a.upgrade() else { return glib::ControlFlow::Break };
            if a.restore_gen.get() != gen {
                return glib::ControlFlow::Break;
            }
            let Some(value) = a.restore.borrow().as_ref().and_then(|r| r.scroll[i]) else {
                return glib::ControlFlow::Break;
            };
            frames.set(frames.get() + 1);
            let adj = s.vadjustment();
            // The first frame's layout runs after this callback; give up
            // waiting for the full height after a few (the folder shrank)
            if frames.get() < 2 || (adj.upper() - adj.page_size() < value && frames.get() < 10) {
                return glib::ControlFlow::Continue;
            }
            adj.set_value(value);
            if let Some(r) = a.restore.borrow_mut().as_mut() {
                r.scroll[i] = None;
            }
            a.finish_restore_if_done();
            glib::ControlFlow::Break
        });
    }

    /// Drops a restore the user has overtaken by scrolling, clicking or
    /// typing in a view, and applies the hidden view's offset when it is shown.
    pub(super) fn setup_view_restore(self: &Rc<Self>) {
        for (i, scroll) in [&self.grid_scroll, &self.list_scroll].into_iter().enumerate() {
            let c = gtk::EventControllerLegacy::new();
            c.set_propagation_phase(gtk::PropagationPhase::Capture);
            let a = Rc::downgrade(self);
            c.connect_event(move |_, ev| {
                use gdk::EventType as E;
                if matches!(ev.event_type(), E::ButtonPress | E::Scroll | E::KeyPress | E::TouchBegin) {
                    if let Some(a) = a.upgrade() {
                        // The other (hidden) view keeps its offset: Ctrl+1/2 must still restore it
                        if let Some(r) = a.restore.borrow_mut().as_mut() {
                            r.names = None;
                            r.scroll[i] = None;
                        }
                        a.finish_restore_if_done();
                    }
                }
                glib::Propagation::Proceed
            });
            scroll.add_controller(c);

            let a = Rc::downgrade(self);
            scroll.connect_map(move |_| {
                let Some(a) = a.upgrade() else { return };
                // Only once the selection is back, i.e. the place has loaded
                if a.restore.borrow().as_ref().is_some_and(|r| r.names.is_none()) {
                    a.restore_scroll_when_laid_out(i);
                }
            });
        }
    }
}
