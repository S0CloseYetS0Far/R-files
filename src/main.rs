mod app;
mod archive;
mod banner;
mod batch_rename;
mod config;
mod dbus_service;
mod dnd;
mod docs;
mod fileops;
mod gpu;
mod jobs;
mod launch;
mod pathbar;
mod preview;
mod properties;
mod theme;
mod thumbs;
mod trash;
mod undo;
mod util;

use gtk::prelude::*;
use gtk::{gio, glib};

/// How long the process stays alive with no windows, so the next window
/// ("Show in folder" or a launch) opens instantly instead of starting over.
const LINGER_SECS: u32 = 30;

/// Identifies one version of the executable file: replacing it (install.sh,
/// a package update) gives a new inode or at least a new modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExeStamp {
    dev: u64,
    ino: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl ExeStamp {
    fn of(path: &std::path::Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some(Self { dev: m.dev(), ino: m.ino(), mtime: m.mtime(), mtime_nsec: m.mtime_nsec() })
    }
}

/// Whether the executable on disk is no longer the one this process started
/// from. Unknown at start-up means "not replaced"; gone now (uninstalled) means replaced.
fn exe_replaced(at_start: Option<ExeStamp>, now: Option<ExeStamp>) -> bool {
    at_start.is_some_and(|s| now != Some(s))
}

/// What to do when the process may have become idle.
#[derive(Debug, PartialEq, Eq)]
enum IdleStep {
    /// Windows are open: nothing changes.
    Keep,
    /// Stay a while for a quick next launch.
    Linger,
    /// Drop the linger hold; the process ends once no job holds it.
    Exit,
}

/// `last_window_closed`: the last window just closed (otherwise "quit if idle" was asked for).
fn idle_step(windows_open: bool, last_window_closed: bool, exe_replaced: bool) -> IdleStep {
    match (windows_open, last_window_closed, exe_replaced) {
        (true, _, _) => IdleStep::Keep,
        (false, true, false) => IdleStep::Linger,
        _ => IdleStep::Exit,
    }
}

#[cfg(test)]
mod exe_update_tests {
    use super::{exe_replaced, idle_step, ExeStamp, IdleStep};

    fn st(ino: u64, mtime: i64) -> ExeStamp {
        ExeStamp { dev: 1, ino, mtime, mtime_nsec: 0 }
    }

    #[test]
    fn replaced_when_inode_or_mtime_changes_or_file_is_gone() {
        assert!(!exe_replaced(Some(st(5, 100)), Some(st(5, 100))));
        assert!(exe_replaced(Some(st(5, 100)), Some(st(6, 100))));
        assert!(exe_replaced(Some(st(5, 100)), Some(st(5, 101))));
        assert!(exe_replaced(Some(st(5, 100)), None));
        // Never treated as replaced when the start-up stamp is unknown
        assert!(!exe_replaced(None, None));
        assert!(!exe_replaced(None, Some(st(5, 100))));
    }

    #[test]
    fn stamp_follows_a_file_replaced_like_install_does() {
        let dir = std::env::temp_dir().join(format!("rfiles-exe-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("rfiles");
        std::fs::write(&exe, b"old").unwrap();
        let start = ExeStamp::of(&exe);
        assert!(start.is_some());
        assert!(!exe_replaced(start, ExeStamp::of(&exe)));
        // install(1) puts a new file in place of the old one
        let new = dir.join("rfiles.new");
        std::fs::write(&new, b"new").unwrap();
        std::fs::rename(&new, &exe).unwrap();
        assert!(exe_replaced(start, ExeStamp::of(&exe)));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(exe_replaced(start, ExeStamp::of(&exe)));
    }

    #[test]
    fn idle_decisions() {
        // Open windows always keep the process as it is
        for (closed, replaced) in [(true, false), (true, true), (false, false), (false, true)] {
            assert_eq!(idle_step(true, closed, replaced), IdleStep::Keep);
        }
        assert_eq!(idle_step(false, true, false), IdleStep::Linger);
        assert_eq!(idle_step(false, true, true), IdleStep::Exit);
        // "quit-if-idle" with no windows
        assert_eq!(idle_step(false, false, false), IdleStep::Exit);
        assert_eq!(idle_step(false, false, true), IdleStep::Exit);
    }
}

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
    // `--thumbnail`: a short-lived helper that decodes one file for thumbs.rs; no GTK, no display
    if let Some(code) = thumbs::helper_request(&std::env::args().collect::<Vec<_>>()) {
        return glib::ExitCode::from(code as u8);
    }
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

    // Checked when the last window closes: an updated binary should take over then
    let exe = std::env::current_exe().ok();
    let exe_at_start = exe.as_deref().and_then(ExeStamp::of);

    app.connect_startup(move |a| {
        theme::init();
        dbus_service::own_name();
        // GTK is ready: open "Show in folder" requests from now on
        let app = a.clone();
        glib::idle_add_local_once(move || dbus_service::attach(&app));
        // Keep the process briefly when its last window closes, unless the
        // binary was updated meanwhile: then end now so the next launch is the new version
        let exe = exe.clone();
        a.connect_window_removed(move |a, _| {
            if !a.windows().is_empty() {
                return;
            }
            let replaced = exe_replaced(exe_at_start, exe.as_deref().and_then(ExeStamp::of));
            match idle_step(false, true, replaced) {
                IdleStep::Linger => dbus_service::linger(a, LINGER_SECS),
                IdleStep::Exit => dbus_service::stop_lingering(),
                IdleStep::Keep => {}
            }
        });
        // For install.sh, over D-Bus (org.gtk.Actions): exit if no window is open
        let quit_if_idle = gio::SimpleAction::new("quit-if-idle", None);
        let weak = a.downgrade();
        quit_if_idle.connect_activate(move |_, _| {
            if let Some(a) = weak.upgrade() {
                if idle_step(!a.windows().is_empty(), false, false) == IdleStep::Exit {
                    dbus_service::stop_lingering();
                }
            }
        });
        a.add_action(&quit_if_idle);
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
