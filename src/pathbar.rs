//! Pure helpers for Tab completion in the path bar. No widgets here, so they can be tested.

use std::path::{Path, PathBuf};

/// How many directory entries to look at when completing, so a huge folder can't stall the listing.
pub const LISTING_CAP: usize = 5000;
/// How many candidates the popover shows before "…".
pub const MAX_SHOWN: usize = 12;

/// `~` and `~/…` become the home folder; anything else is returned as typed.
pub fn expand_tilde(text: &str, home: &Path) -> String {
    if text == "~" || text.starts_with("~/") {
        format!("{}{}", home.display(), &text[1..])
    } else {
        text.to_string()
    }
}

/// Does the text look like a local path we can complete? (Relative names and URIs are left alone.)
pub fn looks_like_path(text: &str) -> bool {
    text.starts_with('/') || text == "~" || text.starts_with("~/")
}

/// Splits typed text into the folder to list (as typed, ending in `/`) and the segment being completed.
/// `~` alone means "the home folder", so it completes to `~/`.
pub fn split_typed(text: &str) -> Option<(String, String)> {
    if !looks_like_path(text) {
        return None;
    }
    if text == "~" {
        return Some(("~/".into(), String::new()));
    }
    let i = text.rfind('/')?;
    Some((text[..=i].to_string(), text[i + 1..].to_string()))
}

/// The folder to list for a typed parent such as `~/Doc/` or `/usr/`.
pub fn parent_dir(parent_typed: &str, home: &Path) -> PathBuf {
    PathBuf::from(expand_tilde(parent_typed, home))
}

/// Names (from `names`) that complete `segment`: case-sensitive prefix first, and only if there
/// is none, case-insensitive. Hidden names only count when the segment starts with ".".
pub fn choose_matches(names: &[String], segment: &str) -> Vec<String> {
    let show_hidden = segment.starts_with('.');
    let visible = || names.iter().filter(|n| show_hidden || !n.starts_with('.'));
    let mut out: Vec<String> = visible().filter(|n| n.starts_with(segment)).cloned().collect();
    if out.is_empty() {
        let seg = segment.to_lowercase();
        out = visible().filter(|n| n.to_lowercase().starts_with(&seg)).cloned().collect();
    }
    out.sort();
    out.dedup();
    out
}

/// Longest common prefix, cut on character boundaries.
pub fn common_prefix(names: &[String]) -> String {
    let Some(first) = names.first() else { return String::new() };
    let mut end = first.len();
    for n in &names[1..] {
        // Length in bytes of the characters both strings share
        end = first[..end].chars().zip(n.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a.len_utf8()).sum();
    }
    first[..end].to_string()
}

/// The entry text after Tab: one match gets completed with a trailing "/", several complete to
/// their common prefix (never shorter than what was typed). None when nothing matches.
pub fn completed_text(parent_typed: &str, segment: &str, matches: &[String]) -> Option<String> {
    match matches {
        [] => None,
        [one] => Some(format!("{parent_typed}{one}/")),
        many => {
            let lcp = common_prefix(many);
            let seg = if lcp.chars().count() < segment.chars().count() { segment } else { &lcp };
            Some(format!("{parent_typed}{seg}"))
        }
    }
}

/// Sub-folder names of `dir`, at most `LISTING_CAP` entries looked at. Symlinks to folders count.
pub fn list_folders(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.take(LISTING_CAP)
        .flatten()
        .filter(|e| match e.file_type() {
            Ok(t) if t.is_dir() => true,
            Ok(t) if t.is_symlink() => std::fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false),
            _ => false,
        })
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

#[cfg(test)]
mod pathbar_tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tilde() {
        let h = Path::new("/home/u");
        assert_eq!(expand_tilde("~", h), "/home/u");
        assert_eq!(expand_tilde("~/Docs", h), "/home/u/Docs");
        assert_eq!(expand_tilde("~x/Docs", h), "~x/Docs");
        assert_eq!(expand_tilde("/a", h), "/a");
    }

    #[test]
    fn splitting() {
        assert_eq!(split_typed("/usr/li"), Some(("/usr/".into(), "li".into())));
        assert_eq!(split_typed("/usr/"), Some(("/usr/".into(), "".into())));
        assert_eq!(split_typed("/"), Some(("/".into(), "".into())));
        assert_eq!(split_typed("~/Do"), Some(("~/".into(), "Do".into())));
        assert_eq!(split_typed("~"), Some(("~/".into(), "".into())));
        assert_eq!(split_typed("~/a/b"), Some(("~/a/".into(), "b".into())));
        assert_eq!(split_typed("docs"), None);
        assert_eq!(split_typed("sftp://h/x"), None);
        assert_eq!(split_typed(""), None);
        assert_eq!(parent_dir("~/a/", Path::new("/home/u")), PathBuf::from("/home/u/a/"));
    }

    #[test]
    fn matching_case_rule() {
        let n = v(&["Docs", "docs2", "Downloads", "dog"]);
        assert_eq!(choose_matches(&n, "Do"), v(&["Docs", "Downloads"]));
        assert_eq!(choose_matches(&n, "do"), v(&["docs2", "dog"]));
        assert_eq!(choose_matches(&n, "DO"), v(&["Docs", "Downloads", "docs2", "dog"]));
        assert!(choose_matches(&n, "x").is_empty());
    }

    #[test]
    fn matching_hidden_rule() {
        let n = v(&[".cache", ".config", "code"]);
        assert_eq!(choose_matches(&n, ""), v(&["code"]));
        assert_eq!(choose_matches(&n, "c"), v(&["code"]));
        assert_eq!(choose_matches(&n, ".c"), v(&[".cache", ".config"]));
        assert_eq!(choose_matches(&n, "."), v(&[".cache", ".config"]));
    }

    #[test]
    fn prefix_on_char_boundaries() {
        assert_eq!(common_prefix(&v(&["abc", "abd"])), "ab");
        assert_eq!(common_prefix(&v(&["abc"])), "abc");
        assert_eq!(common_prefix(&v(&["ab", "abc"])), "ab");
        assert_eq!(common_prefix(&v(&["abc", "ab"])), "ab");
        assert_eq!(common_prefix(&v(&["x", "y"])), "");
        assert_eq!(common_prefix(&[]), "");
        // é and è share their first byte; the cut must not split a character
        assert_eq!(common_prefix(&v(&["caféa", "cafèb"])), "caf");
        assert_eq!(common_prefix(&v(&["日本語", "日本人"])), "日本");
        assert_eq!(common_prefix(&v(&["日本", "日本語", "日本人"])), "日本");
    }

    #[test]
    fn completed() {
        assert_eq!(completed_text("/a/", "Do", &v(&["Docs"])), Some("/a/Docs/".into()));
        assert_eq!(completed_text("/a/", "D", &v(&["Docs", "Downloads"])), Some("/a/Do".into()));
        // case-insensitive matches with differing case must not shorten what was typed
        assert_eq!(completed_text("/a/", "fo", &v(&["Foo", "fOo"])), Some("/a/fo".into()));
        assert_eq!(completed_text("/a/", "fo", &v(&["foo"])), Some("/a/foo/".into()));
        assert_eq!(completed_text("/a/", "é", &v(&["éa", "éb"])), Some("/a/é".into()));
        assert_eq!(completed_text("/a/", "x", &[]), None);
    }

    #[test]
    fn listing_only_folders() {
        let d = std::env::temp_dir().join(format!("rfiles-pathbar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("file"), b"x").unwrap();
        let mut got = list_folders(&d);
        got.sort();
        assert_eq!(got, v(&["sub"]));
        assert!(list_folders(&d.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
