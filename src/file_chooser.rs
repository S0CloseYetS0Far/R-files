//! RFiles as the desktop's Save / Open dialog.
//!
//! Apps (Brave, Firefox, anything sandboxed or using GTK's portal file
//! chooser) ask xdg-desktop-portal for a file dialog; the portal hands the
//! request to a backend, `org.freedesktop.impl.portal.FileChooser`. On
//! GNOME that backend is GNOME Files. `install.sh set-default` registers
//! this one instead: D-Bus starts `rfiles --file-chooser`, which answers each
//! request with an RFiles window in picker mode (`app/picker.rs`).
//! It is its own process, separate from the RFiles windows, and exits 30
//! seconds after its last dialog closes.

use crate::app::{self, PickKind};
use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const BUS_NAME: &str = "io.github.rfiles.FileChooser";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const LINGER_SECS: u32 = 30;

const XML: &str = r#"<node>
  <interface name="org.freedesktop.impl.portal.FileChooser">
    <method name="OpenFile">
      <arg type="o" name="handle" direction="in"/><arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="parent_window" direction="in"/><arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="u" name="response" direction="out"/><arg type="a{sv}" name="results" direction="out"/>
    </method>
    <method name="SaveFile">
      <arg type="o" name="handle" direction="in"/><arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="parent_window" direction="in"/><arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="u" name="response" direction="out"/><arg type="a{sv}" name="results" direction="out"/>
    </method>
    <method name="SaveFiles">
      <arg type="o" name="handle" direction="in"/><arg type="s" name="app_id" direction="in"/>
      <arg type="s" name="parent_window" direction="in"/><arg type="s" name="title" direction="in"/>
      <arg type="a{sv}" name="options" direction="in"/>
      <arg type="u" name="response" direction="out"/><arg type="a{sv}" name="results" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.impl.portal.Request">
    <method name="Close"/>
  </interface>
</node>"#;

/// Portal response codes
const OK: u32 = 0;
const CANCELLED: u32 = 1;

/// What one request asks for, read from its method and options.
#[derive(Debug, PartialEq)]
struct Ask {
    open: Option<(bool, bool)>,
    save_name: String,
    /// SaveFiles: the names to put in the chosen folder
    save_files: Vec<String>,
    folder: Option<String>,
    accept: String,
}

/// A path option (`ay`, NUL-terminated).
fn bytes_path(b: Vec<u8>) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn read_ask(method: &str, opts: &glib::VariantDict) -> Ask {
    let s = |k: &str| opts.lookup::<String>(k).ok().flatten();
    let b = |k: &str| opts.lookup::<bool>(k).ok().flatten().unwrap_or(false);
    let path = |k: &str| opts.lookup::<Vec<u8>>(k).ok().flatten().map(bytes_path).filter(|p| !p.is_empty());
    let current_file = path("current_file");
    let mut folder = path("current_folder");
    let mut save_name = s("current_name").unwrap_or_default();
    if let Some(f) = &current_file {
        let p = std::path::Path::new(f);
        folder = folder.or_else(|| p.parent().map(|d| d.to_string_lossy().into_owned()));
        if save_name.is_empty() {
            save_name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        }
    }
    let save_files = match method {
        "SaveFiles" => opts.lookup::<Vec<Vec<u8>>>("files").ok().flatten().unwrap_or_default().into_iter().map(bytes_path).collect(),
        _ => Vec::new(),
    };
    let open = match method {
        "OpenFile" => Some((b("multiple"), b("directory"))),
        "SaveFiles" => Some((false, true)),
        _ => None,
    };
    let default_accept = match (method, open) {
        ("OpenFile", Some((_, true))) | ("SaveFiles", _) => "_Select",
        ("OpenFile", _) => "_Open",
        _ => "_Save",
    };
    let accept = s("accept_label").filter(|l| !l.is_empty()).unwrap_or_else(|| default_accept.into());
    Ask { open, save_name, save_files, folder, accept }
}

/// The URIs to answer with: the chosen files, or for SaveFiles each name inside the chosen folder.
fn result_uris(chosen: &[gio::File], save_files: &[String]) -> Vec<String> {
    match (chosen, save_files.is_empty()) {
        ([dir], false) => save_files.iter().map(|n| dir.child(n).uri().to_string()).collect(),
        _ => chosen.iter().map(|f| f.uri().to_string()).collect(),
    }
}

thread_local! {
    static APP: RefCell<Option<gtk::Application>> = const { RefCell::new(None) };
    /// Requests that arrived while GTK was starting
    static QUEUE: RefCell<Vec<(gio::DBusConnection, String, glib::Variant, gio::DBusMethodInvocation)>> = const { RefCell::new(Vec::new()) };
    static LINGER: RefCell<Option<gio::ApplicationHoldGuard>> = const { RefCell::new(None) };
    static LINGER_GEN: Cell<u64> = const { Cell::new(0) };
}

/// Keeps the process for `LINGER_SECS` more, unless a dialog opens meanwhile.
fn linger(app: &gtk::Application) {
    let id = LINGER_GEN.get() + 1;
    LINGER_GEN.set(id);
    LINGER.with_borrow_mut(|l| *l = Some(app.hold()));
    glib::timeout_add_seconds_local_once(LINGER_SECS, move || {
        if LINGER_GEN.get() == id {
            LINGER.with_borrow_mut(|l| l.take());
        }
    });
}

/// Makes the dialog belong to the app's window (on top of it, focused), as the portal asks.
fn set_parent(window: &gtk::ApplicationWindow, parent: &str) {
    // In libgtk-4 (GDK's Wayland backend), which RFiles links anyway
    extern "C" {
        fn gdk_wayland_toplevel_set_transient_for_exported(
            toplevel: *mut gtk::gdk::ffi::GdkToplevel,
            handle: *const std::ffi::c_char,
        ) -> glib::ffi::gboolean;
    }
    let Some(handle) = parent.strip_prefix("wayland:") else { return };
    let Some(surface) = window.surface() else { return };
    if surface.type_().name() != "GdkWaylandToplevel" {
        return;
    }
    let Ok(handle) = std::ffi::CString::new(handle) else { return };
    unsafe {
        use glib::translate::ToGlibPtr;
        let s: *mut gtk::gdk::ffi::GdkSurface = surface.to_glib_none().0;
        gdk_wayland_toplevel_set_transient_for_exported(s.cast(), handle.as_ptr());
    }
}

fn handle_request(conn: &gio::DBusConnection, method: &str, params: &glib::Variant, inv: gio::DBusMethodInvocation) {
    let Some(app) = APP.with_borrow(|a| a.clone()) else {
        QUEUE.with_borrow_mut(|q| q.push((conn.clone(), method.to_string(), params.clone(), inv)));
        return;
    };
    let handle = params.child_value(0).str().unwrap_or_default().to_string();
    let parent = params.child_value(2).str().unwrap_or_default().to_string();
    let title = params.child_value(3).str().unwrap_or_default().to_string();
    let opts = glib::VariantDict::new(Some(&params.child_value(4)));
    let ask = read_ask(method, &opts);
    let title = if title.is_empty() { if ask.open.is_some() { "Open" } else { "Save" }.to_string() } else { title };
    let start = ask
        .folder
        .as_deref()
        .map(gio::File::for_path)
        .filter(|f| f.query_file_type(gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>) == gio::FileType::Directory)
        .or_else(crate::util::downloads_dir)
        .unwrap_or_else(crate::util::home);
    let kind = match ask.open {
        Some((multiple, folders)) => PickKind::Open { multiple, folders },
        None => PickKind::Save { name: ask.save_name.clone() },
    };

    // The portal calls Close on the request object when the app gives up waiting
    let window: Rc<RefCell<Option<glib::WeakRef<gtk::ApplicationWindow>>>> = Rc::default();
    let w = window.clone();
    let node = gio::DBusNodeInfo::for_xml(XML).ok();
    let reg = node.as_ref().and_then(|n| n.lookup_interface("org.freedesktop.impl.portal.Request")).and_then(|iface| {
        conn.register_object(&handle, &iface)
            .method_call(move |_, _, _, _, _, _, inv| {
                inv.return_value(None);
                if let Some(win) = w.borrow().as_ref().and_then(|w| w.upgrade()) {
                    win.close();
                }
            })
            .build()
            .ok()
    });

    let conn2 = conn.clone();
    let save_files = ask.save_files;
    let done: app::PickDone = Box::new(move |chosen| {
        if let Some(id) = reg {
            let _ = conn2.unregister_object(id);
        }
        let results = glib::VariantDict::new(None);
        let code = match chosen {
            Some(files) => {
                results.insert_value("uris", &result_uris(&files, &save_files).to_variant());
                OK
            }
            None => CANCELLED,
        };
        inv.return_value(Some(&glib::Variant::tuple_from_iter([code.to_variant(), results.end()])));
    });
    let picker = app::build_picker(&app, start, kind, &title, &ask.accept, done);
    set_parent(&picker.window, &parent);
    *window.borrow_mut() = Some(picker.window.downgrade());
}

/// `rfiles --file-chooser`: started by D-Bus for the portal.
pub fn run() -> glib::ExitCode {
    // The name first, with GIO only: the portal waits for it before it answers anyone,
    // and GTK's start-up asks the portal for settings
    let ml = glib::MainLoop::new(None, false);
    let lost = Rc::new(Cell::new(false));
    let (m1, m2, l) = (ml.clone(), ml.clone(), lost.clone());
    let _owner = gio::bus_own_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameOwnerFlags::NONE,
        |conn, _| {
            let Some(iface) = gio::DBusNodeInfo::for_xml(XML)
                .ok()
                .and_then(|n| n.lookup_interface("org.freedesktop.impl.portal.FileChooser"))
            else {
                return;
            };
            let reg = conn
                .register_object(OBJECT_PATH, &iface)
                .method_call(|conn, _, _, _, method, params, inv| handle_request(&conn, method, &params, inv))
                .build();
            if let Err(e) = reg {
                eprintln!("rfiles: couldn't register the file chooser: {e}");
            }
        },
        move |_, _| m1.quit(),
        move |_, _| {
            l.set(true);
            m2.quit();
        },
    );
    let m3 = ml.clone();
    glib::timeout_add_seconds_local_once(5, move || m3.quit());
    ml.run();
    if lost.get() {
        // Another one already runs
        return glib::ExitCode::SUCCESS;
    }

    // NON_UNIQUE: its own process, never handing windows to (or taking them from) the main RFiles
    let gapp = gtk::Application::builder()
        .application_id("io.github.rfiles.Files")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    gapp.connect_startup(|a| {
        crate::theme::init();
        APP.with_borrow_mut(|x| *x = Some(a.clone()));
        linger(a);
        a.connect_window_removed(|a, _| {
            if a.windows().is_empty() {
                linger(a);
            }
        });
        glib::idle_add_local_once(|| {
            for (conn, method, params, inv) in QUEUE.take() {
                handle_request(&conn, &method, &params, inv);
            }
        });
    });
    gapp.connect_activate(|_| {});
    gapp.run_with_args(&[] as &[String])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(entries: &[(&str, glib::Variant)]) -> glib::VariantDict {
        let d = glib::VariantDict::new(None);
        for (k, v) in entries {
            d.insert_value(k, v);
        }
        d
    }

    fn path(p: &str) -> glib::Variant {
        let mut b = p.as_bytes().to_vec();
        b.push(0);
        b.to_variant()
    }

    #[test]
    fn save_reads_name_and_folder() {
        let a = read_ask("SaveFile", &dict(&[("current_name", "a.pdf".to_variant()), ("current_folder", path("/home/u/Downloads"))]));
        assert_eq!(a.open, None);
        assert_eq!((a.save_name.as_str(), a.folder.as_deref(), a.accept.as_str()), ("a.pdf", Some("/home/u/Downloads"), "_Save"));
        // current_file gives both when they are missing
        let a = read_ask("SaveFile", &dict(&[("current_file", path("/tmp/x/b.txt"))]));
        assert_eq!((a.save_name.as_str(), a.folder.as_deref()), ("b.txt", Some("/tmp/x")));
    }

    #[test]
    fn open_reads_flags_and_label() {
        let a = read_ask("OpenFile", &dict(&[("multiple", true.to_variant()), ("accept_label", "_Upload".to_variant())]));
        assert_eq!((a.open, a.accept.as_str(), a.folder), (Some((true, false)), "_Upload", None));
        let a = read_ask("OpenFile", &dict(&[("directory", true.to_variant())]));
        assert_eq!((a.open, a.accept.as_str()), (Some((false, true)), "_Select"));
    }

    #[test]
    fn save_files_puts_names_in_the_folder() {
        let files = vec![path("a.jpg"), path("b.jpg")];
        let a = read_ask("SaveFiles", &dict(&[("files", glib::Variant::array_from_iter_with_type(glib::VariantTy::BYTE_STRING, files))]));
        assert_eq!((a.open, a.save_files.clone()), (Some((false, true)), vec!["a.jpg".to_string(), "b.jpg".to_string()]));
        let dir = gio::File::for_path("/tmp/d");
        assert_eq!(result_uris(&[dir.clone()], &a.save_files), vec!["file:///tmp/d/a.jpg", "file:///tmp/d/b.jpg"]);
        assert_eq!(result_uris(&[dir], &[]), vec!["file:///tmp/d"]);
    }
}
