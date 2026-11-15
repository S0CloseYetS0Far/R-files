//! Colours: the Black look (the default), a Light look, and "Follow system" with the desktop's accent colour.
//!
//! GTK 4.14 has no CSS variables, so the colours are `@define-color` names that sit in front of one CSS body;
//! switching theme reloads the single provider that every window shares.

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;

/// What the user chose in the settings popover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Black,
    Light,
    Follow,
}

pub const THEME_NAMES: [&str; 3] = ["Black", "Light", "Follow system"];

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Black => "black",
            Theme::Light => "light",
            Theme::Follow => "system",
        }
    }

    /// Anything unknown is the Black look, so a bad settings line changes nothing.
    pub fn parse(s: &str) -> Theme {
        match s.trim() {
            "light" => Theme::Light,
            "system" => Theme::Follow,
            _ => Theme::Black,
        }
    }

    pub fn index(self) -> u32 {
        self as u32
    }

    pub fn from_index(i: u32) -> Theme {
        match i {
            1 => Theme::Light,
            2 => Theme::Follow,
            _ => Theme::Black,
        }
    }
}

/// The two looks that exist; a `Theme` resolves to one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Black,
    Light,
}

/// `org.freedesktop.appearance color-scheme`: 1 is dark, 2 is light, 0 (or anything else) no preference.
/// `fallback_dark` decides when the desktop gives no answer.
pub fn look_for(theme: Theme, portal_scheme: Option<u32>, fallback_dark: bool) -> Look {
    match theme {
        Theme::Black => Look::Black,
        Theme::Light => Look::Light,
        Theme::Follow => match portal_scheme {
            Some(1) => Look::Black,
            Some(2) => Look::Light,
            _ if fallback_dark => Look::Black,
            _ => Look::Light,
        },
    }
}

/// `org.freedesktop.appearance accent-color`: (r, g, b) from 0 to 1. Values outside that range mean "no accent".
pub fn accent_rgb(c: (f64, f64, f64)) -> Option<(u8, u8, u8)> {
    let ok = |v: f64| (0.0..=1.0).contains(&v);
    if !(ok(c.0) && ok(c.1) && ok(c.2)) {
        return None;
    }
    let b = |v: f64| (v * 255.0).round() as u8;
    Some((b(c.0), b(c.1), b(c.2)))
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// `c` moved towards `to` by `t` (0 to 1).
fn mix(c: (u8, u8, u8), to: u8, t: f64) -> (u8, u8, u8) {
    let m = |v: u8| (v as f64 + (to as f64 - v as f64) * t).round() as u8;
    (m(c.0), m(c.1), m(c.2))
}

/// Today's blue.
const DEFAULT_ACCENT: (u8, u8, u8) = (0x35, 0x84, 0xe4);

/// The `@define-color` lines for the accent: the colour itself, a readable variant for text, and see-through ones.
fn accent_colors(accent: Option<(u8, u8, u8)>, look: Look) -> String {
    let a = accent.unwrap_or(DEFAULT_ACCENT);
    // Text must stay readable on the window colour, so it goes lighter on black and darker on light
    let text = match (accent, look) {
        (None, Look::Black) => (0x4a, 0x9e, 0xff),
        (_, Look::Black) => mix(a, 255, 0.25),
        (_, Look::Light) => mix(a, 0, 0.25),
    };
    let rgba = |al: &str| format!("rgba({}, {}, {}, {al})", a.0, a.1, a.2);
    let mut s = format!(
        "@define-color accent {};\n@define-color accent_text {};\n@define-color accent_a18 {};\n\
         @define-color accent_a30 {};\n@define-color accent_a38 {};\n",
        hex(a),
        hex(text),
        rgba("0.18"),
        rgba("0.30"),
        rgba("0.38"),
    );
    if accent.is_some() {
        // GTK's own widgets (switches, check marks, progress bars) use these names
        s += &format!("@define-color accent_bg_color {};\n@define-color accent_color {};\n", hex(a), hex(text));
    }
    s
}

const BLACK: &[(&str, &str)] = &[
    ("win_bg", "#000000"),
    ("win_fg", "#e6e6e6"),
    ("titlebar_bg", "#0e0e0e"),
    ("edge", "#1d1d1d"),
    ("title_fg", "#9a9a9a"),
    ("newtab_fg", "#8a8a8a"),
    ("hover_bg", "#262626"),
    ("strong_fg", "#ffffff"),
    ("tab_bg", "#191919"),
    ("tab_fg", "#a8a8a8"),
    ("tab_edge", "#222222"),
    ("tab_hover_bg", "#232323"),
    ("tab_hover_fg", "#dddddd"),
    ("tab_active_bg", "#2e2e2e"),
    ("search_bg", "#181818"),
    ("navbar_bg", "#0a0a0a"),
    ("field_bg", "#161616"),
    ("side_bg", "#2a2a2a"),
    ("side_fg", "#e2e2e2"),
    ("side_hover", "#363636"),
    ("side_sel", "#444444"),
    ("header_fg", "#8c8c8c"),
    ("faint_fg", "#6e6e6e"),
    ("files_bg", "#000000"),
    ("files_fg", "#e8e8e8"),
    ("files_hover", "#121212"),
    ("row_hover", "#101010"),
    ("colhead_bg", "#050505"),
    ("colhead_edge", "#1a1a1a"),
    ("dim_fg", "#7a7a7a"),
    ("bar_bg", "#080808"),
    ("bar_fg", "#808080"),
    ("bar_edge", "#161616"),
    ("transfer_fg", "#a8a8a8"),
    ("banner_bg", "#0c0c0c"),
    ("banner_fg", "#d0d0d0"),
    ("text_bg", "#0c0c0c"),
    ("text_fg", "#dcdcdc"),
    ("preview_bg", "#050505"),
    ("meta_fg", "#9b9b9b"),
    ("error_fg", "#ff7b72"),
    ("error_edge", "#d9534f"),
    ("menu_bg", "#1c1c1c"),
    ("ctx_hover", "#2f2f2f"),
    ("ctx_sep", "#2c2c2c"),
    ("list_bg", "#0a0a0a"),
];

const LIGHT: &[(&str, &str)] = &[
    ("win_bg", "#ffffff"),
    ("win_fg", "#1f1f1f"),
    ("titlebar_bg", "#ebebeb"),
    ("edge", "#d0d0d0"),
    ("title_fg", "#5e5e5e"),
    ("newtab_fg", "#5e5e5e"),
    ("hover_bg", "#d9d9d9"),
    ("strong_fg", "#000000"),
    ("tab_bg", "#e0e0e0"),
    ("tab_fg", "#4a4a4a"),
    ("tab_edge", "#cfcfcf"),
    ("tab_hover_bg", "#d4d4d4"),
    ("tab_hover_fg", "#222222"),
    ("tab_active_bg", "#ffffff"),
    ("search_bg", "#f2f2f2"),
    ("navbar_bg", "#f4f4f4"),
    ("field_bg", "#ffffff"),
    ("side_bg", "#eaeaea"),
    ("side_fg", "#222222"),
    ("side_hover", "#dcdcdc"),
    ("side_sel", "#c8c8c8"),
    ("header_fg", "#5e5e5e"),
    ("faint_fg", "#8a8a8a"),
    ("files_bg", "#ffffff"),
    ("files_fg", "#1f1f1f"),
    ("files_hover", "#f1f1f1"),
    ("row_hover", "#f3f3f3"),
    ("colhead_bg", "#f6f6f6"),
    ("colhead_edge", "#dedede"),
    ("dim_fg", "#6e6e6e"),
    ("bar_bg", "#f4f4f4"),
    ("bar_fg", "#5e5e5e"),
    ("bar_edge", "#dcdcdc"),
    ("transfer_fg", "#4a4a4a"),
    ("banner_bg", "#f0f0f0"),
    ("banner_fg", "#2a2a2a"),
    ("text_bg", "#f6f6f6"),
    ("text_fg", "#222222"),
    ("preview_bg", "#f7f7f7"),
    ("meta_fg", "#666666"),
    ("error_fg", "#c01c28"),
    ("error_edge", "#d9534f"),
    ("menu_bg", "#ffffff"),
    ("ctx_hover", "#e8e8e8"),
    ("ctx_sep", "#dcdcdc"),
    ("list_bg", "#f4f4f4"),
];

/// The system accent counts only when following the system: the other themes keep today's blue.
pub fn accent_for(theme: Theme, system: Option<(u8, u8, u8)>) -> Option<(u8, u8, u8)> {
    system.filter(|_| theme == Theme::Follow)
}

/// The whole style sheet for a look and an accent.
pub fn css(look: Look, accent: Option<(u8, u8, u8)>) -> String {
    let table = match look {
        Look::Black => BLACK,
        Look::Light => LIGHT,
    };
    let mut s = String::new();
    for (name, value) in table {
        s += &format!("@define-color {name} {value};\n");
    }
    s += &accent_colors(accent, look);
    s += BODY;
    s
}

const BODY: &str = r#"
/* ---------- overall ---------- */
window.rfiles { background-color: @win_bg; color: @win_fg; }

/* ---------- title bar: window buttons, + button, tabs, search ---------- */
.titlebar-box { background-color: @titlebar_bg; padding: 4px 8px 6px 10px; border-bottom: 1px solid @edge; }
.top-row { min-height: 30px; }
.app-title { font-weight: bold; color: @title_fg; }
windowcontrols button { min-width: 26px; min-height: 26px; margin: 0 2px; }

button.new-tab {
  min-width: 26px; min-height: 26px; padding: 0; margin-left: 2px;
  border-radius: 7px; color: @newtab_fg;
}
button.new-tab:hover { background: @hover_bg; color: @strong_fg; }

.tab {
  background: @tab_bg; border-radius: 9px; padding: 5px 4px 5px 10px; color: @tab_fg;
  min-height: 26px; border: 1px solid @tab_edge;
}
.tab:hover { background: @tab_hover_bg; color: @tab_hover_fg; }
.tab:focus-visible { outline: 2px solid @accent; outline-offset: 1px; }
.tab.active { background: @tab_active_bg; color: @strong_fg; border-color: @accent; }
.tab.drop-before { box-shadow: inset 3px 0 @accent; }
.tab.drop-after { box-shadow: inset -3px 0 @accent; }
.tab.dnd-hover { background: @accent_a30; border-color: @accent; }
button.tab-close { min-width: 22px; min-height: 22px; padding: 0; border-radius: 6px; }

entry.search { background: @search_bg; border-radius: 9px; min-height: 32px; }

/* ---------- navigation bar ---------- */
.navbar { background: @navbar_bg; padding: 5px 8px; border-bottom: 1px solid @edge; }
entry.path { background: @field_bg; border-radius: 8px; }
button.pinned { color: @accent_text; background-color: @accent_a18; }

/* ---------- sidebar (a bit gray) ---------- */
.sidebar, .sidebar viewport, .sidebar list, .sidebar list.navigation-sidebar { background-color: @side_bg; }
.sidebar list row { color: @side_fg; padding: 7px 10px; margin: 1px 6px; border-radius: 7px; }
.sidebar list row:hover { background-color: @side_hover; }
.sidebar list row:selected { background-color: @side_sel; color: @strong_fg; }
.sidebar-header { font-size: 11px; font-weight: bold; color: @header_fg; margin-top: 12px; }
.sidebar-hint { font-size: 11px; color: @faint_fg; }
.sidebar list row.pin-missing { color: @faint_fg; }
/* Small enough not to make device rows taller than the others */
.sidebar-eject { padding: 0 4px; min-height: 0; min-width: 0; margin: -4px -6px -4px 0; }

/* ---------- files (pitch black in the Black theme) ---------- */
.files, .files viewport, gridview.files, columnview.files, columnview.files listview,
columnview.files > listview > row, .files-area { background-color: @files_bg; color: @files_fg; }
gridview.files > child { padding: 6px 4px; margin: 2px; border-radius: 10px; }
gridview.files > child:hover { background-color: @files_hover; }
gridview.files > child:selected { background-color: @accent_a38; }
columnview.files > listview > row:hover { background-color: @row_hover; }
columnview.files > listview > row:selected { background-color: @accent_a38; }
columnview.files > header { background-color: @colhead_bg; }
columnview.files > header > button { background: @colhead_bg; color: @header_fg; border-color: @colhead_edge; }
.empty-view { color: @dim_fg; font-size: 15px; }
.statusbar { background: @bar_bg; color: @bar_fg; padding: 4px 12px; font-size: 12px; border-top: 1px solid @bar_edge; }
.transfer-row { background: @bar_bg; color: @transfer_fg; padding: 0 4px 0 12px; font-size: 12px; }
.transfer-row { border-top: 1px solid @bar_edge; }
.transfer-row button { min-height: 22px; padding: 0 8px; }
.banner { background: @banner_bg; color: @banner_fg; padding: 0 4px 0 12px; font-size: 12px; border-top: 1px solid @bar_edge; }
.banner.banner-error { box-shadow: inset 3px 0 @error_edge; }
.banner button { min-height: 22px; padding: 0 8px; }
textview.details-text, textview.details-text text { background-color: @text_bg; color: @text_fg; }

/* ---------- preview ---------- */
.preview { background-color: @preview_bg; padding: 14px; border-left: 1px solid @edge; }
.preview-title { font-size: 17px; font-weight: bold; color: @strong_fg; }
.meta { color: @meta_fg; font-size: 12.5px; }
textview.preview-text, textview.preview-text text { background-color: @text_bg; color: @text_fg; }
.preview-body { border-radius: 10px; }
.dim { color: @dim_fg; }
.small { font-size: 11px; }
.error { color: @error_fg; }

paned > separator { background-color: @edge; min-width: 1px; }
popover.menu contents { background-color: @menu_bg; }

/* ---------- right-click menu: compact, everything visible ---------- */
popover.ctx-menu contents { background-color: @menu_bg; padding: 4px; border-radius: 10px; }
popover.ctx-menu button.ctx-item { padding: 5px 10px; min-height: 0; border-radius: 6px; font-weight: normal; }
popover.ctx-menu button.ctx-item:hover { background-color: @ctx_hover; }
popover.ctx-menu separator { margin: 3px 6px; background-color: @ctx_sep; min-height: 1px; }
.accel { color: @faint_fg; font-size: 12px; }

/* ---------- sidebar drag & drop ---------- */
.sidebar list row.drop-above { box-shadow: inset 0 2px @accent; }
.sidebar list row.drop-below { box-shadow: inset 0 -2px @accent; }

/* ---------- file drag & drop (dnd.rs): highlight the drop target ---------- */
gridview.files > child.dnd-hover, columnview.files > listview > row.dnd-hover,
.sidebar list row.dnd-hover { background-color: @accent_a30; outline: 2px solid @accent; outline-offset: -2px; }
scrolledwindow.files.dnd-hover { outline: 2px solid @accent; outline-offset: -2px; }

/* ---------- properties dialog ---------- */
window.props .props-name { font-size: 16px; font-weight: bold; color: @strong_fg; }
window.props .props-key { color: @header_fg; }
window.props .props-head { color: @header_fg; font-size: 11px; font-weight: bold; }
window.props .props-mono, window.props entry.props-octal { font-family: monospace; }
window.props entry.props-octal { min-height: 26px; background: @field_bg; }
window.props separator { background-color: @edge; }

/* ---------- batch rename dialog (batch_rename.rs) ---------- */
listview.br-list { background-color: @list_bg; }
listview.br-list > row { padding: 0; }
.br-row { padding: 3px 8px; border-radius: 6px; }
.br-row.br-same { color: @faint_fg; }
.br-row.br-bad { background-color: rgba(255, 123, 114, 0.14); }
.br-arrow { color: @faint_fg; }
scrolledwindow.br-scroll { border: 1px solid @edge; border-radius: 8px; }
"#;

/// What the desktop said, as far as it said anything.
#[derive(Default)]
struct System {
    scheme: Option<u32>,
    accent: Option<(u8, u8, u8)>,
}

struct State {
    provider: gtk::CssProvider,
    system: System,
    /// GTK's own dark preference before RFiles changed it
    fallback_dark: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Loads the style sheet for the saved theme and starts listening to the desktop's appearance settings.
/// Call once, after GTK has started.
pub fn init() {
    let fallback_dark = gtk::Settings::default().is_some_and(|s| s.is_gtk_application_prefer_dark_theme());
    let provider = gtk::CssProvider::new();
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    STATE.with(|s| *s.borrow_mut() = Some(State { provider, system: System::default(), fallback_dark }));
    apply();
    glib::spawn_future_local(watch_portal());
}

/// Applies the theme saved in the settings to every window.
pub fn apply() {
    let theme = crate::config::shared().borrow().theme;
    STATE.with(|s| {
        let s = s.borrow();
        let Some(st) = s.as_ref() else { return };
        let look = look_for(theme, st.system.scheme, st.fallback_dark);
        if let Some(gs) = gtk::Settings::default() {
            gs.set_gtk_application_prefer_dark_theme(look == Look::Black);
        }
        st.provider.load_from_string(&css(look, accent_for(theme, st.system.accent)));
    });
}

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const APPEARANCE: &str = "org.freedesktop.appearance";

/// A value that came wrapped in variants (the older `Read` call wraps it twice).
fn unwrap_variant(mut v: glib::Variant) -> glib::Variant {
    while v.type_() == glib::VariantTy::VARIANT {
        match v.as_variant() {
            Some(inner) => v = inner,
            None => break,
        }
    }
    v
}

fn scheme_of(v: &glib::Variant) -> Option<u32> {
    unwrap_variant(v.clone()).get::<u32>()
}

fn accent_of(v: &glib::Variant) -> Option<(u8, u8, u8)> {
    accent_rgb(unwrap_variant(v.clone()).get::<(f64, f64, f64)>()?)
}

fn set_system(f: impl FnOnce(&mut System)) {
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            f(&mut st.system);
        }
    });
    apply();
}

/// Reads one appearance setting: `ReadOne`, or the older `Read` where the portal predates it.
async fn read_setting(conn: &gio::DBusConnection, key: &str) -> Option<glib::Variant> {
    let args = (APPEARANCE, key).to_variant();
    for method in ["ReadOne", "Read"] {
        let reply = conn
            .call_future(Some(PORTAL), PORTAL_PATH, SETTINGS, method, Some(&args), None, gio::DBusCallFlags::NONE, 2000)
            .await;
        if let Ok(r) = reply {
            return Some(r.child_value(0));
        }
    }
    None
}

/// Reads the colour scheme and accent once, then follows `SettingChanged`. Does nothing without the portal.
async fn watch_portal() {
    let Ok(conn) = gio::bus_get_future(gio::BusType::Session).await else { return };
    // Subscribe first so a change between the reads and the subscription isn't lost
    // The plain call: its id does not unsubscribe when dropped, and this lasts as long as the process
    #[allow(deprecated)]
    let _ = conn.signal_subscribe(
        Some(PORTAL),
        Some(SETTINGS),
        Some("SettingChanged"),
        Some(PORTAL_PATH),
        Some(APPEARANCE),
        gio::DBusSignalFlags::NONE,
        |_, _, _, _, _, params| {
            let key = params.child_value(1);
            let value = params.child_value(2);
            match key.str() {
                Some("color-scheme") => set_system(|s| s.scheme = scheme_of(&value)),
                Some("accent-color") => set_system(|s| s.accent = accent_of(&value)),
                _ => {}
            }
        },
    );
    if let Some(v) = read_setting(&conn, "color-scheme").await {
        let scheme = scheme_of(&v);
        set_system(|s| s.scheme = scheme);
    }
    if let Some(v) = read_setting(&conn, "accent-color").await {
        let accent = accent_of(&v);
        set_system(|s| s.accent = accent);
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn setting_text_round_trips_and_defaults_to_black() {
        for t in [Theme::Black, Theme::Light, Theme::Follow] {
            assert_eq!(Theme::parse(t.as_str()), t);
            assert_eq!(Theme::from_index(t.index()), t);
        }
        assert_eq!(Theme::parse(" light "), Theme::Light);
        assert_eq!(Theme::parse(""), Theme::Black);
        assert_eq!(Theme::parse("purple"), Theme::Black);
        assert_eq!(Theme::from_index(9), Theme::Black);
        assert_eq!(THEME_NAMES.len(), 3);
    }

    #[test]
    fn portal_scheme_picks_the_look_only_when_following() {
        assert_eq!(look_for(Theme::Follow, Some(1), false), Look::Black);
        assert_eq!(look_for(Theme::Follow, Some(2), true), Look::Light);
        // No preference or no portal: GTK's own setting decides
        assert_eq!(look_for(Theme::Follow, Some(0), true), Look::Black);
        assert_eq!(look_for(Theme::Follow, None, true), Look::Black);
        assert_eq!(look_for(Theme::Follow, None, false), Look::Light);
        assert_eq!(look_for(Theme::Follow, Some(7), false), Look::Light);
        // A fixed theme ignores the desktop
        assert_eq!(look_for(Theme::Black, Some(2), false), Look::Black);
        assert_eq!(look_for(Theme::Light, Some(1), true), Look::Light);
    }

    #[test]
    fn accent_tuple_becomes_a_colour() {
        assert_eq!(accent_rgb((0.0, 0.0, 0.0)), Some((0, 0, 0)));
        assert_eq!(accent_rgb((1.0, 0.5, 0.0)), Some((255, 128, 0)));
        assert_eq!(hex((0x35, 0x84, 0xe4)), "#3584e4");
        // The portal sends out-of-range numbers for "no accent"
        assert_eq!(accent_rgb((-1.0, 0.5, 0.5)), None);
        assert_eq!(accent_rgb((0.5, 2.0, 0.5)), None);
        assert_eq!(accent_rgb((f64::NAN, 0.5, 0.5)), None);
    }

    #[test]
    fn variants_from_the_portal_are_read_however_wrapped() {
        let scheme = 1u32.to_variant();
        assert_eq!(scheme_of(&scheme), Some(1));
        assert_eq!(scheme_of(&scheme.to_variant()), Some(1));
        assert_eq!(scheme_of(&"x".to_variant()), None);
        let accent = (1.0f64, 0.0f64, 0.0f64).to_variant();
        assert_eq!(accent_of(&accent.to_variant().to_variant()), Some((255, 0, 0)));
        assert_eq!(accent_of(&(-1.0f64, -1.0f64, -1.0f64).to_variant()), None);
    }

    #[test]
    fn default_accent_keeps_todays_blue_and_system_accent_replaces_it() {
        let d = css(Look::Black, None);
        assert!(d.contains("@define-color accent #3584e4;"));
        assert!(d.contains("@define-color accent_text #4a9eff;"));
        assert!(d.contains("@define-color accent_a38 rgba(53, 132, 228, 0.38);"));
        assert!(!d.contains("accent_bg_color"));
        let g = css(Look::Light, Some((0x3a, 0x94, 0x4a)));
        assert!(g.contains("@define-color accent #3a944a;"));
        assert!(g.contains("@define-color accent_a30 rgba(58, 148, 74, 0.30);"));
        assert!(g.contains("@define-color accent_bg_color #3a944a;"));
        // Darker than the accent itself, so it reads on white
        assert!(g.contains("@define-color accent_text #2c6f38;"));
    }

    #[test]
    fn system_accent_applies_only_when_following_the_system() {
        let orange = Some((0xe9, 0x54, 0x20));
        assert_eq!(accent_for(Theme::Black, orange), None);
        assert_eq!(accent_for(Theme::Light, orange), None);
        assert_eq!(accent_for(Theme::Follow, orange), orange);
        assert_eq!(accent_for(Theme::Follow, None), None);
        let black = css(Look::Black, accent_for(Theme::Black, orange));
        assert!(black.contains("@define-color accent #3584e4;"));
        assert!(black.contains("@define-color accent_text #4a9eff;"));
        assert!(!black.contains("e95420"));
        let follow = css(Look::Black, accent_for(Theme::Follow, orange));
        assert!(follow.contains("@define-color accent #e95420;"));
    }

    #[test]
    fn every_colour_name_the_style_uses_is_defined_in_every_look() {
        for look in [Look::Black, Look::Light] {
            for accent in [None, Some((10, 20, 30))] {
                let sheet = css(look, accent);
                let (defs, body) = sheet.split_at(sheet.find("/* ---------- overall").unwrap());
                let mut n = 0;
                for part in body.split('@').skip(1) {
                    let name: String = part.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    assert!(defs.contains(&format!("@define-color {name} ")), "{look:?}: {name} is not defined");
                    n += 1;
                }
                assert!(n > 50);
            }
        }
        // Both looks define the same names
        fn names<'a>(t: &[(&'a str, &str)]) -> Vec<&'a str> {
            t.iter().map(|e| e.0).collect()
        }
        assert_eq!(names(BLACK), names(LIGHT));
    }

    #[test]
    fn rules_other_features_rely_on_are_kept() {
        let body = css(Look::Black, None);
        for rule in [
            ".banner.banner-error",
            ".transfer-row",
            ".empty-view",
            ".tab:focus-visible",
            ".tab.drop-before",
            ".tab.dnd-hover",
            ".sidebar list row.pin-missing",
            ".dnd-hover",
            ".sidebar-eject",
            ".br-row.br-bad",
        ] {
            assert!(body.contains(rule), "{rule}");
        }
    }
}
