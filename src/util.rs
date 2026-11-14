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

/// "file2" sorts before "file10", case-insensitive. Equal only for equal strings:
/// names that differ only in case or leading zeros fall back to byte order.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut i, mut j) = (0, 0);
    loop {
        let (x, y) = match (a[i..].chars().next(), b[j..].chars().next()) {
            (None, None) => return a.cmp(b),
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) => (x, y),
        };
        if x.is_ascii_digit() && y.is_ascii_digit() {
            // Compare runs of digits by value, on slices: no copies.
            let ea = i + a[i..].bytes().take_while(u8::is_ascii_digit).count();
            let eb = j + b[j..].bytes().take_while(u8::is_ascii_digit).count();
            let ta = a[i..ea].trim_start_matches('0');
            let tb = b[j..eb].trim_start_matches('0');
            let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
            if o != Ordering::Equal {
                return o;
            }
            (i, j) = (ea, eb);
        } else {
            let o = x.to_lowercase().cmp(y.to_lowercase());
            if o != Ordering::Equal {
                return o;
            }
            i += x.len_utf8();
            j += y.len_utf8();
        }
    }
}

/// The locale's file-name collation key (accents, other scripts, "file2" before "file10"),
/// built once per item and kept on the FileInfo, so a big sort does not rebuild it on every
/// comparison. A rename or reload gives a new FileInfo (GtkDirectoryList, `infos_store`),
/// so a cached key never outlives the name it was made from.
fn name_key(info: &gio::FileInfo) -> std::ptr::NonNull<glib::FilenameCollationKey> {
    static QUARK: std::sync::OnceLock<glib::Quark> = std::sync::OnceLock::new();
    let q = *QUARK.get_or_init(|| glib::Quark::from_str("rfiles-name-key"));
    // SAFETY: only this function uses this quark, and it always stores this type.
    unsafe {
        if let Some(k) = info.qdata::<glib::FilenameCollationKey>(q) {
            return k;
        }
        info.set_qdata(q, glib::FilenameCollationKey::from(info.display_name()));
        info.qdata(q).expect("key was just set")
    }
}

/// Folders first, then the locale's file-name order. Names the collation ranks
/// the same fall back to the raw names, so the order is total.
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
    // SAFETY: nothing replaces the keys while `a` and `b` are borrowed here.
    let o = unsafe { name_key(a).as_ref().cmp(name_key(b).as_ref()) };
    o.then_with(|| a.display_name().cmp(&b.display_name())).then_with(|| a.name().cmp(&b.name())).into()
}

/// What the file views are sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Type,
}

impl SortKey {
    pub const ALL: [SortKey; 4] = [SortKey::Name, SortKey::Size, SortKey::Modified, SortKey::Type];

    /// Name in the settings file, the list view column id and the menu action target.
    pub fn as_str(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Type => "type",
        }
    }

    pub fn parse(s: &str) -> Option<SortKey> {
        SortKey::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// The type description shown in the Type column, kept on the FileInfo like `name_key`
/// because looking it up on every comparison would make big sorts slow.
fn type_key(info: &gio::FileInfo) -> std::ptr::NonNull<String> {
    static QUARK: std::sync::OnceLock<glib::Quark> = std::sync::OnceLock::new();
    let q = *QUARK.get_or_init(|| glib::Quark::from_str("rfiles-type-key"));
    // SAFETY: only this function uses this quark, and it always stores this type.
    unsafe {
        if let Some(k) = info.qdata::<String>(q) {
            return k;
        }
        let d = if info.has_attribute("standard::content-type") { type_description(info) } else { String::new() };
        info.set_qdata(q, d);
        info.qdata(q).expect("key was just set")
    }
}

/// Unset attributes are checked first: GIO logs a critical when asked for one.
fn size_of(info: &gio::FileInfo) -> Option<i64> {
    info.has_attribute("standard::size").then(|| info.size())
}

fn modified_of(info: &gio::FileInfo) -> Option<(u64, u32)> {
    info.has_attribute("time::modified").then(|| {
        let usec = if info.has_attribute("time::modified-usec") { info.attribute_uint32("time::modified-usec") } else { 0 };
        (info.attribute_uint64("time::modified"), usec)
    })
}

/// Folders first in every mode, then `key`, then the name order of `sort_infos`, so the
/// order is total. Folders have no known size, so Size and Type keep them in name order.
/// Missing sizes and times sort before known ones. `desc` reverses files and folders
/// but keeps folders first.
pub fn compare_infos(a: &gio::FileInfo, b: &gio::FileInfo, key: SortKey, desc: bool) -> Ordering {
    let (ad, bd) = (is_dir_like(a), is_dir_like(b));
    if ad != bd {
        return if ad { Ordering::Less } else { Ordering::Greater };
    }
    let o = match key {
        SortKey::Name => Ordering::Equal,
        SortKey::Size | SortKey::Type if ad => Ordering::Equal,
        SortKey::Size => size_of(a).cmp(&size_of(b)),
        SortKey::Modified => modified_of(a).cmp(&modified_of(b)),
        // SAFETY: nothing replaces the keys while `a` and `b` are borrowed here.
        SortKey::Type => unsafe { natural_cmp(type_key(a).as_ref(), type_key(b).as_ref()) },
    };
    let o = o.then_with(|| sort_infos(a.upcast_ref(), b.upcast_ref()).into());
    if desc {
        o.reverse()
    } else {
        o
    }
}

#[cfg(test)]
mod compare_infos_tests {
    use super::*;

    struct F {
        name: &'static str,
        dir: bool,
        size: Option<i64>,
        mtime: Option<u64>,
        ctype: Option<&'static str>,
    }

    fn f(name: &'static str, size: Option<i64>, mtime: Option<u64>, ctype: Option<&'static str>) -> F {
        F { name, dir: false, size, mtime, ctype }
    }

    fn d(name: &'static str, mtime: Option<u64>) -> F {
        F { name, dir: true, size: None, mtime, ctype: None }
    }

    fn make(x: &F) -> gio::FileInfo {
        let i = gio::FileInfo::new();
        i.set_name(x.name);
        i.set_display_name(x.name);
        i.set_file_type(if x.dir { gio::FileType::Directory } else { gio::FileType::Regular });
        if let Some(s) = x.size {
            i.set_size(s);
        }
        if let Some(t) = x.mtime {
            i.set_attribute_uint64("time::modified", t);
        }
        if let Some(c) = x.ctype {
            i.set_content_type(c);
        }
        i
    }

    fn sorted(files: &[F], key: SortKey, desc: bool) -> Vec<&'static str> {
        let mut v: Vec<(gio::FileInfo, &'static str)> = files.iter().map(|x| (make(x), x.name)).collect();
        v.sort_by(|a, b| compare_infos(&a.0, &b.0, key, desc));
        v.into_iter().map(|(_, n)| n).collect()
    }

    fn sample() -> Vec<F> {
        vec![
            f("b.txt", Some(300), Some(20), Some("text/plain")),
            d("zdir", Some(5)),
            f("a.png", Some(100), Some(30), Some("image/png")),
            f("c.txt", Some(100), Some(10), Some("text/plain")),
            d("adir", Some(50)),
            f("nothing", None, None, None),
        ]
    }

    #[test]
    fn key_parse_round_trip() {
        for k in SortKey::ALL {
            assert_eq!(SortKey::parse(k.as_str()), Some(k));
        }
        assert_eq!(SortKey::parse("colour"), None);
        assert_eq!(SortKey::parse(""), None);
    }

    #[test]
    fn by_name() {
        assert_eq!(sorted(&sample(), SortKey::Name, false), ["adir", "zdir", "a.png", "b.txt", "c.txt", "nothing"]);
        assert_eq!(sorted(&sample(), SortKey::Name, true), ["zdir", "adir", "nothing", "c.txt", "b.txt", "a.png"]);
    }

    #[test]
    fn by_size_ties_and_missing() {
        // Missing size first; equal sizes (a.png, c.txt) in name order; folders by name.
        assert_eq!(sorted(&sample(), SortKey::Size, false), ["adir", "zdir", "nothing", "a.png", "c.txt", "b.txt"]);
        assert_eq!(sorted(&sample(), SortKey::Size, true), ["zdir", "adir", "b.txt", "c.txt", "a.png", "nothing"]);
    }

    #[test]
    fn by_modified_sorts_folders_too() {
        assert_eq!(sorted(&sample(), SortKey::Modified, false), ["zdir", "adir", "nothing", "c.txt", "b.txt", "a.png"]);
        assert_eq!(sorted(&sample(), SortKey::Modified, true), ["adir", "zdir", "a.png", "b.txt", "c.txt", "nothing"]);
    }

    #[test]
    fn by_modified_uses_microseconds() {
        let a = make(&f("a", None, Some(7), None));
        let b = make(&f("b", None, Some(7), None));
        a.set_attribute_uint32("time::modified-usec", 900);
        b.set_attribute_uint32("time::modified-usec", 100);
        assert_eq!(compare_infos(&a, &b, SortKey::Modified, false), Ordering::Greater);
    }

    #[test]
    fn by_type() {
        let png = gio::content_type_get_description("image/png").to_string();
        let txt = gio::content_type_get_description("text/plain").to_string();
        // No content type has an empty description, so it comes first.
        let mut want = vec!["nothing"];
        if natural_cmp(&png, &txt) != Ordering::Greater {
            want.extend(["a.png", "b.txt", "c.txt"]);
        } else {
            want.extend(["b.txt", "c.txt", "a.png"]);
        }
        let mut all = vec!["adir", "zdir"];
        all.extend(&want);
        assert_eq!(sorted(&sample(), SortKey::Type, false), all);
        let mut rev = vec!["zdir", "adir"];
        rev.extend(want.iter().rev());
        assert_eq!(sorted(&sample(), SortKey::Type, true), rev);
    }

    #[test]
    fn folders_first_and_total_in_every_mode() {
        let s = sample();
        let infos: Vec<gio::FileInfo> = s.iter().map(make).collect();
        for key in SortKey::ALL {
            for desc in [false, true] {
                for (x, a) in s.iter().zip(&infos) {
                    for (y, b) in s.iter().zip(&infos) {
                        let o = compare_infos(a, b, key, desc);
                        assert_eq!(o == Ordering::Equal, x.name == y.name, "{} vs {} {key:?}", x.name, y.name);
                        assert_eq!(o, compare_infos(b, a, key, desc).reverse());
                        if x.dir && !y.dir {
                            assert_eq!(o, Ordering::Less, "{} vs {} {key:?} {desc}", x.name, y.name);
                        }
                    }
                }
            }
        }
    }
}

/// With `thumbs`, shows the file's thumbnail if it was decoded recently;
/// otherwise the themed icon, and `thumbs::bind` loads the thumbnail off the
/// UI thread.
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
        if let Some(tex) = crate::thumbs::cached(info) {
            img.set_paintable(Some(&tex));
            return;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The app gets the user's locale from gtk_init; tests have to ask for one. Returns false
    /// when no English UTF-8 locale is installed, so locale-dependent checks can be skipped.
    fn english_locale() -> bool {
        extern "C" {
            fn setlocale(category: i32, locale: *const std::ffi::c_char) -> *mut std::ffi::c_char;
        }
        const LC_ALL: i32 = 6; // glibc
        static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OK.get_or_init(|| {
            ["en_US.UTF-8", "en_US.utf8", "en_GB.UTF-8", "en_GB.utf8"].iter().any(|l| {
                let l = std::ffi::CString::new(*l).unwrap();
                // SAFETY: a valid C string, and setlocale copies what it needs.
                !unsafe { setlocale(LC_ALL, l.as_ptr()) }.is_null()
            })
        })
    }

    fn info(name: &str, dir: bool) -> glib::Object {
        let i = gio::FileInfo::new();
        i.set_name(name);
        i.set_display_name(name);
        i.set_file_type(if dir { gio::FileType::Directory } else { gio::FileType::Regular });
        i.upcast()
    }

    /// Sorts the names with `sort_infos`; names ending in "/" are folders.
    fn sorted(names: &[&str]) -> Vec<String> {
        let mut v: Vec<glib::Object> =
            names.iter().map(|n| info(n.trim_end_matches('/'), n.ends_with('/'))).collect();
        v.sort_by(|a, b| sort_infos(a, b).into());
        v.iter().map(|o| o.downcast_ref::<gio::FileInfo>().unwrap().display_name().to_string()).collect()
    }

    #[test]
    fn natural_cmp_numbers_case_and_ties() {
        assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("file10", "file2"), Ordering::Greater);
        assert_eq!(natural_cmp("File2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("abc", "abcd"), Ordering::Less);
        assert_eq!(natural_cmp("x", "x"), Ordering::Equal);
        // Different names are never Equal.
        assert_ne!(natural_cmp("a01", "a1"), Ordering::Equal);
        assert_eq!(natural_cmp("a01", "a1"), natural_cmp("a1", "a01").reverse());
        assert_ne!(natural_cmp("Abc", "abc"), Ordering::Equal);
        assert_eq!(natural_cmp("Abc", "abc"), natural_cmp("abc", "Abc").reverse());
        let mut parts = vec!["slide10.xml", "slide2.xml", "slide1.xml"];
        parts.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(parts, ["slide1.xml", "slide2.xml", "slide10.xml"]);
    }

    #[test]
    fn sort_folders_first_and_numbers() {
        english_locale();
        assert_eq!(sorted(&["file10", "b/", "file2", "a/", "file1"]), ["a", "b", "file1", "file2", "file10"]);
        assert_eq!(sorted(&["zz", "dir10/", "dir9/"]), ["dir9", "dir10", "zz"]);
    }

    #[test]
    fn sort_accents_and_case_follow_the_locale() {
        if !english_locale() {
            eprintln!("no English UTF-8 locale installed; skipping locale order checks");
            return;
        }
        assert_eq!(sorted(&["zebra", "école", "apple"]), ["apple", "école", "zebra"]);
        assert_eq!(sorted(&["Zoo", "Ångström", "Bar"]), ["Ångström", "Bar", "Zoo"]);
        assert_eq!(sorted(&["cherry", "Banana", "apple"]), ["apple", "Banana", "cherry"]);
    }

    #[test]
    fn sort_is_total() {
        english_locale();
        let names = ["a01", "a1", "Apple", "apple", "APPLE", "é", "e\u{301}"];
        for x in names {
            for y in names {
                let (a, b) = (info(x, false), info(y, false));
                let o: Ordering = sort_infos(&a, &b).into();
                assert_eq!(o == Ordering::Equal, x == y, "{x:?} vs {y:?}");
                assert_eq!(o, Ordering::from(sort_infos(&b, &a)).reverse(), "{x:?} vs {y:?}");
            }
        }
    }
}

/// Splits "photo.jpg" into ("photo", ".jpg"). Hidden files without another
/// dot (".bashrc") have no extension; ".tar.*" is kept as one extension.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => {
            let stem = &name[..i];
            if let Some(j) = stem.len().checked_sub(4) {
                if j > 0 && stem.is_char_boundary(j) && stem[j..].eq_ignore_ascii_case(".tar") {
                    return (&name[..j], &name[j..]);
                }
            }
            (stem, &name[i..])
        }
        _ => (name, ""),
    }
}

/// How many characters of `name` the rename box pre-selects: the stem for a file (as `split_ext` sees it), the
/// whole name for a folder. Counted in characters because GTK entry positions are.
pub fn select_len(name: &str, is_dir: bool) -> usize {
    if is_dir { name.chars().count() } else { split_ext(name).0.chars().count() }
}

#[cfg(test)]
mod select_len_tests {
    use super::select_len;

    #[test]
    fn counts_characters() {
        assert_eq!(select_len("photo.jpg", false), 5);
        assert_eq!(select_len("fotó-é.jpg", false), 6);
        assert_eq!(select_len("archive.tar.gz", false), 7);
        assert_eq!(select_len(".bashrc", false), 7);
        assert_eq!(select_len("noext", false), 5);
        assert_eq!(select_len("my.folder.d", true), 11);
        assert_eq!(select_len("Ünï", true), 3);
    }
}

