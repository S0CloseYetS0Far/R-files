//! `rfiles --dbus-service`: answers the freedesktop `org.freedesktop.FileManager1`
//! D-Bus interface, which browsers, editors and the desktop portal use for
//! "Show in folder". Each request opens a normal RFiles window (a separate
//! process, like any other launch). The service itself starts on demand via
//! D-Bus activation, never touches GTK, and exits after a short idle period,
//! so it costs no memory while unused.

use gtk::{gio, glib};
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub const BUS_NAME: &str = "org.freedesktop.FileManager1";
const OBJECT_PATH: &str = "/org/freedesktop/FileManager1";
const IDLE_EXIT: Duration = Duration::from_secs(30);

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
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for uri in uris.iter().filter(|u| !u.is_empty()) {
        let key = if method == "ShowFolders" {
            uri.trim_end_matches('/').to_string()
        } else {
            match uri.trim_end_matches('/').rsplit_once('/') {
                Some((parent, _)) => parent.to_string(),
                None => uri.clone(),
            }
        };
        if seen.insert(key) {
            out.push(uri.clone());
        }
    }
    out
}

fn launch(uri: &str, startup_id: &str) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(uri);
    if !startup_id.is_empty() {
        // Lets the new window come to the front (Wayland / X11)
        cmd.env("XDG_ACTIVATION_TOKEN", startup_id);
        cmd.env("DESKTOP_STARTUP_ID", startup_id);
    }
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

pub fn run() -> glib::ExitCode {
    let main_loop = glib::MainLoop::new(None, false);
    let last_call = Rc::new(Cell::new(Instant::now()));

    let node = match gio::DBusNodeInfo::for_xml(XML) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("rfiles: bad D-Bus interface: {e}");
            return glib::ExitCode::FAILURE;
        }
    };
    let Some(iface) = node.lookup_interface(BUS_NAME) else {
        return glib::ExitCode::FAILURE;
    };

    let lc = last_call.clone();
    let ml = main_loop.clone();
    let _owner = gio::bus_own_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameOwnerFlags::NONE,
        move |conn, _| {
            let lc = lc.clone();
            let reg = conn
                .register_object(OBJECT_PATH, &iface)
                .method_call(move |_, _, _, _, method, params, invocation| {
                    lc.set(Instant::now());
                    match params.get::<(Vec<String>, String)>() {
                        Some((uris, startup_id)) => {
                            for t in targets(method, &uris) {
                                launch(&t, &startup_id);
                            }
                            invocation.return_value(None);
                        }
                        None => invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.InvalidArgs",
                            "expected (as, s)",
                        ),
                    }
                })
                .build();
            if let Err(e) = reg {
                eprintln!("rfiles: couldn't register {OBJECT_PATH}: {e}");
            }
        },
        |_, _| {},
        // Another file manager already owns the name, or we lost the bus
        move |_, _| ml.quit(),
    );

    let ml = main_loop.clone();
    glib::timeout_add_seconds_local(5, move || {
        if last_call.get().elapsed() >= IDLE_EXIT {
            ml.quit();
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });

    main_loop.run();
    glib::ExitCode::SUCCESS
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
