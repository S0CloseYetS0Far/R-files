use super::*;

impl App {
    pub(super) fn activate(&self, name: &str) {
        gtk::prelude::ActionGroupExt::activate_action(&self.window, name, None);
    }

    // ================= opening =================
    pub(super) fn open_position(self: &Rc<Self>, pos: u32) {
        // Enter with several items selected opens all of them, not just the focused one
        if self.selection.is_selected(pos) && self.selection.selection().size() > 1 {
            self.open_selected();
            return;
        }
        if let Some(info) = self.selection.item(pos).and_downcast::<gio::FileInfo>() {
            self.open_info(&info);
        }
    }

    /// Opens every selected item: files in their default app, the first folder here, other folders in new tabs.
    pub(super) fn open_selected(self: &Rc<Self>) {
        let infos = self.selected_infos();
        let plan = plan_open(&infos.iter().map(is_dir_like).collect::<Vec<_>>());
        for &i in &plan.files {
            self.launch(&item_file(&infos[i]), false);
        }
        if let Some(i) = plan.first_dir {
            self.navigate(Loc::Dir(item_file(&infos[i])));
        }
        for &i in &plan.other_dirs {
            self.new_tab(Loc::Dir(item_file(&infos[i])));
        }
        if plan.skipped > 0 {
            self.status.set_text(&format!("Opened the first {MAX_OPEN} of {} items", MAX_OPEN + plan.skipped));
        }
    }

    pub fn open_info(self: &Rc<Self>, info: &gio::FileInfo) {
        let file = item_file(info);
        if is_dir_like(info) {
            self.navigate(Loc::Dir(file));
        } else {
            self.launch(&file, false);
        }
    }

    pub(super) fn launch(&self, file: &gio::File, ask: bool) {
        if ask {
            crate::launch::choose(self.window.upcast_ref(), file, &self.status);
        } else {
            crate::launch::open(self.window.upcast_ref(), file, &self.status);
        }
    }

    pub(super) fn open_terminal(&self, dir: Option<gio::File>) {
        let Some(path) = dir.and_then(|d| d.path()) else {
            self.status.set_text("Terminal can only open in local folders");
            return;
        };
        if let Err(e) = fileops::open_terminal(&path) {
            self.status.set_text(&e);
        }
    }
}

/// Most items "Open" handles at once, so a stray Ctrl+A then Enter can't launch hundreds of apps.
const MAX_OPEN: usize = 20;

/// Which of several selected items go where, as indexes into the selection.
#[derive(Debug, PartialEq, Eq)]
struct OpenPlan {
    /// Default app
    files: Vec<usize>,
    /// Opens in the current tab
    first_dir: Option<usize>,
    /// Open in new tabs
    other_dirs: Vec<usize>,
    /// Selected items beyond the cap, left alone
    skipped: usize,
}

fn plan_open(is_dir: &[bool]) -> OpenPlan {
    let skipped = is_dir.len().saturating_sub(MAX_OPEN);
    let mut plan = OpenPlan { files: vec![], first_dir: None, other_dirs: vec![], skipped };
    for (i, &d) in is_dir.iter().take(MAX_OPEN).enumerate() {
        match (d, plan.first_dir) {
            (false, _) => plan.files.push(i),
            (true, None) => plan.first_dir = Some(i),
            (true, Some(_)) => plan.other_dirs.push(i),
        }
    }
    plan
}

#[cfg(test)]
mod open_plan_tests {
    use super::{plan_open, MAX_OPEN};

    #[test]
    fn files_go_to_apps_and_folders_to_tabs() {
        let p = plan_open(&[false, true, false, true, true]);
        assert_eq!(p.files, vec![0, 2]);
        assert_eq!(p.first_dir, Some(1));
        assert_eq!(p.other_dirs, vec![3, 4]);
        assert_eq!(p.skipped, 0);
    }

    #[test]
    fn only_files_leave_the_tab_alone() {
        let p = plan_open(&[false, false]);
        assert_eq!((p.files, p.first_dir, p.other_dirs), (vec![0, 1], None, vec![]));
    }

    #[test]
    fn capped_at_twenty() {
        let p = plan_open(&vec![false; MAX_OPEN + 5]);
        assert_eq!(p.files.len(), MAX_OPEN);
        assert_eq!(p.skipped, 5);
        assert_eq!(plan_open(&vec![true; MAX_OPEN]).skipped, 0);
        assert_eq!(plan_open(&[]).skipped, 0);
    }
}
