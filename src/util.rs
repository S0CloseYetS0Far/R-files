use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;

thread_local! {
    /// Icon used for every folder (None = system theme icons).
    pub static FOLDER_ICON: RefCell<Option<gio::Icon>> = const { RefCell::new(None) };
    /// Size of icons in the grid view, in pixels.
    pub static ICON_SIZE: Cell<i32> = const { Cell::new(40) };
}

/// The built-in light-blue folder, drawn as a small, flat SVG.
pub const BLUE_FOLDER_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 64 64">
<path d="M5 15a4 4 0 0 1 4-4h15.5l5 5H55a4 4 0 0 1 4 4v28a4 4 0 0 1-4 4H9a4 4 0 0 1-4-4z" fill="#6ab4ee"/>
<path d="M5 23a4 4 0 0 1 4-4h46a4 4 0 0 1 4 4v25a4 4 0 0 1-4 4H9a4 4 0 0 1-4-4z" fill="#a9dcff"/>
<path d="M9 19h46a4 4 0 0 1 4 4v1.5H5V23a4 4 0 0 1 4-4z" fill="#d2eeff"/>
</svg>"##;

/// Writes the built-in icon to the cache folder and returns it as a GIcon.
pub fn blue_folder_icon() -> Option<gio::Icon> {
    let dir = glib::user_cache_dir().join("rfiles");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("folder-light-blue.svg");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(BLUE_FOLDER_SVG) {
        std::fs::write(&path, BLUE_FOLDER_SVG).ok()?;
    }
    Some(gio::FileIcon::new(&gio::File::for_path(path)).upcast())
}

/// Applies the "folder_icon" setting: "blue", "system" or "custom:<path>".
pub fn apply_folder_icon_setting(setting: &str) {
    let icon = if let Some(p) = setting.strip_prefix("custom:") {
        Some(gio::FileIcon::new(&gio::File::for_path(p)).upcast())
    } else if setting == "system" {
        None
    } else {
        blue_folder_icon()
    };
    FOLDER_ICON.with(|f| *f.borrow_mut() = icon);
}

/// File attributes we ask GIO for. Kept small on purpose: every extra
/// attribute costs memory for every file in every open folder.
pub const ATTRS: &str = "standard::name,standard::display-name,standard::icon,\
standard::type,standard::size,standard::content-type,standard::is-hidden,\
standard::is-backup,standard::target-uri,time::modified,thumbnail::path,trash::orig-path,\
metadata::custom-icon,thumbnail::is-valid";

/// The file an item refers to. Items in recent:/// or network:/// carry a
/// target-uri that points at the real file.
pub fn item_file(info: &gio::FileInfo) -> gio::File {
    if let Some(t) = info.attribute_string("standard::target-uri") {
        if !t.is_empty() {
            return gio::File::for_uri(&t);
        }
    }
    info.attribute_object("standard::file")
        .and_downcast::<gio::File>()
        .unwrap_or_else(|| gio::File::for_path("/"))
}

pub fn is_dir_like(info: &gio::FileInfo) -> bool {
    matches!(
        info.file_type(),
        gio::FileType::Directory | gio::FileType::Mountable | gio::FileType::Shortcut
    )
}

/// "file2" sorts before "file10", case-insensitive.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(&c) = ai.peek() {
                    if !c.is_ascii_digit() {
                        break;
                    }
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(&c) = bi.peek() {
                    if !c.is_ascii_digit() {
                        break;
                    }
                    nb.push(c);
                    bi.next();
                }
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = x.to_lowercase().cmp(y.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Folders first, then natural name order.
pub fn sort_infos(a: &glib::Object, b: &glib::Object) -> gtk::Ordering {
    let (Some(a), Some(b)) = (
        a.downcast_ref::<gio::FileInfo>(),
        b.downcast_ref::<gio::FileInfo>(),
    ) else {
        return gtk::Ordering::Equal;
    };
    let (ad, bd) = (is_dir_like(a), is_dir_like(b));
    if ad != bd {
        return if ad { gtk::Ordering::Smaller } else { gtk::Ordering::Larger };
    }
    natural_cmp(&a.display_name(), &b.display_name()).into()
}

/// Uses an existing system thumbnail when there is one (cheap, already on
/// disk), otherwise the themed icon.
pub fn set_icon(img: &gtk::Image, info: &gio::FileInfo, thumbs: bool) {
    // A per-file icon chosen with "Change Icon…"
    if let Some(ci) = info.attribute_string("metadata::custom-icon") {
        if !ci.is_empty() {
            let f = if ci.contains("://") { gio::File::for_uri(&ci) } else { gio::File::for_path(ci.as_str()) };
            img.set_from_gicon(&gio::FileIcon::new(&f));
            return;
        }
    }
    if info.file_type() == gio::FileType::Directory {
        if let Some(i) = FOLDER_ICON.with(|f| f.borrow().clone()) {
            img.set_from_gicon(&i);
            return;
        }
    }
    if thumbs {
        if let Some(p) = info.attribute_byte_string("thumbnail::path") {
            if !p.is_empty() {
                img.set_from_file(Some(p.as_str()));
                return;
            }
        }
    }
    match info.icon() {
        Some(i) => img.set_from_gicon(&i),
        None => img.set_icon_name(Some("text-x-generic")),
    }
}

pub fn format_time(info: &gio::FileInfo) -> String {
    info.modification_date_time()
        .and_then(|d| d.to_local().ok())
        .and_then(|d| d.format("%Y-%m-%d  %H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

pub fn format_size(info: &gio::FileInfo) -> String {
    if is_dir_like(info) {
        String::new()
    } else {
        glib::format_size(info.size().max(0) as u64).to_string()
    }
}

pub fn type_description(info: &gio::FileInfo) -> String {
    if info.file_type() == gio::FileType::Directory {
        return "Folder".into();
    }
    match info.content_type() {
        Some(ct) => gio::content_type_get_description(&ct).to_string(),
        None => String::new(),
    }
}

pub fn home() -> gio::File {
    gio::File::for_path(glib::home_dir())
}

/// Path for display: "~/Pictures" instead of "/home/name/Pictures".
pub fn display_path(f: &gio::File) -> String {
    if let Some(p) = f.path() {
        if let Ok(rest) = p.strip_prefix(glib::home_dir()) {
            return if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    f.parse_name().to_string()
}

pub fn file_title(f: &gio::File) -> String {
    if let Some(p) = f.path() {
        if p == glib::home_dir() {
            return "Home".into();
        }
        if p == std::path::Path::new("/") {
            return "Computer".into();
        }
    }
    let uri = f.uri();
    match uri.trim_end_matches('/') {
        "recent:" => return "Recent".into(),
        "trash:" => return "Trash".into(),
        "network:" => return "Network".into(),
        _ => {}
    }
    f.basename()
        .map(|b| b.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && s != "/")
        .unwrap_or_else(|| uri.to_string())
}

pub fn icon_for_file(f: &gio::File) -> &'static str {
    if let Some(p) = f.path() {
        if p == glib::home_dir() {
            return "user-home-symbolic";
        }
        if p == std::path::Path::new("/") {
            return "drive-harddisk-symbolic";
        }
    }
    let uri = f.uri();
    match uri.trim_end_matches('/') {
        "recent:" => "document-open-recent-symbolic",
        "trash:" => "user-trash-symbolic",
        "network:" => "network-workgroup-symbolic",
        _ => "folder-symbolic",
    }
}

pub fn is_trash(f: &gio::File) -> bool {
    f.uri().starts_with("trash:")
}

/// Build a list of FileInfos for an arbitrary set of files (Starred,
/// search results). The "standard::file" attribute is set the same way
/// GtkDirectoryList does it, so the rest of the code treats them alike.
pub fn infos_store(files: impl Iterator<Item = gio::File>) -> gio::ListStore {
    let store = gio::ListStore::new::<gio::FileInfo>();
    for f in files {
        if let Ok(info) = f.query_info(ATTRS, gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>) {
            info.set_attribute_object("standard::file", &f);
            store.append(&info);
        }
    }
    store
}
