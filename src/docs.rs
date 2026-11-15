//! Text extraction for office documents (docx, odt, ods, odp, pptx, xlsx,
//! epub). These are zip files holding XML, so we read the XML and strip
//! the tags. No office suite needed, nothing kept in memory afterwards.

use std::io::Read;
use std::path::Path;

const MAX_TEXT: usize = 400 * 1024;

pub fn is_office(ext: &str) -> bool {
    matches!(ext, "docx" | "odt" | "ods" | "odp" | "pptx" | "xlsx" | "epub")
}

pub fn extract_text(path: &Path) -> Result<String, String> {
    let ext = ext_of(path);
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    let names: Vec<String> = zip.file_names().filter_map(|r| r.ok().map(|c| c.into_owned())).collect();
    let mut parts: Vec<String> = match ext.as_str() {
        "docx" => vec!["word/document.xml".into()],
        "odt" | "ods" | "odp" => vec!["content.xml".into()],
        "xlsx" => vec!["xl/sharedStrings.xml".into()],
        "pptx" => names
            .iter()
            .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
            .cloned()
            .collect(),
        "epub" => names
            .iter()
            .filter(|n| n.ends_with(".xhtml") || n.ends_with(".html") || n.ends_with(".htm"))
            .cloned()
            .collect(),
        _ => return Err("unsupported".into()),
    };
    parts.sort_by(|a, b| crate::util::natural_cmp(a, b));

    let mut out = String::new();
    for (i, name) in parts.iter().enumerate() {
        let Ok(mut entry) = zip.by_name(name) else { continue };
        let mut bytes = Vec::new();
        if entry.by_ref().take(8 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
            continue;
        }
        // The size cap can cut a character in half; keep everything before it. Other bad bytes still drop the part.
        let xml = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) if e.utf8_error().error_len().is_none() => {
                let n = e.utf8_error().valid_up_to();
                String::from_utf8_lossy(&e.as_bytes()[..n]).into_owned()
            }
            Err(_) => continue,
        };
        if ext == "pptx" {
            out.push_str(&format!("── Slide {} ──\n", i + 1));
        }
        out.push_str(&xml_to_text(&xml));
        out.push_str("\n\n");
        if out.len() > MAX_TEXT {
            out.truncate(floor_char_boundary(&out, MAX_TEXT));
            out.push_str("\n\n… (preview truncated)");
            break;
        }
    }
    let out = collapse_blank_lines(&out);
    if out.trim().is_empty() {
        Err("no text".into())
    } else {
        Ok(out)
    }
}

pub fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn xml_to_text(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 4);
    let mut chars = xml.char_indices();
    let mut text_start = 0;
    let mut in_body = true;
    while let Some((i, c)) = chars.next() {
        if c != '<' {
            continue;
        }
        if in_body {
            out.push_str(&decode_entities(&xml[text_start..i]));
        }
        let mut end = xml.len();
        for (j, c2) in chars.by_ref() {
            if c2 == '>' {
                end = j;
                break;
            }
        }
        let tag = &xml[i + 1..end.min(xml.len())];
        text_start = (end + 1).min(xml.len());
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let closing = tag.starts_with('/');
        match name {
            // Skip the contents of script/style in epub html
            "script" | "style" | "head" => in_body = closing,
            _ => {}
        }
        if closing {
            if matches!(
                name,
                "w:p" | "text:p" | "text:h" | "a:p" | "p" | "div" | "li" | "tr" | "si"
                    | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "table:table-row"
            ) {
                out.push('\n');
            } else if matches!(name, "table:table-cell" | "td" | "th") {
                out.push('\t');
            }
        } else if matches!(name, "w:br" | "text:line-break" | "a:br" | "br") {
            out.push('\n');
        } else if matches!(name, "w:tab" | "text:tab") {
            out.push('\t');
        }
    }
    // A part cut by the size cap ends in text with no closing tag; keep it.
    if in_body {
        out.push_str(&decode_entities(&xml[text_start..]));
    }
    out
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let Some(semi) = rest.find(';').filter(|&n| n < 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Entry names of a zip archive, for the archive preview.
pub fn zip_listing(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let total = zip.len();
    let mut out = format!("{total} entries\n\n");
    for name in zip.file_names().filter_map(|r| r.ok()).take(2000) {
        out.push_str(&name);
        out.push('\n');
    }
    if total > 2000 {
        out.push_str("…\n");
    }
    Ok(out)
}

#[cfg(test)]
mod extract_tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// A fresh temp folder that is removed again when the test ends, even on failure.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("rfiles-docs-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Writes a zip named `name` holding `parts` (entry name, contents) and returns its path.
    fn zip_file(dir: &TempDir, name: &str, parts: &[(&str, &str)]) -> PathBuf {
        let path = dir.0.join(name);
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for (entry, text) in parts {
            if entry.ends_with('/') {
                z.add_directory(*entry, zip::write::SimpleFileOptions::default()).unwrap();
                continue;
            }
            z.start_file(*entry, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(text.as_bytes()).unwrap();
        }
        z.finish().unwrap();
        path
    }

    #[test]
    fn entities() {
        assert_eq!(decode_entities("plain"), "plain");
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;&gt;&quot;&apos;&nbsp;"), "<>\"' ");
        assert_eq!(decode_entities("&#65;&#x42;&#X43;&#233;"), "ABCé");
        // Decoded once only.
        assert_eq!(decode_entities("&amp;amp;"), "&amp;");
        // Unknown, malformed, out of range and unterminated entities are kept as written.
        for s in ["&bogus;", "&#;", "&#x;", "&#xD800;", "&#99999999999;", "a & b", "tail &", "&é;"] {
            assert_eq!(decode_entities(s), s);
        }
        // A ';' too far away does not end an entity.
        assert_eq!(decode_entities("&this is long;"), "&this is long;");
        assert_eq!(decode_entities("é&amp;ü&lt;"), "é&ü<");
    }

    #[test]
    fn word_and_open_document_markup() {
        let docx = "<?xml version=\"1.0\"?><w:document><w:body><w:p><w:r><w:t>Hello</w:t></w:r></w:p>\
                    <w:p><w:r><w:t>A &amp; B</w:t><w:tab/><w:t>x</w:t><w:br/><w:t>y</w:t></w:r></w:p>\
                    </w:body></w:document>";
        assert_eq!(xml_to_text(docx), "Hello\nA & B\tx\ny\n");
        let odt = "<office:text><text:h>Title</text:h><text:p>one<text:tab/>two<text:line-break/>three</text:p>\
                   </office:text>";
        assert_eq!(xml_to_text(odt), "Title\none\ttwo\nthree\n");
        // Spreadsheet cells end in a tab, rows in a newline.
        let ods = "<table:table-row><table:table-cell>1</table:table-cell><table:table-cell>2</table:table-cell>\
                   </table:table-row>";
        assert_eq!(xml_to_text(ods), "1\t2\t\n");
        // An empty self-closing paragraph adds nothing.
        assert_eq!(xml_to_text("<w:p/><w:t>z</w:t>"), "z");
    }

    #[test]
    fn html_skips_head_script_and_style() {
        let html = "<html><head><title>T</title></head><body><h1>Title</h1><p>One &lt;b&gt;</p>\
                    <script>var a = 1;</script><style>p { color: red }</style><div>Two<br/>Three</div>\
                    <ul><li>x</li></ul><table><tr><td>a</td><th>b</th></tr></table></body></html>";
        assert_eq!(xml_to_text(html), "Title\nOne <b>\nTwo\nThree\nx\na\tb\t\n");
    }

    #[test]
    fn blank_lines() {
        assert_eq!(collapse_blank_lines("a\n\n\n\nb  \n \n\t\nc"), "a\n\nb\n\nc\n");
        assert_eq!(collapse_blank_lines(""), "");
        assert_eq!(collapse_blank_lines("x\r\ny"), "x\ny\n");
    }

    #[test]
    fn char_boundary() {
        assert_eq!(floor_char_boundary("aé", 0), 0);
        assert_eq!(floor_char_boundary("aé", 1), 1);
        assert_eq!(floor_char_boundary("aé", 2), 1);
        assert_eq!(floor_char_boundary("aé", 3), 3);
        assert_eq!(floor_char_boundary("a😀", 4), 1);
    }

    #[test]
    fn office_formats() {
        let t = TempDir::new("formats");
        let docx = zip_file(
            &t,
            "a.DOCX",
            &[
                ("[Content_Types].xml", "<Types/>"),
                ("word/document.xml", "<w:body><w:p><w:t>Report</w:t></w:p><w:p><w:t>Line</w:t></w:p></w:body>"),
            ],
        );
        // The extension is matched without regard to case.
        assert_eq!(extract_text(&docx).unwrap(), "Report\nLine\n\n");

        let odt = zip_file(&t, "b.odt", &[("mimetype", "x"), ("content.xml", "<text:p>Hej</text:p>")]);
        assert_eq!(extract_text(&odt).unwrap(), "Hej\n\n");

        let xlsx = zip_file(
            &t,
            "c.xlsx",
            &[("xl/sharedStrings.xml", "<sst><si><t>Name</t></si><si><t>Price</t></si></sst>")],
        );
        assert_eq!(extract_text(&xlsx).unwrap(), "Name\nPrice\n\n");

        // Every part ends in a blank line. Slides come in number order (10 after 2); layouts and rels are
        // not slides.
        let pptx = zip_file(
            &t,
            "d.pptx",
            &[
                ("ppt/slides/slide10.xml", "<a:p><a:t>ten</a:t></a:p>"),
                ("ppt/slides/slide2.xml", "<a:p><a:t>two</a:t></a:p>"),
                ("ppt/slides/slide1.xml", "<a:p><a:t>one</a:t></a:p>"),
                ("ppt/slides/_rels/slide1.xml.rels", "<r>rels</r>"),
                ("ppt/slideLayouts/slideLayout1.xml", "<a:p><a:t>layout</a:t></a:p>"),
            ],
        );
        assert_eq!(
            extract_text(&pptx).unwrap(),
            "── Slide 1 ──\none\n\n── Slide 2 ──\ntwo\n\n── Slide 3 ──\nten\n\n"
        );

        let epub = zip_file(
            &t,
            "e.epub",
            &[
                ("mimetype", "application/epub+zip"),
                ("OEBPS/ch10.xhtml", "<html><body><p>Last</p></body></html>"),
                ("OEBPS/ch2.html", "<html><head><title>x</title></head><body><p>First</p></body></html>"),
                ("OEBPS/style.css", "p { color: red }"),
            ],
        );
        assert_eq!(extract_text(&epub).unwrap(), "First\n\nLast\n\n");
    }

    #[test]
    fn errors() {
        let t = TempDir::new("errors");
        assert_eq!(extract_text(&zip_file(&t, "a.zip", &[("x.txt", "x")])), Err("unsupported".into()));
        // The expected part is missing, or holds no text.
        assert_eq!(extract_text(&zip_file(&t, "b.docx", &[("other.xml", "<p>x</p>")])), Err("no text".into()));
        let blank = zip_file(&t, "c.odt", &[("content.xml", "<text:p> </text:p>")]);
        assert_eq!(extract_text(&blank), Err("no text".into()));
        // Not a zip at all, and not there at all.
        let bad = t.0.join("d.docx");
        std::fs::write(&bad, "not a zip").unwrap();
        assert!(extract_text(&bad).is_err());
        assert!(extract_text(&t.0.join("missing.docx")).is_err());
        assert!(zip_listing(&bad).is_err());
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let t = TempDir::new("long");
        // One ASCII byte first, so MAX_TEXT falls in the middle of a two-byte 'é'.
        let body = format!("a{}", "é".repeat(MAX_TEXT));
        let xml = format!("<w:p><w:t>{body}</w:t></w:p>");
        let out = extract_text(&zip_file(&t, "long.docx", &[("word/document.xml", &xml)])).unwrap();
        let text = out.strip_suffix("\n\n… (preview truncated)\n").expect("truncation marker");
        assert_eq!(text.len(), MAX_TEXT - 1);
        assert!(body.starts_with(text));
    }

    #[test]
    fn listing() {
        let t = TempDir::new("listing");
        let z = zip_file(&t, "a.zip", &[("a.txt", "1"), ("b/", ""), ("b/c.txt", "2")]);
        assert_eq!(zip_listing(&z).unwrap(), "3 entries\n\na.txt\nb/\nb/c.txt\n");

        let names: Vec<String> = (0..2001).map(|i| format!("f{i}")).collect();
        let parts: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "")).collect();
        let out = zip_listing(&zip_file(&t, "many.zip", &parts)).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // Count, blank line, the first 2000 names, then a mark that there is more.
        assert_eq!(lines.len(), 2 + 2000 + 1);
        assert_eq!((lines[0], lines[2], lines[2001], lines[2002]), ("2001 entries", "f0", "f1999", "…"));
    }

    #[test]
    fn huge_part_cut_inside_a_character() {
        let t = TempDir::new("huge");
        // Over the 8 MiB read limit per part, with the limit falling inside a two-byte 'é'.
        let xml = format!("<w:p><w:t>a{}</w:t></w:p>", "é".repeat(4_200_000));
        assert!(!xml.is_char_boundary(8 * 1024 * 1024));
        let out = extract_text(&zip_file(&t, "huge.docx", &[("word/document.xml", &xml)]));
        assert!(out.as_ref().is_ok_and(|s| s.ends_with("… (preview truncated)\n")), "{:?}", out.map(|s| s.len()));
    }
}
