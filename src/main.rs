mod app;
mod archive;
mod batch_rename;
mod config;
mod dbus_service;
mod dnd;
mod docs;
mod fileops;
mod gpu;
mod pathbar;
mod preview;
mod properties;
mod thumbs;
mod trash;
mod util;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

const CSS: &str = r#"
/* ---------- overall ---------- */
window.rfiles { background-color: #000000; color: #e6e6e6; }

/* ---------- title bar: window buttons, + button, tabs, search ---------- */
.titlebar-box { background-color: #0e0e0e; padding: 4px 8px 6px 10px; border-bottom: 1px solid #1d1d1d; }
.top-row { min-height: 30px; }
.app-title { font-weight: bold; color: #9a9a9a; }
windowcontrols button { min-width: 26px; min-height: 26px; margin: 0 2px; }

button.new-tab {
  min-width: 26px; min-height: 26px; padding: 0; margin-left: 2px;
  border-radius: 7px; color: #8a8a8a;
}
button.new-tab:hover { background: #262626; color: #ffffff; }

.tab {
  background: #191919; border-radius: 9px; padding: 5px 4px 5px 10px; color: #a8a8a8;
  min-height: 26px; border: 1px solid #222;
}
.tab:hover { background: #232323; color: #ddd; }
.tab:focus-visible { outline: 2px solid #3584e4; outline-offset: 1px; }
.tab.active { background: #2e2e2e; color: #ffffff; border-color: #3584e4; }
button.tab-close { min-width: 22px; min-height: 22px; padding: 0; border-radius: 6px; }

entry.search { background: #181818; border-radius: 9px; min-height: 32px; }

/* ---------- navigation bar ---------- */
.navbar { background: #0a0a0a; padding: 5px 8px; border-bottom: 1px solid #1d1d1d; }
entry.path { background: #161616; border-radius: 8px; }
button.pinned { color: #4a9eff; background-color: rgba(53, 132, 228, 0.18); }

/* ---------- sidebar (a bit gray) ---------- */
.sidebar, .sidebar viewport, .sidebar list, .sidebar list.navigation-sidebar { background-color: #2a2a2a; }
.sidebar list row { color: #e2e2e2; padding: 7px 10px; margin: 1px 6px; border-radius: 7px; }
.sidebar list row:hover { background-color: #363636; }
.sidebar list row:selected { background-color: #444444; color: #ffffff; }
.sidebar-header { font-size: 11px; font-weight: bold; color: #8c8c8c; margin-top: 12px; }
.sidebar-hint { font-size: 11px; color: #6e6e6e; }
.sidebar list row.pin-missing { color: #6e6e6e; }
/* Small enough not to make device rows taller than the others */
.sidebar-eject { padding: 0 4px; min-height: 0; min-width: 0; margin: -4px -6px -4px 0; }

/* ---------- files (pitch black) ---------- */
.files, .files viewport, gridview.files, columnview.files, columnview.files listview,
columnview.files > listview > row, .files-area { background-color: #000000; color: #e8e8e8; }
gridview.files > child { padding: 6px 4px; margin: 2px; border-radius: 10px; }
gridview.files > child:hover { background-color: #121212; }
gridview.files > child:selected { background-color: rgba(53, 132, 228, 0.38); }
columnview.files > listview > row:hover { background-color: #101010; }
columnview.files > listview > row:selected { background-color: rgba(53, 132, 228, 0.38); }
columnview.files > header { background-color: #050505; }
columnview.files > header > button { background: #050505; color: #8c8c8c; border-color: #1a1a1a; }
.empty-view { color: #7a7a7a; font-size: 15px; }
.statusbar { background: #080808; color: #808080; padding: 4px 12px; font-size: 12px; border-top: 1px solid #161616; }

/* ---------- preview ---------- */
.preview { background-color: #050505; padding: 14px; border-left: 1px solid #1d1d1d; }
.preview-title { font-size: 17px; font-weight: bold; color: #ffffff; }
.meta { color: #9b9b9b; font-size: 12.5px; }
textview.preview-text, textview.preview-text text { background-color: #0c0c0c; color: #dcdcdc; }
.preview-body { border-radius: 10px; }
.dim { color: #7a7a7a; }
.small { font-size: 11px; }
.error { color: #ff7b72; }

paned > separator { background-color: #1d1d1d; min-width: 1px; }
popover.menu contents { background-color: #1c1c1c; }

/* ---------- right-click menu: compact, everything visible ---------- */
popover.ctx-menu contents { background-color: #1c1c1c; padding: 4px; border-radius: 10px; }
popover.ctx-menu button.ctx-item { padding: 5px 10px; min-height: 0; border-radius: 6px; font-weight: normal; }
popover.ctx-menu button.ctx-item:hover { background-color: #2f2f2f; }
popover.ctx-menu separator { margin: 3px 6px; background-color: #2c2c2c; min-height: 1px; }
.accel { color: #6f6f6f; font-size: 12px; }

/* ---------- sidebar drag & drop ---------- */
.sidebar list row.drop-above { box-shadow: inset 0 2px #3584e4; }
.sidebar list row.drop-below { box-shadow: inset 0 -2px #3584e4; }

/* ---------- file drag & drop (dnd.rs): highlight the drop target ---------- */
gridview.files > child.dnd-hover, columnview.files > listview > row.dnd-hover,
.sidebar list row.dnd-hover { background-color: rgba(53, 132, 228, 0.30); outline: 2px solid #3584e4; outline-offset: -2px; }
scrolledwindow.files.dnd-hover { outline: 2px solid #3584e4; outline-offset: -2px; }

/* ---------- properties dialog ---------- */
window.props .props-name { font-size: 16px; font-weight: bold; color: #ffffff; }
window.props .props-key { color: #8c8c8c; }
window.props .props-head { color: #8c8c8c; font-size: 11px; font-weight: bold; }
window.props .props-mono, window.props entry.props-octal { font-family: monospace; }
window.props entry.props-octal { min-height: 26px; background: #161616; }
window.props separator { background-color: #1d1d1d; }

/* ---------- batch rename dialog (batch_rename.rs) ---------- */
listview.br-list { background-color: #0a0a0a; }
listview.br-list > row { padding: 0; }
.br-row { padding: 3px 8px; border-radius: 6px; }
.br-row.br-same { color: #6f6f6f; }
.br-row.br-bad { background-color: rgba(255, 123, 114, 0.14); }
.br-arrow { color: #6f6f6f; }
scrolledwindow.br-scroll { border: 1px solid #1d1d1d; border-radius: 8px; }
"#;

/// How long the process stays alive with no windows, so the next window
/// ("Show in folder" or a launch) opens instantly instead of starting over.
const LINGER_SECS: u32 = 30;

/// Whether `file` is a local file that is not a folder (so it opens as its folder, selected).
fn is_file(file: &gio::File) -> bool {
    file.path().is_some()
        && !matches!(
            file.query_file_type(gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>),
            gio::FileType::Directory | gio::FileType::Mountable | gio::FileType::Unknown
        )
}

/// Opens a window for `file`: a folder opens as-is; a file opens its folder
/// with the file selected. `startup_id` lets the window take focus when
/// another app (a browser, the desktop portal) asked for it.
pub fn open_location(gapp: &gtk::Application, file: &gio::File, startup_id: Option<&str>) {
    let w = match (is_file(file), file.parent(), file.basename()) {
        (true, Some(dir), Some(name)) => {
            let w = app::build(gapp, dir);
            w.select_when_loaded(name.to_string_lossy().into_owned());
            w
        }
        _ => app::build(gapp, file.clone()),
    };
    if let Some(id) = startup_id.filter(|id| !id.is_empty()) {
        w.window.set_startup_id(id);
        w.window.present();
    }
}

/// `rfiles --version` / `-V`: the line to print, if that is what was asked for.
fn version_request(args: &[String]) -> Option<String> {
    args.iter()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
        .then(|| format!("rfiles {}", env!("CARGO_PKG_VERSION")))
}

#[cfg(test)]
mod version_tests {
    use super::version_request;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn version_flags_print_name_and_version() {
        let want = Some(format!("rfiles {}", env!("CARGO_PKG_VERSION")));
        assert_eq!(version_request(&v(&["rfiles", "--version"])), want);
        assert_eq!(version_request(&v(&["rfiles", "-V"])), want);
    }

    #[test]
    fn other_arguments_are_not_a_version_request() {
        assert_eq!(version_request(&v(&["rfiles"])), None);
        assert_eq!(version_request(&v(&["rfiles", "--dbus-service"])), None);
        assert_eq!(version_request(&v(&["--version"])), None);
        assert_eq!(version_request(&v(&["rfiles", "/tmp"])), None);
    }
}

fn main() -> glib::ExitCode {
    // `--version`: answer before GTK, the D-Bus service or any forwarding to a running RFiles
    if let Some(line) = version_request(&std::env::args().collect::<Vec<_>>()) {
        println!("{line}");
        return glib::ExitCode::SUCCESS;
    }
    // Before anything else: see gpu.rs
    gpu::keep_sleeping_cards_asleep();

    // `--dbus-service`: started by D-Bus for a "Show in folder" request.
    // It becomes the shared RFiles process but opens no window by itself.
    let mut args: Vec<String> = std::env::args().collect();
    let service = args.get(1).map(String::as_str) == Some("--dbus-service");
    if service {
        args.remove(1);
        // Answer the request before GTK starts (see dbus_service.rs)
        dbus_service::wait_for_first_request(std::time::Duration::from_secs(5));
    }

    // One process for all RFiles windows: a second launch just asks the
    // running one to open a window, which is much faster and lighter.
    let app = gtk::Application::builder()
        .application_id("io.github.rfiles.Files")
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    // Listed in `--help`; the flag itself is answered at the top of main
    app.add_main_option(
        "version",
        glib::Char::from(b'V'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Print the version and exit",
        None,
    );

    app.connect_startup(move |a| {
        if let Some(s) = gtk::Settings::default() {
            s.set_gtk_application_prefer_dark_theme(true);
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        dbus_service::own_name();
        // GTK is ready: open "Show in folder" requests from now on
        let app = a.clone();
        glib::idle_add_local_once(move || dbus_service::attach(&app));
        // Keep the process briefly when its last window closes
        a.connect_window_removed(|a, _| {
            if a.windows().is_empty() {
                dbus_service::linger(a, LINGER_SECS);
            }
        });
        if service {
            // Stay a little even if the request opened nothing
            dbus_service::linger(a, LINGER_SECS);
        }
    });

    let skip_activate = std::cell::Cell::new(service);
    app.connect_activate(move |a| {
        if skip_activate.replace(false) {
            return;
        }
        let _ = app::build(a, util::home());
    });
    app.connect_open(|a, files, hint| {
        // The hint carries the startup id of forwarded requests
        let id = Some(hint).filter(|h| !h.is_empty());
        if files.is_empty() {
            open_location(a, &util::home(), id);
            return;
        }
        // One window per distinct folder, in order
        let items: Vec<(String, bool)> = files.iter().map(|f| (f.uri().to_string(), !is_file(f))).collect();
        for i in dbus_service::distinct_windows(&items) {
            open_location(a, &files[i], id);
        }
    });

    // Registering runs `startup` (which claims the D-Bus name), so it must
    // come after the handlers above. If RFiles is already running, that
    // process owns the name and answers the request: nothing to do here.
    if service && (app.register(None::<&gio::Cancellable>).is_err() || app.is_remote()) {
        dbus_service::forward_queued(&app);
        return glib::ExitCode::SUCCESS;
    }

    app.run_with_args(&args)
}
