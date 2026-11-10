mod app;
mod archive;
mod batch_rename;
mod config;
mod dnd;
mod docs;
mod fileops;
mod preview;
mod properties;
mod thumbs;
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

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id("io.github.rfiles.Files")
        .flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_startup(|_| {
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
    });

    app.connect_activate(|a| {
        let _ = app::build(a, util::home());
    });
    app.connect_open(|a, files, _| {
        let start = files.first().cloned().unwrap_or_else(util::home);
        let _ = app::build(a, start);
    });

    app.run()
}
