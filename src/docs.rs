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
        let mut xml = String::new();
        if entry.by_ref().take(8 * 1024 * 1024).read_to_string(&mut xml).is_err() {
            continue;
        }
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
