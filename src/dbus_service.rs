//! The freedesktop `org.freedesktop.FileManager1` D-Bus interface, which
//! browsers, editors and the desktop portal use for "Show in folder".
//!
//! All RFiles windows share one process. That process owns the bus name
//! while it runs and opens a window in-process for each request, so when
//! RFiles is already open, "Show in folder" costs no process start at all.
//! When it isn't running, D-Bus starts `rfiles --dbus-service`, which
//! becomes that process (see `main.rs`).

use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;

pub const BUS_NAME: &str = "org.freedesktop.FileManager1";
const OBJECT_PATH: &str = "/org/freedesktop/FileManager1";

const XML: &str = r#"<node>
  <interface name="org.freedesktop.FileManager1">
    <method name="ShowFolders">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItems">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItemProperties">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
  </interface>
</node>"#;

/// What to open for one request: one window per folder, at most.
/// `ShowFolders` opens the folders themselves; `ShowItems` (and
/// `ShowItemProperties`) open each item, which RFiles shows by opening its
/// folder with the item selected. Several items in the same folder share
/// one window (the first one is selected).
fn targets(method: &str, uris: &[String]) -> Vec<String> {
    let items: Vec<(String, bool)> = uris
        .iter()
        .filter(|u| !u.is_empty())
        .map(|u| (u.clone(), method == "ShowFolders"))
        .collect();
    distinct_windows(&items).into_iter().map(|i| items[i].0.clone()).collect()
}

/// Which of `items` (uri, is it opened as a folder) get a window: the indexes
/// of the first item for each distinct folder, in order. A folder is its own
/// window; a file belongs to its parent folder, so files in one folder
/// share a window (the first one is selected).
pub fn distinct_windows(items: &[(String, bool)]) -> Vec<usize> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (i, (uri, is_folder)) in items.iter().enumerate() {
        let trimmed = uri.trim_end_matches('/');
        let key = if *is_folder {
            trimmed.to_string()
        } else {
            match trimmed.rsplit_once('/') {
                Some((parent, _)) => parent.to_string(),
                None => uri.clone(),
            }
        };
        if seen.insert(key) {
            out.push(i);
        }
    }
    out
}

#[cfg(test)]
mod distinct_windows_tests {
    use super::distinct_windows;

    fn it(a: &[(&str, bool)]) -> Vec<(String, bool)> {
        a.iter().map(|(s, f)| (s.to_string(), *f)).collect()
    }

    #[test]
    fn keeps_order_and_shares_folder_windows() {
        let items = it(&[("file:///b/z", true), ("file:///a/x.txt", false), ("file:///a/y.txt", false)]);
        assert_eq!(distinct_windows(&items), vec![0, 1]);
    }

    #[test]
    fn folder_and_file_inside_it_are_separate() {
        // The file opens /a (selected); the folder /a/sub is its own window
        let items = it(&[("file:///a/x.txt", false), ("file:///a/sub", true), ("file:///a", true)]);
        assert_eq!(distinct_windows(&items), vec![0, 1]);
    }

    #[test]
    fn duplicates_and_trailing_slashes() {
        let items = it(&[("file:///a", true), ("file:///a/", true), ("file:///b/f/", false), ("file:///b/g", false)]);
        assert_eq!(distinct_windows(&items), vec![0, 2]);
    }

    #[test]
    fn empty_input() {
        assert!(distinct_windows(&[]).is_empty());
    }
}

thread_local! {
    /// The running app, once GTK is set up and windows can be opened.
    static APP: RefCell<Option<glib::WeakRef<gtk::Application>>> = const { RefCell::new(None) };
    /// Requests that arrived before that: (uri, startup id).
    static QUEUE: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
    /// Loop to stop when the first request arrives (service start-up).
    static WAITING: RefCell<Option<glib::MainLoop>> = const { RefCell::new(None) };
    static OWNED: Cell<bool> = const { Cell::new(false) };
}

fn dispatch(uri: String, startup_id: String) {
    match APP.with(|a| a.borrow().as_ref().and_then(|w| w.upgrade())) {
        Some(app) => crate::open_location(&app, &gio::File::for_uri(&uri), Some(&startup_id)),
        None => {
            QUEUE.with(|q| q.borrow_mut().push((uri, startup_id)));
            if let Some(ml) = WAITING.with(|w| w.borrow_mut().take()) {
                ml.quit();
            }
        }
    }
}

/// Claims the bus name (once per process). Uses only GIO, never GTK, so it
/// can run before GTK is set up: GTK's start-up talks to the desktop
/// portal, and the portal may be the one waiting for our reply.
pub fn own_name() {
    if OWNED.replace(true) {
        return;
    }
    let node = match gio::DBusNodeInfo::for_xml(XML) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("rfiles: bad D-Bus interface: {e}");
            return;
        }
    };
    let Some(iface) = node.lookup_interface(BUS_NAME) else { return };
    // The owner id lives as long as the process; never unowned explicitly.
    let _ = gio::bus_own_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameOwnerFlags::NONE,
        move |conn, _| {
            let reg = conn
                .register_object(OBJECT_PATH, &iface)
                .method_call(|_, _, _, _, method, params, invocation| {
                    let Some((uris, startup_id)) = params.get::<(Vec<String>, String)>() else {
                        invocation.return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "expected (as, s)");
                        return;
                    };
                    // Reply first: the caller (often the portal) must not
                    // wait for windows, or it can't answer GTK's start-up.
                    invocation.return_value(None);
                    for t in targets(method, &uris) {
                        dispatch(t, startup_id.clone());
                    }
                })
                .build();
            if let Err(e) = reg {
                eprintln!("rfiles: couldn't register {OBJECT_PATH}: {e}");
            }
        },
        |_, _| {},
        |_, _| {
            // Someone else owns it; stop waiting for requests
            if let Some(ml) = WAITING.with(|w| w.borrow_mut().take()) {
                ml.quit();
            }
        },
    );
}

/// Service start-up: claims the name and waits (without GTK) until the
/// request that started us has arrived and been answered.
pub fn wait_for_first_request(timeout: std::time::Duration) {
    own_name();
    if QUEUE.with(|q| !q.borrow().is_empty()) {
        return;
    }
    let ml = glib::MainLoop::new(None, false);
    WAITING.with(|w| *w.borrow_mut() = Some(ml.clone()));
    glib::timeout_add_local_once(timeout, || {
        if let Some(ml) = WAITING.with(|w| w.borrow_mut().take()) {
            ml.quit();
        }
    });
    ml.run();
}

/// Called once GTK is ready: later requests open windows right away, and
/// queued ones are opened now.
pub fn attach(app: &gtk::Application) {
    APP.with(|a| *a.borrow_mut() = Some(app.downgrade()));
    for (uri, id) in QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut())) {
        crate::open_location(app, &gio::File::for_uri(&uri), Some(&id));
    }
}

/// Requests that arrived but can't be handled here (another RFiles is
/// already running): hands them to it as normal "open" requests.
pub fn forward_queued(app: &gtk::Application) {
    // One `open` per startup id, so each keeps its own id for focus
    let mut groups: Vec<(String, Vec<gio::File>)> = Vec::new();
    for (uri, id) in QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut())) {
        let file = gio::File::for_uri(&uri);
        match groups.iter_mut().find(|(g, _)| *g == id) {
            Some((_, files)) => files.push(file),
            None => groups.push((id, vec![file])),
        }
    }
    for (id, files) in groups {
        app.open(&files, &id);
    }
}

/// Keeps the process alive for `secs` without any window, so a quick
/// follow-up request (or launch) reuses it instead of starting a new one.
pub fn linger(app: &gtk::Application, secs: u32) {
    // One hold at a time, kept where `stop_lingering` can drop it early; a
    // newer linger replaces the older one, and each timer only ends its own.
    let id = LINGER.with_borrow_mut(|l| {
        l.0 += 1;
        l.1 = Some(app.hold());
        l.0
    });
    glib::timeout_add_seconds_local_once(secs, move || {
        LINGER.with_borrow_mut(|l| {
            if l.0 == id {
                l.1 = None;
            }
        })
    });
}

/// Drops the linger hold now, so a process with no windows (and no job
/// holding it) exits right away.
pub fn stop_lingering() {
    // Taken out first: dropping the guard may run GTK code that lingers again
    let guard = LINGER.with_borrow_mut(|l| l.1.take());
    drop(guard);
}

thread_local! {
    /// The current linger hold and a counter telling the timers apart.
    static LINGER: RefCell<(u64, Option<gio::ApplicationHoldGuard>)> =
        const { RefCell::new((0, None)) };
}

#[cfg(test)]
mod tests {
    use super::targets;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_window_per_folder() {
        // Two files in one folder share a window; a third folder gets its own
        let items = v(&["file:///a/x.txt", "file:///a/y.txt", "file:///b/z.txt", ""]);
        assert_eq!(targets("ShowItems", &items), v(&["file:///a/x.txt", "file:///b/z.txt"]));
        // Folders are opened themselves, duplicates once
        let dirs = v(&["file:///a", "file:///a/", "file:///b"]);
        assert_eq!(targets("ShowFolders", &dirs), v(&["file:///a", "file:///b"]));
    }
}
