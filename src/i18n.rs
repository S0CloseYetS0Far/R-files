//! Translations (gettext) and locale-aware numbers. Strings go through `tr` and `ntr`; the catalogue is
//! `rfiles.mo` in the usual locale folders (see po/README.md). With no catalogue the English text is used as is.

use gtk::glib;
use std::path::{Path, PathBuf};

const DOMAIN: &str = "rfiles";

extern "C" {
    // glibc: the folder holding <lang>/LC_MESSAGES/<domain>.mo
    fn bindtextdomain(domain: *const libc::c_char, dir: *const libc::c_char) -> *mut libc::c_char;
    fn bind_textdomain_codeset(domain: *const libc::c_char, codeset: *const libc::c_char) -> *mut libc::c_char;
}

/// Translates `msgid` (the English text) into the user's language.
pub fn tr(msgid: &str) -> String {
    glib::dgettext(Some(DOMAIN), msgid).to_string()
}

/// Translates a text with a count: `singular` when `n` is 1 (or whatever the language says), else `plural`.
/// "{n}" in the result is replaced by the number.
pub fn ntr(n: usize, singular: &str, plural: &str) -> String {
    glib::dngettext(Some(DOMAIN), singular, plural, n as libc::c_ulong).replace("{n}", &n.to_string())
}

/// Where translations are looked up: the user's own folder when it holds an RFiles catalogue, else the system's.
fn locale_dir(user: &Path, system: &Path) -> PathBuf {
    let has_catalogue = std::fs::read_dir(user).is_ok_and(|rd| {
        rd.flatten()
            .any(|e| e.path().join("LC_MESSAGES").join(format!("{DOMAIN}.mo")).is_file())
    });
    if has_catalogue {
        user.to_path_buf()
    } else {
        system.to_path_buf()
    }
}

/// Points gettext at the catalogue folder. Call once at start-up.
pub fn init() {
    let user = glib::user_data_dir().join("locale");
    let dir = locale_dir(&user, Path::new("/usr/share/locale"));
    let (Ok(domain), Ok(dir), Ok(codeset)) = (
        std::ffi::CString::new(DOMAIN),
        std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()),
        std::ffi::CString::new("UTF-8"),
    ) else {
        return;
    };
    // SAFETY: valid NUL-terminated strings; glibc copies them
    unsafe {
        bindtextdomain(domain.as_ptr(), dir.as_ptr());
        bind_textdomain_codeset(domain.as_ptr(), codeset.as_ptr());
    }
}

/// The locale's thousands separator, read once. A locale without one ("C") falls back to ",".
pub fn thousands_separator() -> &'static str {
    static SEP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SEP.get_or_init(|| {
        // SAFETY: localeconv returns a pointer to static data; read right away
        let sep = unsafe {
            let lc = libc::localeconv();
            if lc.is_null() || (*lc).thousands_sep.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr((*lc).thousands_sep)
                    .to_string_lossy()
                    .into_owned()
            }
        };
        if sep.is_empty() {
            ",".to_string()
        } else {
            sep
        }
    })
}

/// The display name of a special folder in the sidebar: its real name on disk, so a German system shows
/// "Dokumente"; `fallback` when the path has no name.
pub fn special_dir_name(path: &Path, fallback: &str) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| fallback.into())
}

#[cfg(test)]
mod i18n_tests {
    use super::*;

    #[test]
    fn special_folder_uses_the_real_name() {
        assert_eq!(
            special_dir_name(Path::new("/home/a/Dokumente"), "Documents"),
            "Dokumente"
        );
        assert_eq!(
            special_dir_name(Path::new("/home/a/Téléchargements/"), "Downloads"),
            "Téléchargements"
        );
        assert_eq!(special_dir_name(Path::new("/"), "Documents"), "Documents");
    }

    #[test]
    fn without_a_catalogue_english_is_used_and_plurals_follow_the_count() {
        assert_eq!(tr("Nothing translated this"), "Nothing translated this");
        assert_eq!(ntr(1, "Moved {n} item", "Moved {n} items"), "Moved 1 item");
        assert_eq!(ntr(0, "Moved {n} item", "Moved {n} items"), "Moved 0 items");
        assert_eq!(ntr(5, "Moved {n} item", "Moved {n} items"), "Moved 5 items");
    }

    #[test]
    fn user_folder_wins_only_with_a_catalogue() {
        let base = std::env::temp_dir().join(format!("rfiles-i18n-{}", std::process::id()));
        let user = base.join("user");
        let system = base.join("system");
        std::fs::create_dir_all(user.join("de").join("LC_MESSAGES")).unwrap();
        assert_eq!(locale_dir(&user, &system), system);
        std::fs::write(user.join("de").join("LC_MESSAGES").join("rfiles.mo"), b"x").unwrap();
        assert_eq!(locale_dir(&user, &system), user);
        assert_eq!(locale_dir(&base.join("missing"), &system), system);
        let _ = std::fs::remove_dir_all(&base);
    }
}
