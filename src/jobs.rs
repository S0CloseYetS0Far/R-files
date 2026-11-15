//! Running file jobs (copy, move, trash, delete, restore, compress, extract, undo). Each one takes a `Guard`,
//! which holds the application, so closing the last window, the linger timeout or "quit-if-idle" never cuts it
//! off, and lists the job here so Quit can say what is still running and stop what can be stopped. A job whose
//! window is gone by the time it ends hands its result to `deliver`.

use crate::app::App;
use crate::banner::Notice;
use gtk::gio;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

struct Entry {
    id: u64,
    what: String,
    cancel: Option<Rc<dyn Fn()>>,
}

thread_local! {
    /// The last id given out and the running jobs, oldest first.
    static RUNNING: RefCell<(u64, Vec<Entry>)> = const { RefCell::new((0, Vec::new())) };
    /// The Quit question is open, so a second Ctrl+Q doesn't ask again.
    static ASKING: Cell<bool> = const { Cell::new(false) };
}

/// A running job: drop it when the job has ended (its result included).
pub struct Guard {
    id: u64,
    // Dropped after `drop` below has removed the entry; may end the process
    _hold: Option<gio::ApplicationHoldGuard>,
}

/// Registers a job, `what` being one line about it ("Copying 3 items…"); `cancel` asks it to stop early,
/// for jobs that can.
pub fn start(what: &str, cancel: Option<Rc<dyn Fn()>>) -> Guard {
    let id = RUNNING.with_borrow_mut(|r| {
        r.0 += 1;
        r.1.push(Entry { id: r.0, what: what.trim_end_matches('…').to_string(), cancel });
        r.0
    });
    Guard { id, _hold: gio::Application::default().map(|a| a.hold()) }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Taken out first, so the cancel closure is dropped outside the borrow
        let gone = RUNNING.with_borrow_mut(|r| r.1.iter().position(|e| e.id == self.id).map(|i| r.1.remove(i)));
        drop(gone);
    }
}

/// Each running job's line and whether it can be stopped, oldest first.
pub fn running() -> Vec<(String, bool)> {
    RUNNING.with_borrow(|r| r.1.iter().map(|e| (e.what.clone(), e.cancel.is_some())).collect())
}

/// Asks every job that can stop early to stop.
fn cancel_all() {
    let cancels: Vec<Rc<dyn Fn()>> = RUNNING.with_borrow(|r| r.1.iter().filter_map(|e| e.cancel.clone()).collect());
    for c in cancels {
        c();
    }
}

#[derive(Debug, PartialEq)]
enum Quit {
    /// Nothing is running: quit as always
    Now,
    /// Ask first; `waits_for` are the jobs "Stop now" can't stop
    Ask { count: usize, waits_for: Vec<String> },
}

fn quit_plan(jobs: &[(String, bool)]) -> Quit {
    if jobs.is_empty() {
        return Quit::Now;
    }
    let waits_for = jobs.iter().filter(|(_, can)| !can).map(|(w, _)| w.clone()).collect();
    Quit::Ask { count: jobs.len(), waits_for }
}

fn close_all(g: &gtk::Application) {
    // close(), not quit() alone: only close-request saves each window's layout
    for w in g.windows() {
        w.close();
    }
}

/// Quit (Ctrl+Q). With jobs running, asks whether to let them finish or stop them, and never ends the process
/// under them: the windows close and the process exits by itself once the last job is done.
pub fn quit(g: &gtk::Application, parent: &gtk::ApplicationWindow) {
    let Quit::Ask { count, waits_for } = quit_plan(&running()) else {
        close_all(g);
        g.quit();
        return;
    };
    if ASKING.replace(true) {
        return;
    }
    let detail = if waits_for.is_empty() {
        String::new()
    } else {
        format!("These can't be stopped and will finish first: {}.", waits_for.join(", "))
    };
    // Keep running is the default, so Enter never stops anything; Escape is Cancel
    let dlg = gtk::AlertDialog::builder()
        .message(format!("{count} operation(s) are still running. Quit when they finish, or stop them now?"))
        .detail(detail)
        .buttons(["Cancel", "Stop now", "Keep running"])
        .cancel_button(0)
        .default_button(2)
        .build();
    let g = g.downgrade();
    dlg.choose(Some(parent), None::<&gio::Cancellable>, move |r| {
        ASKING.set(false);
        let Some(g) = g.upgrade() else { return };
        match r {
            Ok(1) => cancel_all(),
            Ok(2) => {}
            _ => return,
        }
        close_all(&g);
        // No 30 s linger after Quit: the jobs' holds alone keep the process until they end
        crate::dbus_service::stop_lingering();
    });
}

#[derive(Debug, PartialEq)]
enum Target {
    /// The window the job was started from
    Own,
    /// Another RFiles window's banner
    Other,
    /// No window is open: a desktop notification
    Desktop,
}

fn target(own_open: bool, other_open: bool) -> Target {
    match (own_open, other_open) {
        (true, _) => Target::Own,
        (false, true) => Target::Other,
        (false, false) => Target::Desktop,
    }
}

/// Shows a job's result in `own`'s banner; if that window has closed meanwhile, in another RFiles window's
/// banner, or as a desktop notification when none is open, so the result isn't lost.
pub(crate) fn deliver(own: &Weak<App>, n: Notice) {
    let open = crate::app::open_windows();
    let own = own.upgrade().filter(|a| open.iter().any(|o| Rc::ptr_eq(o, a)));
    let other = open.iter().find(|a| a.window.is_active()).or(open.first());
    match (target(own.is_some(), other.is_some()), own.as_ref(), other) {
        (Target::Own, Some(a), _) | (Target::Other, _, Some(a)) => a.notify(n),
        _ => {
            let Some(g) = gio::Application::default() else { return };
            let note = gio::Notification::new("RFiles");
            note.set_body(Some(&n.text));
            g.send_notification(None, &note);
        }
    }
}

#[cfg(test)]
mod jobs_tests {
    use super::*;
    use std::sync::Mutex;

    fn count() -> usize {
        running().len()
    }

    // One at a time: the linger test's application is the process default meanwhile, which `start` holds
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn registry_counts_and_forgets_on_drop() {
        let _s = serial();
        let before = count();
        let a = start("Copying 3 items…", Some(Rc::new(|| {})));
        let b = start("Moving 2 item(s) to the trash…", None);
        assert_eq!(count(), before + 2);
        let lines = running();
        let n = lines.len();
        assert_eq!(lines[n - 2], ("Copying 3 items".to_string(), true));
        assert_eq!(lines[n - 1], ("Moving 2 item(s) to the trash".to_string(), false));
        drop(a);
        assert_eq!(count(), before + 1);
        assert_eq!(running().last().unwrap().0, "Moving 2 item(s) to the trash");
        drop(b);
        assert_eq!(count(), before);
    }

    #[test]
    fn cancel_reaches_only_cancellable_jobs() {
        let _s = serial();
        let hit = Rc::new(Cell::new(0));
        let h = hit.clone();
        let a = start("Copying 1 item…", Some(Rc::new(move || h.set(h.get() + 1))));
        let b = start("Undoing rename…", None);
        cancel_all();
        assert_eq!(hit.get(), 1);
        drop((a, b));
    }

    #[test]
    fn quit_decisions() {
        let _s = serial();
        assert_eq!(quit_plan(&[]), Quit::Now);
        let s = |w: &str, c| (w.to_string(), c);
        assert_eq!(quit_plan(&[s("Copying 3 items", true)]), Quit::Ask { count: 1, waits_for: vec![] });
        assert_eq!(
            quit_plan(&[s("Copying 3 items", true), s("Emptying the trash", false), s("Undoing rename", false)]),
            Quit::Ask { count: 3, waits_for: vec!["Emptying the trash".into(), "Undoing rename".into()] }
        );
    }

    #[test]
    fn result_goes_to_its_window_then_another_then_the_desktop() {
        let _s = serial();
        assert_eq!(target(true, true), Target::Own);
        assert_eq!(target(true, false), Target::Own);
        assert_eq!(target(false, true), Target::Other);
        assert_eq!(target(false, false), Target::Desktop);
    }

    /// The linger timeout and "quit-if-idle" only drop their own hold; a running job's hold keeps the
    /// process until the job ends.
    #[test]
    fn dropping_the_linger_hold_waits_for_jobs() {
        let _s = serial();
        use gtk::glib;
        use std::time::{Duration, Instant};
        let app = gio::Application::new(None, gio::ApplicationFlags::NON_UNIQUE);
        let job_ended = Rc::new(Cell::new(None::<Instant>));
        let give_up = Rc::new(RefCell::new(None::<glib::SourceId>));
        let (e, gu) = (job_ended.clone(), give_up.clone());
        app.connect_activate(move |a| {
            let linger = Some(a.hold());
            let job = RefCell::new(Some(start("Copying 1 item", None)));
            // Take the job's hold from this app even if another one is the default
            let job_hold = RefCell::new(Some(a.hold()));
            glib::timeout_add_local_once(Duration::from_millis(20), move || drop(linger));
            let e = e.clone();
            glib::timeout_add_local_once(Duration::from_millis(300), move || {
                e.set(Some(Instant::now()));
                job.borrow_mut().take();
                job_hold.borrow_mut().take();
            });
            // Can't hang: gives up after 5 s
            let a = a.clone();
            *gu.borrow_mut() = Some(glib::timeout_add_local_once(Duration::from_secs(5), move || a.quit()));
        });
        let start_at = Instant::now();
        app.run_with_args::<&str>(&[]);
        // Removed so nothing keeps the application (the process default) after the test
        if let Some(id) = give_up.borrow_mut().take() {
            id.remove();
        }
        let ended = job_ended.get().expect("the process ended before the job");
        assert!(ended.duration_since(start_at) >= Duration::from_millis(300));
        assert!(start_at.elapsed() < Duration::from_secs(4));
    }
}
