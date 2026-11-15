use super::*;

impl App {
    // ================= search =================
    pub(super) fn run_search(self: &Rc<Self>, root: gio::File, query: String) {
        let Some(path) = root.path() else {
            self.status.set_text("Deep search only works on local folders");
            return;
        };
        let g = self.search_gen.get() + 1;
        self.search_gen.set(g);
        self.cancel_search();
        let cancel = Arc::new(AtomicBool::new(false));
        *self.search_cancel.borrow_mut() = Some(cancel.clone());
        self.status.set_text(&format!("Searching for “{query}” in {}…", path.display()));
        let hidden = self.show_hidden.get();
        let content = self.search_content.is_active();
        let a = Rc::downgrade(self);
        let q = query.to_lowercase();
        *self.content_query.borrow_mut() = content.then(|| q.clone());
        // The worker bumps these counters; a timer shows them at most 5 times a second
        let progress = Arc::new(SearchProgress::default());
        let (p, w) = (progress.clone(), Rc::downgrade(self));
        glib::timeout_add_local(std::time::Duration::from_millis(200), move || match w.upgrade() {
            Some(a) if a.search_gen.get() == g && a.search_cancel.borrow().is_some() => {
                let (checked, found) = (p.checked.load(AtomicOrdering::Relaxed), p.found.load(AtomicOrdering::Relaxed));
                a.status.set_text(&search_progress_text(checked, found));
                glib::ControlFlow::Continue
            }
            _ => glib::ControlFlow::Break,
        });
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || {
                let lim = SearchLimits { max_results: SEARCH_MAX_RESULTS, max_visited: SEARCH_MAX_VISITED, content };
                search_walk_ex(&path, &q, hidden, &cancel, lim, &progress)
            })
            .await;
            // The window may have closed while the walk ran
            let Some(a) = a.upgrade() else { return };
            if a.search_gen.get() != g {
                return;
            }
            a.search_cancel.borrow_mut().take();
            if let Ok((results, end)) = res {
                if end != SearchEnd::Cancelled {
                    a.navigate(Loc::Search { root, query, results: Rc::new(results), end });
                }
            }
        });
    }

    /// Stops the running deep search walk, if any. Returns whether one was running.
    pub(super) fn cancel_search(&self) -> bool {
        match self.search_cancel.borrow_mut().take() {
            Some(c) => {
                c.store(true, AtomicOrdering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// The user stopped a deep search (Escape or an emptied search box).
    pub(super) fn stop_search_by_user(&self) {
        if self.cancel_search() {
            // Its result is ignored even if the walk already finished
            self.search_gen.set(self.search_gen.get() + 1);
            self.status.set_text("Search stopped");
        }
    }
}

/// Counters the search worker bumps and the UI thread reads.
#[derive(Default)]
struct SearchProgress {
    checked: AtomicUsize,
    found: AtomicUsize,
}

#[derive(Clone, Copy)]
struct SearchLimits {
    max_results: usize,
    max_visited: usize,
    /// Also match files whose content holds the query
    content: bool,
}

/// Status bar text while a deep search runs.
fn search_progress_text(checked: usize, found: usize) -> String {
    use crate::properties::group_digits;
    let files = if checked == 1 { "file" } else { "files" };
    format!("Searching… {} {files} checked, {} found", group_digits(checked as u64), group_digits(found as u64))
}

/// Recursive name search, run on a worker thread. Stops once `cancel` is set or a limit is reached, and
/// says which.
#[cfg(test)]
fn search_walk(
    root: &Path,
    q: &str,
    hidden: bool,
    cancel: &AtomicBool,
    max_results: usize,
    max_visited: usize,
) -> (Vec<PathBuf>, SearchEnd) {
    let lim = SearchLimits { max_results, max_visited, content: false };
    search_walk_ex(root, q, hidden, cancel, lim, &SearchProgress::default())
}

fn search_walk_ex(
    root: &Path,
    q: &str,
    hidden: bool,
    cancel: &AtomicBool,
    lim: SearchLimits,
    progress: &SearchProgress,
) -> (Vec<PathBuf>, SearchEnd) {
    let (max_results, max_visited) = (lim.max_results, lim.max_visited);
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    let skip: &[&str] = &["/proc", "/sys", "/dev", "/run", "/snap"];
    while let Some(dir) = stack.pop() {
        if cancel.load(AtomicOrdering::Relaxed) {
            return (out, SearchEnd::Cancelled);
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            // Checked per entry too: one folder can hold a huge number of files
            if cancel.load(AtomicOrdering::Relaxed) {
                return (out, SearchEnd::Cancelled);
            }
            visited += 1;
            progress.checked.store(visited, AtomicOrdering::Relaxed);
            if visited > max_visited {
                return (out, SearchEnd::VisitedLimit);
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !hidden && name.starts_with('.') {
                continue;
            }
            let path = e.path();
            let ft = e.file_type().ok();
            // A name match wins; only plain files (not links, pipes or devices) get their content read
            let hit = name.to_lowercase().contains(q)
                || (lim.content && ft.is_some_and(|t| t.is_file()) && content_matches(&path, q, cancel));
            if hit {
                // Only a match past the limit means results were left out
                if out.len() >= max_results {
                    return (out, SearchEnd::ResultLimit);
                }
                out.push(path.clone());
                progress.found.store(out.len(), AtomicOrdering::Relaxed);
            }
            if ft.is_some_and(|t| t.is_dir()) && !skip.iter().any(|s| path == Path::new(s)) {
                stack.push(path);
            }
        }
    }
    (out, SearchEnd::Done)
}

/// Biggest plain text file whose content is searched.
const CONTENT_MAX_TEXT: u64 = 2 * 1024 * 1024;
/// Biggest office document opened for its text (they are zip files, so the extractor unpacks them).
const CONTENT_MAX_DOC: u64 = 32 * 1024 * 1024;

/// Whether the file's content holds `q` (already lower-case). Text files are read in chunks so `cancel`
/// is noticed inside a big file. PDFs are not searched: that needs poppler.
fn content_matches(path: &Path, q: &str, cancel: &AtomicBool) -> bool {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    if q.is_empty() {
        return false;
    }
    // O_NONBLOCK so a pipe swapped in after the directory listing can't block this thread
    let Ok(mut f) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path) else {
        return false;
    };
    let Ok(meta) = f.metadata() else { return false };
    if !meta.is_file() {
        return false;
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if crate::docs::is_office(&ext) {
        if meta.len() > CONTENT_MAX_DOC || cancel.load(AtomicOrdering::Relaxed) {
            return false;
        }
        drop(f);
        return crate::docs::extract_text(path).is_ok_and(|t| t.to_lowercase().contains(q));
    }
    if meta.len() > CONTENT_MAX_TEXT {
        return false;
    }
    let keep = q.chars().count().saturating_sub(1);
    let mut buf = vec![0u8; 64 * 1024];
    let mut pending: Vec<u8> = Vec::new(); // an unfinished UTF-8 sequence at the end of the last chunk
    let mut tail = String::new(); // the end of the text so far, in case the query spans two chunks
    let mut first = true;
    let mut total = 0u64;
    loop {
        if cancel.load(AtomicOrdering::Relaxed) {
            return false;
        }
        let n = match f.read(&mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        total += n as u64;
        // Same test as the preview: a NUL in the first 8 KiB means binary
        if first && buf[..n.min(8192)].contains(&0) {
            return false;
        }
        first = false;
        pending.extend_from_slice(&buf[..n]);
        let cut = match std::str::from_utf8(&pending) {
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            _ => pending.len(),
        };
        let text = format!("{tail}{}", String::from_utf8_lossy(&pending[..cut]).to_lowercase());
        pending.drain(..cut);
        if text.contains(q) {
            return true;
        }
        tail = text.chars().rev().take(keep).collect::<Vec<_>>().into_iter().rev().collect();
        if total > CONTENT_MAX_TEXT {
            return false;
        }
    }
}

#[cfg(test)]
mod content_search_tests {
    use super::*;
    use std::io::Write;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rfiles-content-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn matches_text_case_insensitively() {
        let d = tmp("text");
        let no = AtomicBool::new(false);
        let p = put(&d, "a.txt", "Hello Wörld\nsecond LINE".as_bytes());
        assert!(content_matches(&p, "hello", &no));
        assert!(content_matches(&p, "wörld", &no));
        assert!(content_matches(&p, "d\nsecond", &no));
        assert!(!content_matches(&p, "absent", &no));
        assert!(!content_matches(&p, "", &no));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn matches_across_chunk_edges() {
        let d = tmp("chunk");
        let no = AtomicBool::new(false);
        // The needle straddles the 64 KiB chunk boundary, after multi-byte characters
        let mut data = "é".repeat(32 * 1024 - 2).into_bytes();
        data.extend_from_slice(b"needle tail");
        let p = put(&d, "big.txt", &data);
        assert!(content_matches(&p, "needle", &no));
        assert!(content_matches(&p, "é needle", &no) || content_matches(&p, "éneedle", &no));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_binary_and_big_files() {
        let d = tmp("bin");
        let no = AtomicBool::new(false);
        let p = put(&d, "b.bin", b"abc\0needle");
        assert!(!content_matches(&p, "needle", &no));
        let mut big = vec![b'x'; CONTENT_MAX_TEXT as usize];
        big.extend_from_slice(b"needle");
        let p = put(&d, "big.txt", &big);
        assert!(!content_matches(&p, "needle", &no));
        assert!(!content_matches(&d.join("missing"), "x", &no));
        assert!(!content_matches(&d, "x", &no)); // a folder isn't a file
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reads_office_documents() {
        let d = tmp("doc");
        let no = AtomicBool::new(false);
        let p = d.join("r.docx");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
        z.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"<w:document><w:p><w:r><w:t>Quarterly Budget</w:t></w:r></w:p></w:document>").unwrap();
        z.finish().unwrap();
        assert!(content_matches(&p, "quarterly budget", &no));
        assert!(!content_matches(&p, "absent", &no));
        assert!(!content_matches(&p, "quarterly budget", &AtomicBool::new(true)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cancel_stops_reading() {
        let d = tmp("cancel");
        let p = put(&d, "a.txt", b"needle");
        assert!(content_matches(&p, "needle", &AtomicBool::new(false)));
        assert!(!content_matches(&p, "needle", &AtomicBool::new(true)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn walk_finds_by_content_only_when_asked() {
        let d = tmp("walk");
        put(&d, "notes.txt", b"the Zebra crossing");
        put(&d, "other.txt", b"nothing");
        let no = AtomicBool::new(false);
        let mut lim = SearchLimits { max_results: 10, max_visited: 100, content: false };
        let pr = SearchProgress::default();
        assert!(search_walk_ex(&d, "zebra", false, &no, lim, &pr).0.is_empty());
        lim.content = true;
        let (found, end) = search_walk_ex(&d, "zebra", false, &no, lim, &pr);
        assert_eq!((found, end), (vec![d.join("notes.txt")], SearchEnd::Done));
        assert_eq!((pr.checked.load(AtomicOrdering::Relaxed), pr.found.load(AtomicOrdering::Relaxed)), (2, 1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn progress_text() {
        assert_eq!(search_progress_text(12_400, 7), "Searching… 12,400 files checked, 7 found");
        assert_eq!(search_progress_text(1, 0), "Searching… 1 file checked, 0 found");
    }
}

#[cfg(test)]
mod tests {
    use super::{search_walk, SearchEnd};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    /// root/{Alpha.txt, beta.txt, .alpha-hidden, sub/{alpha2, deep/ALPHA3}, .hid/alpha4}
    fn search_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rfiles-search-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::create_dir_all(root.join(".hid")).unwrap();
        for f in ["Alpha.txt", "beta.txt", ".alpha-hidden", "sub/alpha2", "sub/deep/ALPHA3", ".hid/alpha4"] {
            std::fs::write(root.join(f), b"").unwrap();
        }
        root
    }

    fn names(found: &[PathBuf], root: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> =
            found.iter().map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn search_walk_matches_and_hidden() {
        let root = search_tree("match");
        let no = AtomicBool::new(false);
        let (found, end) = search_walk(&root, "alpha", false, &no, 100, 100);
        assert_eq!(end, SearchEnd::Done);
        assert_eq!(names(&found, &root), ["Alpha.txt", "sub/alpha2", "sub/deep/ALPHA3"]);
        let (found, end) = search_walk(&root, "alpha", true, &no, 100, 100);
        assert_eq!(end, SearchEnd::Done);
        assert_eq!(
            names(&found, &root),
            [".alpha-hidden", ".hid/alpha4", "Alpha.txt", "sub/alpha2", "sub/deep/ALPHA3"]
        );
        // Exactly as many results as the limit is not a cut-off search
        let (found, end) = search_walk(&root, "alpha", false, &no, 3, 100);
        assert_eq!((found.len(), end), (3, SearchEnd::Done));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_walk_limits() {
        let root = search_tree("limits");
        let no = AtomicBool::new(false);
        let (found, end) = search_walk(&root, "alpha", true, &no, 2, 100);
        assert_eq!((found.len(), end), (2, SearchEnd::ResultLimit));
        assert_eq!(end.note().as_deref(), Some("first 2,000 results"));
        // Root holds 5 entries; a limit of 3 visited stops inside it
        let (found, end) = search_walk(&root, "zzz", true, &no, 100, 3);
        assert_eq!((found.len(), end), (0, SearchEnd::VisitedLimit));
        assert_eq!(end.note().as_deref(), Some("stopped after 400,000 files"));
        assert_eq!(SearchEnd::Done.note(), None);
        assert_eq!(SearchEnd::Cancelled.note(), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_walk_cancelled() {
        let root = search_tree("cancel");
        let (found, end) = search_walk(&root, "alpha", true, &AtomicBool::new(true), 100, 100);
        assert_eq!((found.len(), end), (0, SearchEnd::Cancelled));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
