//! Thumbnail generation for images and PDFs, shared with other desktop apps
//! through the freedesktop thumbnail cache (`~/.cache/thumbnails`).
//!
//! Bound grid tiles register what they need; after each frame the UI thread
//! hands one or two worker threads only the tiles that are actually on
//! screen (or half a screen away), a few at a time. A tile that is recycled
//! for another file or destroyed drops its request, so leaving a folder or
//! scrolling past thousands of files queues nothing. Workers decode at
//! reduced size, write the PNG atomically and send back the pixels, and the
//! UI thread updates only the tile that still shows that file.
//!
//! Thumbnails already in the cache are loaded the same way, never on the UI
//! thread; the decoded textures are kept in a small in-memory LRU so tiles
//! scrolled back into view show them at once.

use crate::util::{item_file, ICON_SIZE};
use gtk::prelude::*;
use gtk::{gdk, gdk_pixbuf::{Colorspace, Pixbuf}, gio, glib};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

/// Files bigger than this are never thumbnailed.
const MAX_FILE_SIZE: i64 = 50 * 1024 * 1024;
/// Formats that can't decode at reduced size are skipped above this many
/// pixels, so one huge PNG can't spike memory by hundreds of MB.
pub(crate) const MAX_FULL_DECODE_PIXELS: i64 = 40_000_000;
/// Jobs handed to the workers at a time. The rest wait on the UI side,
/// where it is known which tiles are on screen.
const MAX_OUTSTANDING: usize = 3;
/// Remembered failures per session (cleared when it grows past this).
const MAX_SKIP: usize = 4096;
/// Existing thumbnails bigger than this are not trusted.
const MAX_THUMB_FILE: u64 = 4 * 1024 * 1024;
/// Decoded thumbnails kept in memory (per process, shared by all windows).
const TEXTURE_BUDGET: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Size {
    Normal,
    Large,
}

impl Size {
    pub fn px(self) -> i32 {
        match self {
            Size::Normal => 128,
            Size::Large => 256,
        }
    }
    pub fn dir(self) -> &'static str {
        match self {
            Size::Normal => "normal",
            Size::Large => "large",
        }
    }
    /// The smallest cache size that still looks sharp at `px` device pixels.
    pub fn for_pixels(px: i32) -> Size {
        if px > Size::Normal.px() {
            Size::Large
        } else {
            Size::Normal
        }
    }
}

/// File name of a thumbnail: md5 of the (escaped) URI, as the spec says.
pub fn thumb_name(uri: &str) -> String {
    let md5 = glib::compute_checksum_for_string(glib::ChecksumType::Md5, uri).unwrap_or_default();
    format!("{md5}.png")
}

/// Text chunks (tEXt and uncompressed iTXt) of a PNG file.
pub fn png_text(data: &[u8]) -> Vec<(String, String)> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    let mut out = Vec::new();
    if !data.starts_with(SIG) {
        return out;
    }
    let mut i = SIG.len();
    while i + 8 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        let kind = &data[i + 4..i + 8];
        let start = i + 8;
        let Some(end) = start.checked_add(len).filter(|&e| e <= data.len()) else { break };
        let body = &data[start..end];
        match kind {
            b"tEXt" => {
                if let Some(z) = body.iter().position(|&b| b == 0) {
                    // Latin-1
                    let key: String = body[..z].iter().map(|&b| b as char).collect();
                    let val: String = body[z + 1..].iter().map(|&b| b as char).collect();
                    out.push((key, val));
                }
            }
            b"iTXt" => {
                // keyword \0 compression-flag method lang \0 translated \0 text
                if let Some(z) = body.iter().position(|&b| b == 0) {
                    let rest = &body[z + 1..];
                    if rest.len() >= 2 && rest[0] == 0 {
                        let rest = &rest[2..];
                        let mut parts = rest.splitn(3, |&b| b == 0);
                        let (_, _, text) = (parts.next(), parts.next(), parts.next());
                        if let Some(text) = text {
                            out.push((
                                String::from_utf8_lossy(&body[..z]).into_owned(),
                                String::from_utf8_lossy(text).into_owned(),
                            ));
                        }
                    }
                }
            }
            b"IEND" => break,
            _ => {}
        }
        i = end + 4; // skip CRC
    }
    out
}

/// True when the PNG's metadata says it belongs to `uri` at `mtime`.
pub fn metadata_matches(text: &[(String, String)], uri: &str, mtime: i64) -> bool {
    let get = |k: &str| text.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    get("Thumb::URI") == Some(uri) && get("Thumb::MTime").and_then(|m| m.trim().parse::<i64>().ok()) == Some(mtime)
}

/// The spec metadata written into a thumbnail.
pub fn spec_metadata(uri: &str, mtime: i64, size: u64, mime: &str, dims: Option<(i32, i32)>) -> Vec<(String, String)> {
    let mut v = vec![
        ("tEXt::Thumb::URI".to_string(), uri.to_string()),
        ("tEXt::Thumb::MTime".to_string(), mtime.to_string()),
        ("tEXt::Thumb::Size".to_string(), size.to_string()),
        ("tEXt::Thumb::Mimetype".to_string(), mime.to_string()),
    ];
    if let Some((w, h)) = dims {
        v.push(("tEXt::Thumb::Image::Width".to_string(), w.to_string()));
        v.push(("tEXt::Thumb::Image::Height".to_string(), h.to_string()));
    }
    v.push(("tEXt::Software".to_string(), "RFiles".to_string()));
    v
}

// ======================= worker side =======================

struct Job {
    id: u64,
    uri: String,
    path: PathBuf,
    mime: String,
    size: Size,
    /// An existing, valid thumbnail to load instead of making one
    load: Option<PathBuf>,
}

/// Pixels of a finished thumbnail (plain data, safe to send to the UI).
struct Pixels {
    bytes: glib::Bytes,
    w: i32,
    h: i32,
    stride: usize,
    alpha: bool,
}

enum Outcome {
    Ready(PathBuf, Pixels),
    Failed,
    Skipped,
}

struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    cv: Condvar,
}

fn queue() -> &'static Queue {
    static Q: OnceLock<Queue> = OnceLock::new();
    Q.get_or_init(|| {
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 2);
        for i in 0..workers {
            let _ = std::thread::Builder::new()
                .name(format!("rfiles-thumb{i}"))
                .stack_size(512 * 1024)
                .spawn(worker);
        }
        Queue { jobs: Mutex::new(VecDeque::new()), cv: Condvar::new() }
    })
}

fn cache_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| glib::user_cache_dir().join("thumbnails"))
}

fn worker() {
    let q = queue();
    loop {
        let job = {
            let mut jobs = q.jobs.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(j) = jobs.pop_front() {
                    break j;
                }
                jobs = q.cv.wait(jobs).unwrap_or_else(|e| e.into_inner());
            }
        };
        let id = job.id;
        let outcome = process(&job);
        glib::idle_add_once(move || deliver(id, outcome));
    }
}

fn read_text_chunks(path: &Path) -> Option<Vec<(String, String)>> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(MAX_THUMB_FILE).read_to_end(&mut buf).ok()?;
    Some(png_text(&buf))
}

fn is_valid(thumb: &Path, uri: &str, mtime: i64) -> bool {
    read_text_chunks(thumb).is_some_and(|t| metadata_matches(&t, uri, mtime))
}

fn pixels(pb: &Pixbuf) -> Pixels {
    Pixels {
        bytes: pb.read_pixel_bytes(),
        w: pb.width(),
        h: pb.height(),
        stride: pb.rowstride() as usize,
        alpha: pb.has_alpha(),
    }
}

fn process(job: &Job) -> Outcome {
    if let Some(thumb) = &job.load {
        return match Pixbuf::from_file(thumb) {
            Ok(pb) => Outcome::Ready(thumb.clone(), pixels(&pb)),
            Err(_) => Outcome::Skipped,
        };
    }
    let Ok(meta) = std::fs::metadata(&job.path) else { return Outcome::Skipped };
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_FILE_SIZE as u64 {
        return Outcome::Skipped;
    }
    let mtime = meta.mtime();
    let name = thumb_name(&job.uri);
    let dir = cache_root().join(job.size.dir());
    let thumb = dir.join(&name);

    // Made meanwhile (same file shown twice, or by another app)
    if is_valid(&thumb, &job.uri, mtime) {
        return match Pixbuf::from_file(&thumb) {
            Ok(pb) => Outcome::Ready(thumb, pixels(&pb)),
            Err(_) => Outcome::Skipped,
        };
    }
    let fail_dir = cache_root().join("fail").join("rfiles");
    let fail = fail_dir.join(&name);
    if is_valid(&fail, &job.uri, mtime) {
        return Outcome::Failed;
    }

    // Decoding happens in a helper process; this one only checks its PNG
    let rendered = match render_in_helper(&job.path, &job.mime, job.size.px()) {
        Helper::Done(pb, dims) => Some((pb, dims)),
        Helper::Failed => None,
        Helper::Unavailable => return Outcome::Skipped,
    };
    let Some((pb, dims)) = rendered else {
        // Record it so it isn't retried until the file changes
        if let Some(empty) = Pixbuf::new(Colorspace::Rgb, true, 8, 1, 1) {
            empty.fill(0);
            let opts = spec_metadata(&job.uri, mtime, meta.len(), &job.mime, None);
            let _ = write_atomic(&empty, &fail_dir, &fail, &opts);
        }
        return Outcome::Failed;
    };
    let opts = spec_metadata(&job.uri, mtime, meta.len(), &job.mime, dims);
    if write_atomic(&pb, &dir, &thumb, &opts).is_err() {
        // Can't cache (read-only home?); still show it this time
        return Outcome::Ready(PathBuf::new(), pixels(&pb));
    }
    Outcome::Ready(thumb, pixels(&pb))
}

/// Thumbnails per second over the files in `$RFILES_BENCH_DIR`, two workers like the app: helper run (through
/// bwrap unless `RFILES_BENCH_DIRECT` is set; in this process with `RFILES_BENCH_IN_PROCESS`), check, metadata
/// and atomic write, as `process` does. Run with a private `XDG_CACHE_HOME`:
/// `cargo test --release thumb_bench -- --ignored --nocapture`.
#[cfg(test)]
mod thumb_bench {
    use super::*;

    #[test]
    #[ignore]
    fn thumb_bench() {
        let Ok(dir) = std::env::var("RFILES_BENCH_DIR") else { return };
        let mut jobs: Vec<Job> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| {
                let path = e.path();
                let data = std::fs::read(&path).unwrap_or_default();
                let (mime, _) = gio::content_type_guess(Some(&path), &data[..data.len().min(4096)]);
                let uri = gio::File::for_path(&path).uri().to_string();
                Job { id: 0, uri, path, mime: mime.to_string(), size: Size::Normal }
            })
            .collect();
        let n = jobs.len();
        let second = jobs.split_off(n / 2);
        // The rfiles binary next to this test binary (target/<profile>/deps/..)
        let exe = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("rfiles");
        let direct = std::env::var_os("RFILES_BENCH_DIRECT").is_some();
        let bwrap = find_bwrap().filter(|b| !direct && sandbox_works(b, &exe));
        println!("BENCH sandbox: {bwrap:?}");
        let one = |j: &Job| {
            let Ok(meta) = std::fs::metadata(&j.path) else { return false };
            let done = if std::env::var_os("RFILES_BENCH_IN_PROCESS").is_some() {
                // How it was before the helper, for comparison
                let r = if j.mime == "application/pdf" {
                    render_pdf(&j.uri, j.size.px()).map(|pb| (pb, None))
                } else {
                    render_image(&j.path, j.size.px())
                };
                r.map_or(Helper::Failed, |(pb, dims)| Helper::Done(pb, dims))
            } else {
                run_helper(&exe, bwrap.as_deref(), &j.path, &j.mime, j.size.px())
            };
            let Helper::Done(pb, dims) = done else { return false };
            let dir = cache_root().join(j.size.dir());
            let opts = spec_metadata(&j.uri, meta.mtime(), meta.len(), &j.mime, dims);
            write_atomic(&pb, &dir, &dir.join(thumb_name(&j.uri)), &opts).is_ok()
        };
        let start = std::time::Instant::now();
        let ready = std::thread::scope(|s| {
            let a = s.spawn(|| jobs.iter().filter(|j| one(j)).count());
            let b = s.spawn(|| second.iter().filter(|j| one(j)).count());
            a.join().unwrap() + b.join().unwrap()
        });
        let secs = start.elapsed().as_secs_f64();
        println!("BENCH {ready}/{n} ready in {secs:.2} s: {:.1} thumbnails/s", n as f64 / secs);
    }
}

/// Decodes an image no bigger than `px` (JPEG decodes at reduced scale).
/// Returns the thumbnail and the original dimensions.
fn render_image(path: &Path, px: i32) -> Option<(Pixbuf, Option<(i32, i32)>)> {
    let (fmt, w, h) = Pixbuf::file_info(path)?;
    if w <= 0 || h <= 0 {
        return None;
    }
    let scaled_decode = fmt.is_scalable() || fmt.name().as_deref() == Some("jpeg");
    if !scaled_decode && (w as i64) * (h as i64) > MAX_FULL_DECODE_PIXELS {
        return None;
    }
    let pb = if w > px || h > px {
        Pixbuf::from_file_at_scale(path, px, px, true).ok()?
    } else {
        Pixbuf::from_file(path).ok()?
    };
    let pb = pb.apply_embedded_orientation().unwrap_or(pb);
    Some((pb, Some((w, h))))
}

/// Renders the first page of a PDF on white, fitting `px`.
fn render_pdf(uri: &str, px: i32) -> Option<Pixbuf> {
    use gtk::cairo;
    let doc = poppler::Document::from_file(uri, None).ok()?;
    let page = doc.page(0)?;
    let (pw, ph) = page.size();
    if pw <= 0.0 || ph <= 0.0 {
        return None;
    }
    let s = px as f64 / pw.max(ph);
    let w = ((pw * s).round() as i32).clamp(1, px);
    let h = ((ph * s).round() as i32).clamp(1, px);
    let mut surf = cairo::ImageSurface::create(cairo::Format::Rgb24, w, h).ok()?;
    {
        let cr = cairo::Context::new(&surf).ok()?;
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.paint().ok()?;
        cr.scale(s, s);
        page.render(&cr);
    }
    surf.flush();
    let stride = surf.stride() as usize;
    let data = surf.data().ok()?;
    // Cairo RGB24 is a native-endian 0x00RRGGBB word per pixel
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for row in data.chunks(stride).take(h as usize) {
        for px in row[..w as usize * 4].as_chunks::<4>().0 {
            let v = u32::from_ne_bytes(*px);
            rgb.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8]);
        }
    }
    Some(Pixbuf::from_mut_slice(rgb, Colorspace::Rgb, false, 8, w, h, w * 3))
}

/// Saves to a temp file next to the target and renames it into place, so
/// other programs never see a half-written thumbnail.
fn write_atomic(pb: &Pixbuf, dir: &Path, target: &Path, opts: &[(String, String)]) -> Result<(), ()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|_| ())?;
    let name = target.file_name().ok_or(())?.to_string_lossy().into_owned();
    let tmp = dir.join(format!(
        ".{name}.rfiles-{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let opts: Vec<(&str, &str)> = opts.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let res = pb
        .savev(&tmp, "png", &opts)
        .map_err(|_| ())
        .and_then(|_| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|_| ()))
        .and_then(|_| std::fs::rename(&tmp, target).map_err(|_| ()));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

// ======================= helper process =======================
//
// Image and PDF parsers are a common source of memory-safety bugs, and folders fill with files nobody chose to
// open. So the decoding runs in a short-lived `rfiles --thumbnail` process, inside bwrap when it works here, with
// a time and memory limit. The main process only reads back a small PNG after checking its header.

/// A helper that runs longer than this is killed and the file marked failed.
const HELPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Address-space limit of the helper. Generous: the pixel limit above already bounds real decodes.
const HELPER_MEMORY: u64 = 2 * 1024 * 1024 * 1024;
/// Largest thumbnail size the helper accepts (the spec's biggest is 1024).
const HELPER_MAX_PX: i32 = 1024;
/// Where things are inside the sandbox.
const BOX_EXE: &str = "/tmp/rfiles";
const BOX_OUT_DIR: &str = "/tmp/out";
const BOX_OUT: &str = "/tmp/out/thumb.png";

/// What a helper run gave.
enum Helper {
    Done(Pixbuf, Option<(i32, i32)>),
    /// The file couldn't be thumbnailed (bad file, timeout, unusable result)
    Failed,
    /// No helper could be started (binary replaced by an update?): try again another time
    Unavailable,
}

/// `rfiles --thumbnail <input> <output-png> <size-px> <mime>`: renders the input and writes a plain PNG. Returns
/// the exit code when that is what was asked for. Runs before GTK starts and needs no display.
pub fn helper_request(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some("--thumbnail") {
        return None;
    }
    let ok = match parse_helper_args(&args[2..]) {
        Some((input, out, px, mime)) => helper_render(input, out, px, mime),
        None => false,
    };
    Some(if ok { 0 } else { 1 })
}

fn parse_helper_args(a: &[String]) -> Option<(&Path, &Path, i32, &str)> {
    let [input, out, px, mime] = a else { return None };
    let px = px.parse::<i32>().ok().filter(|p| (1..=HELPER_MAX_PX).contains(p))?;
    Some((Path::new(input), Path::new(out), px, mime.as_str()))
}

fn helper_render(input: &Path, out: &Path, px: i32, mime: &str) -> bool {
    if !std::fs::metadata(input).is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_FILE_SIZE as u64) {
        return false;
    }
    let rendered = if mime == "application/pdf" {
        let Ok(uri) = glib::filename_to_uri(input, None) else { return false };
        render_pdf(&uri, px).map(|pb| (pb, None))
    } else {
        render_image(input, px)
    };
    let Some((pb, dims)) = rendered else { return false };
    if pb.width() > px || pb.height() > px {
        return false;
    }
    // The original size goes back as text chunks; the caller writes the real spec metadata
    let dims: Vec<(String, String)> = dims
        .map(|(w, h)| {
            let (w, h) = (w.to_string(), h.to_string());
            vec![("tEXt::Thumb::Image::Width".into(), w), ("tEXt::Thumb::Image::Height".into(), h)]
        })
        .unwrap_or_default();
    let opts: Vec<(&str, &str)> = dims.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    pb.savev(out, "png", &opts).is_ok()
}

/// Width and height of a helper's PNG when it is what `helper_render` writes: an 8-bit RGB or RGBA PNG of
/// 1..=`px` pixels each way, complete up to IEND. Anything else is refused before the PNG decoder sees it.
fn check_png(data: &[u8], px: i32) -> Option<(i32, i32)> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    // Some libpng loaders accept a cut-off file and fill in the rest
    const IEND: &[u8] = b"\0\0\0\0IEND\xae\x42\x60\x82";
    if !data.ends_with(IEND) {
        return None;
    }
    // The first chunk must be a 13-byte IHDR
    let ihdr = data.strip_prefix(SIG)?.get(..8 + 13)?;
    if ihdr[..8] != [0, 0, 0, 13, b'I', b'H', b'D', b'R'] {
        return None;
    }
    let num = |i: usize| i32::try_from(u32::from_be_bytes([ihdr[i], ihdr[i + 1], ihdr[i + 2], ihdr[i + 3]])).ok();
    let (w, h) = (num(8)?, num(12)?);
    let (depth, color) = (ihdr[16], ihdr[17]);
    let ok = (1..=px).contains(&w) && (1..=px).contains(&h) && depth == 8 && (color == 2 || color == 6);
    ok.then_some((w, h))
}

/// The original image size the helper reported, if it is sane.
fn helper_dims(text: &[(String, String)]) -> Option<(i32, i32)> {
    let get = |k: &str| {
        let v = text.iter().find(|(key, _)| key == k)?.1.parse::<i32>().ok()?;
        (v > 0).then_some(v)
    };
    Some((get("Thumb::Image::Width")?, get("Thumb::Image::Height")?))
}

/// Decodes a helper's PNG once `check_png` accepted it, with the PNG loader only (no format sniffing).
fn load_helper_png(data: &[u8], px: i32) -> Option<(Pixbuf, Option<(i32, i32)>)> {
    let (w, h) = check_png(data, px)?;
    let loader = gtk::gdk_pixbuf::PixbufLoader::with_type("png").ok()?;
    let fed = loader.write(data).is_ok();
    if loader.close().is_err() || !fed {
        return None;
    }
    let pb = loader.pixbuf()?;
    (pb.width() == w && pb.height() == h).then(|| (pb, helper_dims(&png_text(data))))
}

/// bwrap, if installed.
fn find_bwrap() -> Option<PathBuf> {
    let fixed = PathBuf::from("/usr/bin/bwrap");
    if fixed.is_file() {
        return Some(fixed);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("bwrap")).find(|p| p.is_file())
}

/// Top-level folders the dynamic loader and libraries are found through, besides /usr: on merged-/usr systems
/// these are symlinks into it, elsewhere real folders. (path, symlink target)
fn root_entries() -> Vec<(&'static str, Option<PathBuf>)> {
    ["/lib", "/lib64", "/lib32", "/libx32"]
        .into_iter()
        .filter_map(|p| {
            let meta = std::fs::symlink_metadata(p).ok()?;
            if meta.file_type().is_symlink() {
                Some((p, Some(std::fs::read_link(p).ok()?)))
            } else {
                meta.is_dir().then_some((p, None))
            }
        })
        .collect()
}

/// File name of the input inside the sandbox. Keeps a plain extension, which some loaders look at.
fn box_input(path: &Path) -> String {
    let ext = path.extension().and_then(|e| e.to_str()).filter(|e| {
        !e.is_empty() && e.len() <= 10 && e.bytes().all(|b| b.is_ascii_alphanumeric())
    });
    match ext {
        Some(e) => format!("/tmp/input.{e}"),
        None => "/tmp/input".to_string(),
    }
}

/// bwrap arguments, up to and including the program to run. With `job` = (input file, output folder), the input
/// is bound read-only at `box_input` and the folder writable at `BOX_OUT_DIR`; without, it only runs the binary.
fn bwrap_args(
    exe: &Path,
    home: Option<&Path>,
    roots: &[(&str, Option<PathBuf>)],
    job: Option<(&Path, &Path)>,
) -> Vec<std::ffi::OsString> {
    let mut a: Vec<std::ffi::OsString> = Vec::new();
    let mut add = |items: &[&std::ffi::OsStr]| a.extend(items.iter().map(|s| s.to_os_string()));
    let s = |v: &'static str| std::ffi::OsStr::new(v);
    // No network, own PID/IPC/user namespaces; nothing outlives RFiles; no access to the terminal
    add(&[s("--unshare-all"), s("--die-with-parent"), s("--new-session")]);
    add(&[s("--ro-bind"), s("/usr"), s("/usr")]);
    for (path, link) in roots {
        match link {
            Some(target) => add(&[s("--symlink"), target.as_os_str(), std::ffi::OsStr::new(path)]),
            None => add(&[s("--ro-bind"), std::ffi::OsStr::new(path), std::ffi::OsStr::new(path)]),
        }
    }
    // Library lookup and fonts for PDFs
    add(&[s("--ro-bind-try"), s("/etc/ld.so.cache"), s("/etc/ld.so.cache")]);
    add(&[s("--ro-bind-try"), s("/etc/fonts"), s("/etc/fonts")]);
    add(&[s("--ro-bind-try"), s("/var/cache/fontconfig"), s("/var/cache/fontconfig")]);
    // gdk-pixbuf's glycin loaders (Ubuntu 25.10+) run as child processes, which need these
    add(&[s("--proc"), s("/proc"), s("--dev"), s("/dev")]);
    add(&[s("--tmpfs"), s("/tmp")]);
    if let Some(home) = home.filter(|h| h.is_absolute() && h.parent().is_some() && !h.starts_with("/tmp")) {
        add(&[s("--tmpfs"), home.as_os_str()]);
    }
    add(&[s("--ro-bind"), exe.as_os_str(), s(BOX_EXE)]);
    if let Some((input, out_dir)) = job {
        let inside = std::ffi::OsString::from(box_input(input));
        add(&[s("--ro-bind"), input.as_os_str(), &inside]);
        add(&[s("--bind"), out_dir.as_os_str(), s(BOX_OUT_DIR)]);
    }
    add(&[s("--chdir"), s("/"), s(BOX_EXE)]);
    a
}

/// A command with the helper's environment and limits: cleared environment, no core dumps, an address-space
/// limit, and its own process group so a timeout kills everything it started.
fn limited(program: &Path) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", glib::home_dir())
        .env("GIO_USE_VFS", "local")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    // SAFETY: setrlimit is async-signal-safe and touches no memory of the parent
    unsafe {
        cmd.pre_exec(|| {
            let core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            let mem = libc::rlimit { rlim_cur: HELPER_MEMORY, rlim_max: HELPER_MEMORY };
            if libc::setrlimit(libc::RLIMIT_CORE, &core) != 0 || libc::setrlimit(libc::RLIMIT_AS, &mem) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// Runs a command to the end or kills it (and its process group) after `HELPER_TIMEOUT`. Err when it couldn't be
/// started; Ok(false) when it failed or ran too long.
fn run_limited(cmd: std::process::Command) -> std::io::Result<bool> {
    run_for(cmd, HELPER_TIMEOUT)
}

fn run_for(mut cmd: std::process::Command, limit: std::time::Duration) -> std::io::Result<bool> {
    let mut child = cmd.spawn()?;
    let start = std::time::Instant::now();
    let mut nap = std::time::Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) if start.elapsed() < limit => {
                std::thread::sleep(nap);
                nap = (nap * 2).min(std::time::Duration::from_millis(10));
            }
            _ => {
                // Not reaped yet, so the group id is still ours
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                return Ok(false);
            }
        }
    }
}

/// Set when bwrap stopped working during the session.
static SANDBOX_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True when `bwrap` can run `exe` in the sandbox here: user namespaces may be disabled or restricted
/// (containers, AppArmor). Running `--version` in it proves bwrap and the binary's libraries both work.
fn sandbox_works(bwrap: &Path, exe: &Path) -> bool {
    let mut cmd = limited(bwrap);
    cmd.args(bwrap_args(exe, Some(&glib::home_dir()), &root_entries(), None)).arg("--version");
    run_limited(cmd).unwrap_or(false)
}

/// bwrap, if it works here. Checked once per session, as otherwise every file would fail the same way.
fn sandbox(exe: &Path) -> Option<&'static Path> {
    static BWRAP: OnceLock<Option<PathBuf>> = OnceLock::new();
    if SANDBOX_OFF.load(Ordering::Relaxed) {
        return None;
    }
    BWRAP.get_or_init(|| find_bwrap().filter(|b| sandbox_works(b, exe))).as_deref()
}

/// Renders `path` in a helper process (sandboxed when bwrap works) and reads back its thumbnail. Types RFiles
/// can't render itself go to the system's thumbnailer for them, if there is one.
fn render_in_helper(path: &Path, mime: &str, px: i32) -> Helper {
    let Ok(exe) = std::env::current_exe() else { return Helper::Unavailable };
    // Replaced by an update: shows as "… (deleted)"
    if !exe.is_file() {
        return Helper::Unavailable;
    }
    let tn = Some(mime).filter(|m| !renders_itself(m)).and_then(thumbnailer_for);
    let run = |bwrap: Option<&Path>| match tn {
        Some(tn) => run_thumbnailer(&exe, bwrap, tn, path, px),
        None => run_helper(&exe, bwrap, path, mime, px),
    };
    match sandbox(&exe) {
        Some(bwrap) => match run(Some(bwrap)) {
            Helper::Unavailable if !bwrap.is_file() => {
                // bwrap went away: carry on without it for this session
                SANDBOX_OFF.store(true, Ordering::Relaxed);
                run(None)
            }
            done => done,
        },
        None => run(None),
    }
}

/// Runs the helper `exe` on one file, inside `bwrap` if given, in a private temp folder.
fn run_helper(exe: &Path, bwrap: Option<&Path>, path: &Path, mime: &str, px: i32) -> Helper {
    run_in_private_dir(px, |tmp, out| {
        let mut cmd = match bwrap {
            Some(bwrap) => {
                let mut cmd = limited(bwrap);
                cmd.args(bwrap_args(exe, Some(&glib::home_dir()), &root_entries(), Some((path, tmp))));
                cmd.arg("--thumbnail").arg(box_input(path)).arg(BOX_OUT);
                cmd
            }
            None => {
                let mut cmd = limited(exe);
                cmd.arg("--thumbnail").arg(path).arg(out);
                cmd
            }
        };
        cmd.arg(px.to_string()).arg(mime);
        Some(cmd)
    })
}

/// Makes a private folder, runs the command `make` builds for it (given the folder and the PNG path in it) under
/// the time limit, and reads back and checks the PNG.
fn run_in_private_dir(px: i32, make: impl FnOnce(&Path, &Path) -> Option<std::process::Command>) -> Helper {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "rfiles-thumb-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // Not recursive: fails rather than reuse a folder someone else made
    if std::fs::DirBuilder::new().mode(0o700).create(&tmp).is_err() {
        return Helper::Unavailable;
    }
    let out = tmp.join("thumb.png");
    let result = match make(&tmp, &out).map(run_limited) {
        None | Some(Ok(false)) => Helper::Failed,
        Some(Err(_)) => Helper::Unavailable,
        Some(Ok(true)) => {
            let mut data = Vec::new();
            let read = std::fs::File::open(&out).and_then(|f| f.take(MAX_THUMB_FILE).read_to_end(&mut data));
            match read.ok().and_then(|_| load_helper_png(&data, px)) {
                Some((pb, dims)) => Helper::Done(pb, dims),
                None => Helper::Failed,
            }
        }
    };
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

// ======================= system thumbnailers =======================
//
// For types RFiles can't draw (videos, fonts, e-books, ...) it runs the program an installed `.thumbnailer` file
// names, the way GNOME Files does, with the same sandbox, limits and PNG check as its own helper.

/// One `.thumbnailer` file.
#[derive(Clone, Debug, PartialEq)]
struct Thumbnailer {
    try_exec: Option<String>,
    exec: String,
}

/// Where the thumbnailer files are. Only /usr is visible inside the sandbox, so programs elsewhere couldn't run.
const THUMBNAILER_DIR: &str = "/usr/share/thumbnailers";

/// Reads a `.thumbnailer` file: the keys of its `[Thumbnailer Entry]` group, and the MIME types it lists.
fn parse_thumbnailer(text: &str) -> Option<(Thumbnailer, Vec<String>)> {
    let (mut in_group, mut exec, mut try_exec, mut mimes) = (false, None, None, Vec::new());
    for line in text.lines().map(str::trim) {
        if let Some(group) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_group = group == "Thumbnailer Entry";
        } else if in_group && !line.starts_with('#') {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim() {
                "Exec" if !value.is_empty() => exec = Some(value.to_string()),
                "TryExec" if !value.is_empty() => try_exec = Some(value.to_string()),
                "MimeType" => {
                    mimes = value.split(';').map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect()
                }
                _ => {}
            }
        }
    }
    if mimes.is_empty() {
        return None;
    }
    Some((Thumbnailer { try_exec, exec: exec? }, mimes))
}

/// Splits an `Exec` line into arguments as the Desktop Entry spec does: spaces separate them, double quotes keep
/// one together (with backslash escapes inside). `None` for an unclosed quote or an empty line.
fn split_exec(line: &str) -> Option<Vec<String>> {
    let (mut args, mut cur, mut started, mut quoted) = (Vec::new(), String::new(), false, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => cur.push(chars.next()?),
            c if c.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        args.push(cur);
    }
    (!args.is_empty()).then_some(args)
}

/// The command line for `exec` with %i (input path), %u (input URI), %o (output path), %s (size) and %% filled
/// in. Each argument stays one argument whatever the values contain; other codes are dropped.
fn expand_exec(exec: &str, input: &str, uri: &str, out: &str, px: i32) -> Option<Vec<String>> {
    let size = px.to_string();
    let args = split_exec(exec)?.into_iter().map(|arg| {
        let mut res = String::new();
        let mut chars = arg.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                res.push(c);
                continue;
            }
            match chars.next() {
                Some('i') => res.push_str(input),
                Some('u') => res.push_str(uri),
                Some('o') => res.push_str(out),
                Some('s') => res.push_str(&size),
                Some('%') => res.push('%'),
                _ => {}
            }
        }
        res
    });
    Some(args.collect())
}

/// True when `TryExec` names a program that exists (an absolute path, or a name found in PATH).
fn try_exec_exists(name: &str) -> bool {
    if name.starts_with('/') {
        return Path::new(name).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// MIME type -> thumbnailer from every `*.thumbnailer` in `dirs`. Files are read in name order and the first one
/// to list a type keeps it; ones whose `TryExec` program is missing are left out.
fn load_thumbnailers(dirs: &[PathBuf]) -> HashMap<String, Thumbnailer> {
    let mut map = HashMap::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut files: Vec<PathBuf> =
            rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "thumbnailer")).collect();
        files.sort();
        for file in files {
            let Ok(f) = std::fs::File::open(&file) else { continue };
            let mut text = String::new();
            if f.take(64 * 1024).read_to_string(&mut text).is_err() {
                continue;
            }
            let Some((tn, mimes)) = parse_thumbnailer(&text) else { continue };
            if tn.try_exec.as_deref().is_some_and(|t| !try_exec_exists(t)) {
                continue;
            }
            for mime in mimes {
                map.entry(mime).or_insert_with(|| tn.clone());
            }
        }
    }
    map
}

/// The installed thumbnailer for a MIME type. The files are read once per session.
fn thumbnailer_for(mime: &str) -> Option<&'static Thumbnailer> {
    static ALL: OnceLock<HashMap<String, Thumbnailer>> = OnceLock::new();
    ALL.get_or_init(|| load_thumbnailers(&[PathBuf::from(THUMBNAILER_DIR)])).get(mime)
}

/// MIME types RFiles' own helper draws: what gdk-pixbuf loads, and PDF.
fn renders_itself(mime: &str) -> bool {
    static OWN: OnceLock<HashSet<String>> = OnceLock::new();
    mime == "application/pdf"
        || OWN
            .get_or_init(|| {
                Pixbuf::formats()
                    .iter()
                    .filter(|f| !f.is_disabled())
                    .flat_map(|f| f.mime_types())
                    .map(|m| m.to_string())
                    .collect()
            })
            .contains(mime)
}

/// Runs a system thumbnailer on one file like `run_helper` does: in `bwrap` if given, in a private temp folder,
/// then the PNG it wrote is checked by `load_helper_png`.
fn run_thumbnailer(exe: &Path, bwrap: Option<&Path>, tn: &Thumbnailer, path: &Path, px: i32) -> Helper {
    run_in_private_dir(px, |tmp, out| {
        // Inside the sandbox the file is at a fixed, plain name
        let (input, out) = match bwrap {
            Some(_) => (PathBuf::from(box_input(path)), PathBuf::from(BOX_OUT)),
            None => (path.to_path_buf(), out.to_path_buf()),
        };
        let uri = glib::filename_to_uri(&input, None).ok()?;
        let args = expand_exec(&tn.exec, &input.to_string_lossy(), uri.as_str(), &out.to_string_lossy(), px)?;
        let (program, rest) = args.split_first()?;
        Some(match bwrap {
            Some(bwrap) => {
                let mut cmd = limited(bwrap);
                let mut a = bwrap_args(exe, Some(&glib::home_dir()), &root_entries(), Some((path, tmp)));
                // The last argument is RFiles' own binary; the thumbnailer runs instead
                a.pop();
                cmd.args(a).arg(program).args(rest);
                cmd
            }
            None => {
                let mut cmd = limited(Path::new(program));
                cmd.args(rest);
                cmd
            }
        })
    })
}

#[cfg(test)]
mod system_thumbnailer_tests {
    use super::*;

    const GST: &str = "[Thumbnailer Entry]\n# a comment\nTryExec=/usr/bin/true\n\
        Exec=/usr/bin/gst-video-thumbnailer --input-uri %u --output %o --size %s\n\
        MimeType=video/mp4; video/ogg;;video/webm;\n[Other]\nExec=/bin/other\nMimeType=x/y\n";

    fn args(v: &[&str]) -> Option<Vec<String>> {
        Some(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn thumbnailer_file_parsing() {
        let (tn, mimes) = parse_thumbnailer(GST).unwrap();
        assert_eq!(tn.try_exec.as_deref(), Some("/usr/bin/true"));
        assert_eq!(tn.exec, "/usr/bin/gst-video-thumbnailer --input-uri %u --output %o --size %s");
        assert_eq!(mimes, ["video/mp4", "video/ogg", "video/webm"]);
        // Keys outside the group, or no MIME types, or no Exec: not usable
        assert!(parse_thumbnailer("[Other]\nExec=/bin/x\nMimeType=a/b;\n").is_none());
        assert!(parse_thumbnailer("[Thumbnailer Entry]\nExec=/bin/x\n").is_none());
        assert!(parse_thumbnailer("[Thumbnailer Entry]\nMimeType=a/b;\n").is_none());
        assert!(parse_thumbnailer("[Thumbnailer Entry]\nExec=\nMimeType=a/b;\n").is_none());
        let (tn, _) = parse_thumbnailer("[Thumbnailer Entry]\nExec=/bin/x %i\nMimeType=a/b\n").unwrap();
        assert_eq!(tn.try_exec, None);
    }

    #[test]
    fn exec_expansion() {
        let line = "/usr/bin/t --uri %u -i %i --out=%o -s %s 100%% %z";
        assert_eq!(
            expand_exec(line, "/a/b.mp4", "file:///a/b.mp4", "/o/t.png", 256),
            args(&["/usr/bin/t", "--uri", "file:///a/b.mp4", "-i", "/a/b.mp4", "--out=/o/t.png", "-s", "256", "100%", ""])
        );
        // Spaces, quotes and percent signs in the values never split or re-expand
        let (i, u) = ("/my dir/a \"b\" %o.mp4", "file:///my%20dir/a%20b.mp4");
        assert_eq!(
            expand_exec("t %i %u", i, u, "/o", 64),
            args(&["t", "/my dir/a \"b\" %o.mp4", "file:///my%20dir/a%20b.mp4"])
        );
        // Quotes in the Exec line itself group words
        assert_eq!(expand_exec("\"/opt/my tool/t\" -x \"a \\\"b\\\" %i\"", "/f", "u", "o", 1), args(&["/opt/my tool/t", "-x", "a \"b\" /f"]));
        assert_eq!(expand_exec("t \"\" x", "/f", "u", "o", 1), args(&["t", "", "x"]));
        assert_eq!(expand_exec("t \"unclosed", "/f", "u", "o", 1), None);
        assert_eq!(expand_exec("   ", "/f", "u", "o", 1), None);
    }

    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("rfiles-thumbnailers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    #[test]
    fn loading_thumbnailer_folders() {
        let d = dir("load");
        let w = |name: &str, text: &str| std::fs::write(d.0.join(name), text).unwrap();
        w("b.thumbnailer", "[Thumbnailer Entry]\nExec=/bin/b %i %o\nMimeType=video/mp4;video/webm;\n");
        w("a.thumbnailer", "[Thumbnailer Entry]\nExec=/bin/a %i %o\nMimeType=video/mp4;\n");
        w("gone.thumbnailer", "[Thumbnailer Entry]\nTryExec=/nonexistent/prog\nExec=/bin/g\nMimeType=font/ttf;\n");
        w("notes.txt", "[Thumbnailer Entry]\nExec=/bin/t\nMimeType=text/x-test;\n");
        w("bad.thumbnailer", "garbage");
        let map = load_thumbnailers(&[d.0.clone(), d.0.join("missing")]);
        assert_eq!(map.len(), 2);
        // Name order: a.thumbnailer wins video/mp4
        assert_eq!(map["video/mp4"].exec, "/bin/a %i %o");
        assert_eq!(map["video/webm"].exec, "/bin/b %i %o");
    }

    /// A 200x100 PNG named with a space in it, and a fake thumbnailer that copies its input to its output.
    fn setup(tag: &str) -> (Dir, PathBuf) {
        let d = dir(tag);
        let input = d.0.join("my clip.png");
        let pb = Pixbuf::new(Colorspace::Rgb, false, 8, 200, 100).unwrap();
        pb.fill(0x2266aaff);
        pb.savev(&input, "png", &[]).unwrap();
        (d, input)
    }

    fn fake(exec: &str) -> Thumbnailer {
        Thumbnailer { try_exec: None, exec: exec.to_string() }
    }

    fn check_fake(exe: &Path, bwrap: Option<&Path>, input: &Path) {
        let copy = fake("/usr/bin/sh -c \"cp \\\"$1\\\" \\\"$2\\\"\" sh %i %o");
        match run_thumbnailer(exe, bwrap, &copy, input, 256) {
            Helper::Done(pb, dims) => assert_eq!((pb.width(), pb.height(), dims), (200, 100, None)),
            _ => panic!("the copy thumbnailer should give a thumbnail"),
        }
        // Too big for the size asked: refused
        assert!(matches!(run_thumbnailer(exe, bwrap, &copy, input, 150), Helper::Failed));
        // Exit status, no output, and output that isn't a PNG
        assert!(matches!(run_thumbnailer(exe, bwrap, &fake("/usr/bin/false"), input, 256), Helper::Failed));
        assert!(matches!(run_thumbnailer(exe, bwrap, &fake("/usr/bin/true"), input, 256), Helper::Failed));
        let text = fake("/usr/bin/sh -c \"echo hi > \\\"$1\\\"\" sh %o");
        assert!(matches!(run_thumbnailer(exe, bwrap, &text, input, 256), Helper::Failed));
        // A program that isn't there: nothing to show, not a retry later
        let r = run_thumbnailer(exe, bwrap, &fake("/nonexistent/prog %i"), input, 256);
        assert!(matches!(r, Helper::Failed | Helper::Unavailable));
    }

    #[test]
    fn fake_thumbnailer_without_sandbox() {
        let (_d, input) = setup("direct");
        check_fake(Path::new("/nonexistent"), None, &input);
    }

    fn sandbox_for_tests() -> Option<(PathBuf, PathBuf)> {
        let exe = std::env::current_exe().ok()?.parent()?.parent()?.join("rfiles");
        if !exe.is_file() {
            eprintln!("SKIPPED: the rfiles binary isn't built next to the tests");
            return None;
        }
        let Some(bwrap) = find_bwrap() else {
            eprintln!("SKIPPED: bwrap is not installed");
            return None;
        };
        if !sandbox_works(&bwrap, &exe) {
            eprintln!("SKIPPED: bwrap can't create a sandbox here (user namespaces restricted?)");
            return None;
        }
        Some((exe, bwrap))
    }

    #[test]
    fn fake_thumbnailer_in_sandbox() {
        let Some((exe, bwrap)) = sandbox_for_tests() else { return };
        let (_d, input) = setup("bwrap");
        check_fake(&exe, Some(&bwrap), &input);
    }

    /// A real video through the system's video thumbnailer, if one is installed and gst-launch can make a clip.
    #[test]
    fn real_video() {
        let Some(tn) = thumbnailer_for("video/ogg") else {
            eprintln!("SKIPPED: no thumbnailer for video/ogg is installed");
            return;
        };
        let d = dir("video");
        let clip = d.0.join("my clip.ogv");
        let made = std::process::Command::new("gst-launch-1.0")
            .args(["-q", "videotestsrc", "num-buffers=30", "!", "video/x-raw,width=320,height=240", "!", "theoraenc"])
            .args(["!", "oggmux", "!", "filesink"])
            .arg(format!("location={}", clip.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !made.is_ok_and(|s| s.success()) || !clip.is_file() {
            eprintln!("SKIPPED: gst-launch-1.0 can't make a test clip");
            return;
        }
        let ok = |h: Helper| match h {
            Helper::Done(pb, _) => (1..=256).contains(&pb.width()) && (1..=256).contains(&pb.height()),
            _ => false,
        };
        assert!(ok(run_thumbnailer(Path::new("/nonexistent"), None, tn, &clip, 256)), "without sandbox");
        if let Some((exe, bwrap)) = sandbox_for_tests() {
            assert!(ok(run_thumbnailer(&exe, Some(&bwrap), tn, &clip, 256)), "in the sandbox");
        }
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn helper_arguments() {
        assert_eq!(helper_request(&v(&["rfiles", "/tmp"])), None);
        assert_eq!(helper_request(&v(&["rfiles"])), None);
        // Wrong argument counts and sizes fail without touching anything
        assert_eq!(helper_request(&v(&["rfiles", "--thumbnail"])), Some(1));
        assert_eq!(helper_request(&v(&["rfiles", "--thumbnail", "/a", "/b", "128"])), Some(1));
        let a = v(&["/in.png", "/out.png", "128", "image/png"]);
        assert_eq!(parse_helper_args(&a), Some((Path::new("/in.png"), Path::new("/out.png"), 128, "image/png")));
        for bad in ["0", "-5", "1025", "99999999999", "abc"] {
            assert_eq!(parse_helper_args(&v(&["/in.png", "/out.png", bad, "image/png"])), None);
        }
    }

    fn png_header(w: u32, h: u32, depth: u8, color: u8) -> Vec<u8> {
        let mut d = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        d.extend_from_slice(&w.to_be_bytes());
        d.extend_from_slice(&h.to_be_bytes());
        d.extend_from_slice(&[depth, color, 0, 0, 0]);
        // CRC (not checked here), then the end marker
        d.extend_from_slice(b"\0\0\0\0\0\0\0\0IEND\xae\x42\x60\x82");
        d
    }

    #[test]
    fn png_check() {
        assert_eq!(check_png(&png_header(128, 96, 8, 6), 128), Some((128, 96)));
        assert_eq!(check_png(&png_header(1, 1, 8, 2), 128), Some((1, 1)));
        // Bigger than asked, empty, absurd
        assert_eq!(check_png(&png_header(129, 96, 8, 6), 128), None);
        assert_eq!(check_png(&png_header(0, 96, 8, 6), 128), None);
        assert_eq!(check_png(&png_header(u32::MAX, 96, 8, 6), 128), None);
        // Other bit depths, palette and gray images are not what the helper writes
        assert_eq!(check_png(&png_header(64, 64, 16, 6), 128), None);
        assert_eq!(check_png(&png_header(64, 64, 8, 3), 128), None);
        // Truncated, not a PNG, IHDR not first
        assert_eq!(check_png(&png_header(64, 64, 8, 6)[..20], 128), None);
        // Complete header but cut off before IEND
        assert_eq!(check_png(&png_header(64, 64, 8, 6)[..37], 128), None);
        assert_eq!(check_png(b"GIF89a", 128), None);
        let mut d = png_header(64, 64, 8, 6);
        d[12..16].copy_from_slice(b"tEXt");
        assert_eq!(check_png(&d, 128), None);
    }

    #[test]
    fn real_png_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rfiles-helper-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("t.png");
        let pb = Pixbuf::new(Colorspace::Rgb, true, 8, 100, 50).unwrap();
        pb.fill(0x336699ff);
        pb.savev(&out, "png", &[("tEXt::Thumb::Image::Width", "4000"), ("tEXt::Thumb::Image::Height", "2000")])
            .unwrap();
        let data = std::fs::read(&out).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let (got, dims) = load_helper_png(&data, 128).unwrap();
        assert_eq!((got.width(), got.height(), dims), (100, 50, Some((4000, 2000))));
        // Too big for a normal thumbnail request
        assert!(load_helper_png(&data, 64).is_none());
        // Cut off after the header
        assert!(load_helper_png(&data[..data.len() / 2], 128).is_none());
    }

    #[test]
    fn dims_from_text() {
        let t = |w: &str, h: &str| {
            let kv = |k: &str, v: &str| (k.to_string(), v.to_string());
            vec![kv("Thumb::Image::Width", w), kv("Thumb::Image::Height", h)]
        };
        assert_eq!(helper_dims(&t("800", "600")), Some((800, 600)));
        assert_eq!(helper_dims(&t("0", "600")), None);
        assert_eq!(helper_dims(&t("x", "600")), None);
        assert_eq!(helper_dims(&[]), None);
    }

    #[test]
    fn input_name_keeps_plain_extensions() {
        assert_eq!(box_input(Path::new("/a/b.JPG")), "/tmp/input.JPG");
        assert_eq!(box_input(Path::new("/a/b")), "/tmp/input");
        assert_eq!(box_input(Path::new("/a/b.tar.gz")), "/tmp/input.gz");
        assert_eq!(box_input(Path::new("/a/b.p g")), "/tmp/input");
        assert_eq!(box_input(Path::new("/a/b.verylongextension")), "/tmp/input");
    }

    #[test]
    fn sandbox_arguments() {
        let roots = [("/lib", Some(PathBuf::from("usr/lib"))), ("/lib64", None)];
        let a = bwrap_args(
            Path::new("/opt/rf/rfiles"),
            Some(Path::new("/home/me")),
            &roots,
            Some((Path::new("/media/usb/x.png"), Path::new("/tmp/rfiles-thumb-1-0"))),
        );
        let a: Vec<&str> = a.iter().map(|s| s.to_str().unwrap()).collect();
        let has = |w: &[&str]| a.windows(w.len()).any(|x| x == w);
        assert!(has(&["--unshare-all", "--die-with-parent", "--new-session"]));
        assert!(has(&["--ro-bind", "/usr", "/usr"]));
        assert!(has(&["--symlink", "usr/lib", "/lib"]));
        assert!(has(&["--ro-bind", "/lib64", "/lib64"]));
        assert!(has(&["--tmpfs", "/tmp"]) && has(&["--tmpfs", "/home/me"]));
        assert!(has(&["--ro-bind", "/opt/rf/rfiles", BOX_EXE]));
        assert!(has(&["--ro-bind", "/media/usb/x.png", "/tmp/input.png"]));
        assert!(has(&["--bind", "/tmp/rfiles-thumb-1-0", BOX_OUT_DIR]));
        assert_eq!(a.last(), Some(&BOX_EXE));
        // The only writable bind is the output folder, and binds under /tmp come after its tmpfs
        assert_eq!(a.iter().filter(|&&x| x == "--bind").count(), 1);
        let pos = |x: &str| a.iter().position(|&y| y == x).unwrap();
        assert!(pos("/tmp") < pos(BOX_EXE) && pos("/tmp") < pos(BOX_OUT_DIR));
        // Nothing of the home folder or the rest of the system is visible
        assert!(!a.contains(&"/home/me/.cache") && !a.contains(&"/etc") && !a.contains(&"/media/usb"));

        // Without a job: just the binary; an odd HOME gets no tmpfs
        let b = bwrap_args(Path::new("/usr/bin/rfiles"), Some(Path::new("/")), &[], None);
        let b: Vec<&str> = b.iter().map(|s| s.to_str().unwrap()).collect();
        assert!(!b.contains(&"--bind") && !b.windows(2).any(|x| x == ["--tmpfs", "/"]));
        assert_eq!(b.iter().filter(|&&x| x == "--tmpfs").count(), 1);
    }
}

/// Runs the built `rfiles` binary as the helper, through bwrap and without it, like the app does.
#[cfg(test)]
mod helper_run_tests {
    use super::*;

    /// The `rfiles` binary cargo builds next to this test binary (target/<profile>/deps/..).
    fn rfiles_exe() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?.parent()?.parent()?.join("rfiles");
        exe.is_file().then_some(exe)
    }

    /// A minimal one-page PDF with a filled rectangle.
    fn pdf() -> Vec<u8> {
        let content = "0 0 1 rg 20 20 160 100 re f";
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 300] /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        let trailer = format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1);
        out.extend_from_slice(trailer.as_bytes());
        out
    }

    struct Files {
        dir: PathBuf,
    }

    impl Drop for Files {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn files(tag: &str) -> Files {
        let dir = std::env::temp_dir().join(format!("rfiles-helper-run-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pb = Pixbuf::new(Colorspace::Rgb, false, 8, 300, 200).unwrap();
        pb.fill(0x2266aaff);
        pb.savev(dir.join("good.png"), "png", &[]).unwrap();
        pb.savev(dir.join("good.jpg"), "jpeg", &[]).unwrap();
        std::fs::write(dir.join("doc.pdf"), pdf()).unwrap();
        let png = std::fs::read(dir.join("good.png")).unwrap();
        std::fs::write(dir.join("cut.png"), &png[..60]).unwrap();
        let mut garbage = png[..33].to_vec();
        garbage.extend((0..4000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8));
        std::fs::write(dir.join("garbage.png"), garbage).unwrap();
        std::fs::write(dir.join("notes.png"), "just some text, not an image\n").unwrap();
        Files { dir }
    }

    fn check_all(exe: &Path, bwrap: Option<&Path>, f: &Files) {
        let run = |name: &str, mime: &str, px: i32| run_helper(exe, bwrap, &f.dir.join(name), mime, px);
        match run("good.png", "image/png", 128) {
            Helper::Done(pb, dims) => {
                assert_eq!((pb.width(), pb.height()), (128, 85));
                assert_eq!(dims, Some((300, 200)));
            }
            _ => panic!("good.png gave no thumbnail"),
        }
        match run("good.jpg", "image/jpeg", 256) {
            // Smaller than the request: kept at its own size
            Helper::Done(pb, dims) => assert_eq!((pb.width(), pb.height(), dims), (256, 171, Some((300, 200)))),
            _ => panic!("good.jpg gave no thumbnail"),
        }
        match run("doc.pdf", "application/pdf", 128) {
            Helper::Done(pb, dims) => assert_eq!((pb.width(), pb.height(), dims), (85, 128, None)),
            _ => panic!("doc.pdf gave no thumbnail"),
        }
        for (name, mime) in [("cut.png", "image/png"), ("garbage.png", "image/png"), ("notes.png", "image/png")] {
            assert!(matches!(run(name, mime, 128), Helper::Failed), "{name} should fail");
        }
        assert!(matches!(run("notes.png", "application/pdf", 128), Helper::Failed));
        // Oversized request and a missing file
        assert!(matches!(run("good.png", "image/png", 5000), Helper::Failed));
        assert!(matches!(run("missing.png", "image/png", 128), Helper::Failed));
    }

    #[test]
    fn helper_without_sandbox() {
        let Some(exe) = rfiles_exe() else {
            eprintln!("SKIPPED: the rfiles binary isn't built next to the tests");
            return;
        };
        check_all(&exe, None, &files("direct"));
    }

    #[test]
    fn helper_in_sandbox() {
        let Some(exe) = rfiles_exe() else {
            eprintln!("SKIPPED: the rfiles binary isn't built next to the tests");
            return;
        };
        let Some(bwrap) = find_bwrap() else {
            eprintln!("SKIPPED: bwrap is not installed");
            return;
        };
        if !sandbox_works(&bwrap, &exe) {
            eprintln!("SKIPPED: bwrap can't create a sandbox here (user namespaces restricted?)");
            return;
        }
        check_all(&exe, Some(&bwrap), &files("bwrap"));
    }

    #[test]
    fn sandbox_hides_the_system() {
        let Some(bwrap) = find_bwrap() else {
            eprintln!("SKIPPED: bwrap is not installed");
            return;
        };
        let sh = Path::new("/usr/bin/sh");
        let mut probe = limited(&bwrap);
        probe.args(bwrap_args(sh, None, &root_entries(), None)).args(["-c", "exit 0"]);
        if !run_limited(probe).unwrap_or(false) {
            eprintln!("SKIPPED: bwrap can't create a sandbox here (user namespaces restricted?)");
            return;
        }
        let f = files("confine");
        let input = f.dir.join("good.png");
        let home = glib::home_dir();
        // Only the input and the output folder are there; home is empty; /usr is read-only; only loopback
        let script = format!(
            "test -r {inp} && test ! -e {real} && test -z \"$(ls -A {home})\" && test ! -e /etc/passwd \
             && ! touch /usr/x 2>/dev/null && touch {out}/ok \
             && test \"$(wc -l < /proc/net/dev)\" -le 3",
            inp = box_input(&input),
            real = input.display(),
            home = home.display(),
            out = BOX_OUT_DIR,
        );
        let mut cmd = limited(&bwrap);
        cmd.args(bwrap_args(sh, Some(&home), &root_entries(), Some((&input, &f.dir)))).args(["-c", &script]);
        assert!(run_limited(cmd).unwrap());
        assert!(f.dir.join("ok").exists());
    }

    #[test]
    fn limits_and_exit_status() {
        let sh = Path::new("/bin/sh");
        let mut ok = limited(sh);
        ok.args(["-c", "exit 0"]);
        assert!(run_limited(ok).unwrap());
        let mut bad = limited(sh);
        bad.args(["-c", "exit 3"]);
        assert!(!run_limited(bad).unwrap());
        assert!(run_limited(limited(Path::new("/nonexistent/rfiles"))).is_err());
        // The environment is cleared and core dumps are off
        let mut env = limited(sh);
        env.args(["-c", r#"test -z "$DBUS_SESSION_BUS_ADDRESS$DISPLAY$WAYLAND_DISPLAY" && test "$(ulimit -c)" = 0"#]);
        assert!(run_limited(env).unwrap());
        // The address-space limit holds
        let mut mem = limited(sh);
        mem.args(["-c", &format!("test \"$(ulimit -v)\" = {}", HELPER_MEMORY / 1024)]);
        assert!(run_limited(mem).unwrap());
    }

    #[test]
    fn timeout_kills_the_whole_group() {
        let pid_file = std::env::temp_dir().join(format!("rfiles-helper-timeout-{}", std::process::id()));
        let mut cmd = limited(Path::new("/bin/sh"));
        cmd.args(["-c", &format!("sleep 30 & echo $! > {}; wait", pid_file.display())]);
        let start = std::time::Instant::now();
        assert!(!run_for(cmd, std::time::Duration::from_millis(300)).unwrap());
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        std::fs::remove_file(&pid_file).unwrap();
        // The background child is gone too (or a zombie waiting for init)
        std::thread::sleep(std::time::Duration::from_millis(100));
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())).unwrap_or_default();
        assert!(stat.is_empty() || stat.contains(") Z "), "{stat}");
    }
}

// ======================= UI side =======================

/// A tile that wants a thumbnail.
struct Want {
    uri: String,
    path: PathBuf,
    mime: String,
    size: Size,
    load: Option<PathBuf>,
    li: glib::WeakRef<gtk::ListItem>,
    img: glib::WeakRef<gtk::Image>,
    info: glib::WeakRef<gio::FileInfo>,
}

impl Want {
    /// The tile, if it still shows this file.
    fn tile(&self) -> Option<(gtk::ListItem, gtk::Image)> {
        let (li, img, info) = (self.li.upgrade()?, self.img.upgrade()?, self.info.upgrade()?);
        (li.item().as_ref() == Some(info.upcast_ref::<glib::Object>())).then_some((li, img))
    }
}

#[derive(Default)]
struct Ui {
    next_id: u64,
    /// Bound tiles (by list item address) waiting for a worker
    wanted: HashMap<usize, Want>,
    /// Tile -> its request that is queued or running
    by_slot: HashMap<usize, u64>,
    /// Requests that are queued or running
    pending: HashMap<u64, (usize, Want)>,
    /// Jobs handed to the workers whose result hasn't come back
    outstanding: usize,
    /// List items we watch for destruction
    live: HashSet<usize>,
    pump_scheduled: bool,
    /// Tiles were bound or moved since the last check
    dirty: bool,
    /// Files that failed or were skipped this session
    skip: HashSet<String>,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
}

impl Ui {
    /// Drops whatever this tile asked for before.
    fn forget_slot(&mut self, slot: usize) {
        self.wanted.remove(&slot);
        if let Some(old) = self.by_slot.remove(&slot) {
            self.pending.remove(&old);
            let mut jobs = queue().jobs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(i) = jobs.iter().position(|j| j.id == old) {
                jobs.remove(i);
                // It will never report back
                self.outstanding = self.outstanding.saturating_sub(1);
            }
        }
    }

    fn supported(&mut self, mime: &str) -> bool {
        renders_itself(mime) || thumbnailer_for(mime).is_some()
    }

    fn schedule_pump(&mut self) {
        if !self.pump_scheduled && !self.wanted.is_empty() {
            self.pump_scheduled = true;
            glib::idle_add_local_once(pump);
        }
    }
}

/// How far (in pixels) a tile is outside its scrolled window's visible
/// area (0 when visible) and the height of that area. None when the tile
/// isn't shown at all.
fn distance_from_view(img: &gtk::Image) -> Option<(f32, f32)> {
    // Tiles GTK keeps around off screen are mapped but get no size
    if !img.is_mapped() || img.height() == 0 || img.width() == 0 {
        return None;
    }
    let scroll = img.ancestor(gtk::ScrolledWindow::static_type())?;
    let p = img.compute_point(&scroll, &gtk::graphene::Point::new(0.0, 0.0))?;
    let (top, bottom, view) = (p.y(), p.y() + img.height() as f32, scroll.height() as f32);
    let d = if bottom < 0.0 {
        -bottom
    } else if top > view {
        top - view
    } else {
        0.0
    };
    Some((d, view))
}

/// Hands the workers the tiles in (or closest to) the visible area. GTK
/// keeps many more tiles bound than are on screen (up to ~2000 in a wide
/// grid), so being bound alone doesn't mean the user can see it.
fn pump() {
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        ui.pump_scheduled = false;
        let free = MAX_OUTSTANDING.saturating_sub(ui.outstanding);
        if free == 0 || ui.wanted.is_empty() {
            return;
        }
        let mut best: Vec<(f32, u32, usize)> = Vec::new();
        let mut gone = Vec::new();
        for (&slot, w) in &ui.wanted {
            // Most bound tiles are off screen: rule them out cheaply first
            let Some(img) = w.img.upgrade() else {
                gone.push(slot);
                continue;
            };
            // Visible tiles first, then up to half a screen around them
            let Some((d, view)) = distance_from_view(&img) else { continue };
            if d > view / 2.0 {
                continue;
            }
            match w.tile() {
                Some((li, _)) => best.push((d, li.position(), slot)),
                None => gone.push(slot),
            }
        }
        for slot in gone {
            ui.wanted.remove(&slot);
        }
        if best.is_empty() {
            return;
        }
        best.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let q = queue();
        let mut jobs = q.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for (_, _, slot) in best.into_iter().take(free) {
            let Some(w) = ui.wanted.remove(&slot) else { continue };
            ui.next_id += 1;
            let id = ui.next_id;
            jobs.push_back(Job {
                id,
                uri: w.uri.clone(),
                path: w.path.clone(),
                mime: w.mime.clone(),
                size: w.size,
                load: w.load.clone(),
            });
            ui.outstanding += 1;
            ui.by_slot.insert(slot, id);
            ui.pending.insert(id, (slot, w));
        }
        drop(jobs);
        q.cv.notify_all();
    });
}

/// Marks that tiles may have moved or appeared. The check runs after the
/// next frame is painted, when GTK has laid the tiles out.
pub fn mark_dirty() {
    UI.with(|ui| ui.borrow_mut().dirty = true);
}

fn after_frame(_: &gdk::FrameClock) {
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        if std::mem::take(&mut ui.dirty) {
            ui.schedule_pump();
        }
    });
}

/// Watches a scrolled window of tiles so that new, scrolled or resized
/// tiles are checked once they are laid out. Call once per grid, before
/// the window is shown.
pub fn watch_scroll(scroll: &gtk::ScrolledWindow) {
    let adj = scroll.vadjustment();
    adj.connect_value_changed(|_| mark_dirty());
    adj.connect_changed(|_| mark_dirty());
    scroll.connect_realize(|s| {
        if let Some(fc) = s.frame_clock() {
            fc.connect_after_paint(after_frame);
        }
    });
}

/// Called from a grid tile's bind, after `set_icon(.., true)`. Notes that
/// the tile wants a thumbnail when its file needs one (an existing one to
/// load, or a new one to make), and drops the tile's older request.
pub fn bind(li: &gtk::ListItem, img: &gtk::Image, info: &gio::FileInfo) {
    bind_with(li, img, info, true);
}

/// Like `bind`, but only loads thumbnails that already exist (the folder
/// preview doesn't make new ones).
pub fn bind_existing(li: &gtk::ListItem, img: &gtk::Image, info: &gio::FileInfo) {
    bind_with(li, img, info, false);
}

fn bind_with(li: &gtk::ListItem, img: &gtk::Image, info: &gio::FileInfo, make: bool) {
    let slot = li.as_ptr() as usize;
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        ui.forget_slot(slot);
        if ui.live.insert(slot) {
            // A destroyed tile's request is dropped too
            li.add_weak_ref_notify_local(move || {
                UI.with(|ui| {
                    if let Ok(mut ui) = ui.try_borrow_mut() {
                        ui.live.remove(&slot);
                        ui.forget_slot(slot);
                    }
                })
            });
        }

        if info.attribute_string("metadata::custom-icon").is_some_and(|s| !s.is_empty()) {
            return;
        }
        let want = Size::for_pixels(ICON_SIZE.with(|s| s.get()) * img.scale_factor().max(1));
        if let Some(p) = info.attribute_byte_string("thumbnail::path").filter(|p| !p.is_empty()) {
            let valid = !info.has_attribute("thumbnail::is-valid") || info.boolean("thumbnail::is-valid");
            let too_small = make && want == Size::Large && p.contains("/thumbnails/normal/");
            if valid && !too_small {
                // set_icon already showed it if it was decoded before
                if info.file_type() == gio::FileType::Directory || cached(info).is_some() {
                    return;
                }
                let file = item_file(info);
                let uri = file.uri().to_string();
                if ui.skip.contains(&uri) {
                    return;
                }
                ui.wanted.insert(
                    slot,
                    Want {
                        uri,
                        path: file.path().unwrap_or_default(),
                        mime: String::new(),
                        size: want,
                        load: Some(PathBuf::from(p.as_str())),
                        li: li.downgrade(),
                        img: img.downgrade(),
                        info: info.downgrade(),
                    },
                );
                ui.dirty = true;
                return;
            }
        }
        if !make || info.file_type() != gio::FileType::Regular {
            return;
        }
        let size = info.size();
        if size <= 0 || size > MAX_FILE_SIZE {
            return;
        }
        let Some(mime) = info.content_type() else { return };
        if !ui.supported(&mime) {
            return;
        }
        let file = item_file(info);
        let Some(path) = file.path() else { return };
        let uri = file.uri().to_string();
        if !uri.starts_with("file://") || ui.skip.contains(&uri) {
            return;
        }
        ui.wanted.insert(
            slot,
            Want {
                uri,
                path,
                mime: mime.to_string(),
                size: want,
                load: None,
                li: li.downgrade(),
                img: img.downgrade(),
                info: info.downgrade(),
            },
        );
        ui.dirty = true;
    });
}

/// A worker finished: update the tile if it still shows the same file.
fn deliver(id: u64, outcome: Outcome) {
    let w = UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        ui.outstanding = ui.outstanding.saturating_sub(1);
        ui.schedule_pump();
        let (slot, w) = ui.pending.remove(&id)?;
        if ui.by_slot.get(&slot) == Some(&id) {
            ui.by_slot.remove(&slot);
        }
        if !matches!(outcome, Outcome::Ready(..)) {
            if ui.skip.len() >= MAX_SKIP {
                ui.skip.clear();
            }
            ui.skip.insert(w.uri.clone());
        }
        Some(w)
    });
    let (Some(w), Outcome::Ready(path, px)) = (w, outcome) else { return };
    let fmt = if px.alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
    let tex = gdk::MemoryTexture::new(px.w, px.h, fmt, &px.bytes, px.stride);
    if let (Some(info), false) = (w.info.upgrade(), path.as_os_str().is_empty()) {
        // Later binds of this item use the new file directly
        info.set_attribute_byte_string("thumbnail::path", path.to_string_lossy().as_ref());
        info.set_attribute_boolean("thumbnail::is-valid", true);
        // Kept even if the tile moved on, for when it scrolls back
        let bytes = texture_bytes(px.w, px.h, px.alpha);
        TEXTURES.with(|t| t.borrow_mut().insert(path, mtime(&info), bytes, tex.clone().upcast()));
    }
    let Some((_, img)) = w.tile() else { return };
    img.set_paintable(Some(&tex));
}

// ======================= decoded texture cache =======================

thread_local! {
    static TEXTURES: RefCell<TexCache<gdk::Texture>> = RefCell::new(TexCache::new(TEXTURE_BUDGET));
}

fn mtime(info: &gio::FileInfo) -> i64 {
    info.attribute_uint64("time::modified") as i64
}

/// Memory a decoded thumbnail takes: 3 or 4 bytes per pixel.
fn texture_bytes(w: i32, h: i32, alpha: bool) -> usize {
    w.max(0) as usize * h.max(0) as usize * if alpha { 4 } else { 3 }
}

/// The decoded thumbnail of this file, if it was shown recently and the
/// file hasn't changed since. No disk access.
pub fn cached(info: &gio::FileInfo) -> Option<gdk::Texture> {
    let p = info.attribute_byte_string("thumbnail::path").filter(|p| !p.is_empty())?;
    TEXTURES.with(|t| t.borrow_mut().get(Path::new(p.as_str()), mtime(info)))
}

struct CacheEntry<T> {
    mtime: i64,
    bytes: usize,
    tick: u64,
    value: T,
}

/// Values keyed by thumbnail path, valid for one mtime of the file, within
/// a byte budget; the least recently used go first.
struct TexCache<T> {
    budget: usize,
    used: usize,
    tick: u64,
    map: HashMap<PathBuf, CacheEntry<T>>,
    /// Last use -> key, oldest first
    order: BTreeMap<u64, PathBuf>,
}

impl<T: Clone> TexCache<T> {
    fn new(budget: usize) -> Self {
        TexCache { budget, used: 0, tick: 0, map: HashMap::new(), order: BTreeMap::new() }
    }

    fn get(&mut self, key: &Path, mtime: i64) -> Option<T> {
        let e = self.map.get_mut(key).filter(|e| e.mtime == mtime)?;
        self.order.remove(&e.tick);
        self.tick += 1;
        e.tick = self.tick;
        self.order.insert(self.tick, key.to_path_buf());
        Some(e.value.clone())
    }

    fn remove(&mut self, key: &Path) {
        if let Some(e) = self.map.remove(key) {
            self.order.remove(&e.tick);
            self.used -= e.bytes;
        }
    }

    fn insert(&mut self, key: PathBuf, mtime: i64, bytes: usize, value: T) {
        // Empty or larger than everything: not worth keeping
        if bytes == 0 || bytes > self.budget {
            return;
        }
        if self.map.get(&key).is_some_and(|e| e.mtime > mtime) {
            return;
        }
        self.remove(&key);
        while self.used + bytes > self.budget {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            if let Some(e) = self.map.remove(&oldest) {
                self.used -= e.bytes;
            }
        }
        self.tick += 1;
        self.order.insert(self.tick, key.clone());
        self.map.insert(key, CacheEntry { mtime, bytes, tick: self.tick, value });
        self.used += bytes;
    }
}

#[cfg(test)]
mod tex_cache_tests {
    use super::{texture_bytes, TexCache};
    use std::path::{Path, PathBuf};

    fn k(n: &str) -> PathBuf {
        PathBuf::from(format!("/c/{n}.png"))
    }

    #[test]
    fn bytes_from_dimensions() {
        assert_eq!(texture_bytes(128, 96, true), 128 * 96 * 4);
        assert_eq!(texture_bytes(256, 256, false), 256 * 256 * 3);
        assert_eq!(texture_bytes(0, 256, true), 0);
        assert_eq!(texture_bytes(-1, 5, true), 0);
    }

    #[test]
    fn stays_within_budget_evicting_least_recent() {
        let mut c = TexCache::new(100);
        c.insert(k("a"), 1, 40, 'a');
        c.insert(k("b"), 1, 40, 'b');
        // Using "a" makes "b" the oldest
        assert_eq!(c.get(&k("a"), 1), Some('a'));
        c.insert(k("c"), 1, 40, 'c');
        assert_eq!(c.used, 80);
        assert_eq!(c.get(&k("b"), 1), None);
        assert_eq!(c.get(&k("a"), 1), Some('a'));
        assert_eq!(c.get(&k("c"), 1), Some('c'));
        // A big one pushes out as many as needed, oldest first
        c.insert(k("d"), 1, 90, 'd');
        assert_eq!(c.used, 90);
        assert_eq!(c.map.len(), 1);
        assert_eq!(c.order.len(), 1);
        assert_eq!(c.get(&k("d"), 1), Some('d'));
    }

    #[test]
    fn mtime_must_match_and_newer_replaces() {
        let mut c = TexCache::new(100);
        c.insert(k("a"), 10, 30, 'x');
        assert_eq!(c.get(&k("a"), 11), None);
        assert_eq!(c.get(Path::new("/c/other.png"), 10), None);
        c.insert(k("a"), 11, 50, 'y');
        assert_eq!(c.get(&k("a"), 10), None);
        assert_eq!(c.get(&k("a"), 11), Some('y'));
        assert_eq!(c.used, 50);
        // A late result for the older file doesn't win
        c.insert(k("a"), 10, 30, 'x');
        assert_eq!(c.get(&k("a"), 11), Some('y'));
        assert_eq!((c.used, c.map.len(), c.order.len()), (50, 1, 1));
        // Same mtime again replaces in place
        c.insert(k("a"), 11, 20, 'z');
        assert_eq!(c.get(&k("a"), 11), Some('z'));
        assert_eq!((c.used, c.map.len(), c.order.len()), (20, 1, 1));
    }

    #[test]
    fn zero_size_and_oversized_are_not_kept() {
        let mut c = TexCache::new(100);
        c.insert(k("a"), 1, 60, 'a');
        c.insert(k("z"), 1, 0, 'z');
        c.insert(k("big"), 1, 101, 'b');
        assert_eq!(c.get(&k("z"), 1), None);
        assert_eq!(c.get(&k("big"), 1), None);
        // Nothing was evicted for them
        assert_eq!(c.get(&k("a"), 1), Some('a'));
        assert_eq!(c.used, 60);
        // Exactly the budget fits
        c.insert(k("full"), 1, 100, 'f');
        assert_eq!(c.get(&k("full"), 1), Some('f'));
        assert_eq!((c.used, c.map.len()), (100, 1));
    }

    #[test]
    fn empty_until_used() {
        let c: TexCache<char> = TexCache::new(100);
        assert_eq!((c.used, c.map.capacity(), c.order.len()), (0, 0, 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_md5_of_uri() {
        // Example from the freedesktop thumbnail spec
        assert_eq!(thumb_name("file:///home/jens/photos/me.png"), "c6ee772d9e49320e97ec29a7eb5b1697.png");
    }

    #[test]
    fn name_uses_escaped_uri() {
        let uri = gio::File::for_path("/tmp/a b/ü.png").uri().to_string();
        assert_eq!(uri, "file:///tmp/a%20b/%C3%BC.png");
        assert_eq!(thumb_name(&uri).len(), 36);
        assert_ne!(thumb_name(&uri), thumb_name("file:///tmp/a b/ü.png"));
    }

    #[test]
    fn size_choice() {
        assert_eq!(Size::for_pixels(40), Size::Normal);
        assert_eq!(Size::for_pixels(128), Size::Normal);
        assert_eq!(Size::for_pixels(144), Size::Large);
        assert_eq!(Size::Large.px(), 256);
        assert_eq!(Size::Normal.dir(), "normal");
    }

    #[test]
    fn spec_metadata_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rfiles-thumb-test-{}", std::process::id()));
        let target = dir.join("t.png");
        let pb = Pixbuf::new(Colorspace::Rgb, false, 8, 4, 3).unwrap();
        pb.fill(0x336699ff);
        let uri = "file:///tmp/x%20y.jpg";
        let opts = spec_metadata(uri, 1_700_000_000, 1234, "image/jpeg", Some((800, 600)));
        write_atomic(&pb, &dir, &target, &opts).unwrap();

        let text = read_text_chunks(&target).unwrap();
        assert!(metadata_matches(&text, uri, 1_700_000_000));
        assert!(!metadata_matches(&text, uri, 1_700_000_001));
        assert!(!metadata_matches(&text, "file:///tmp/other.jpg", 1_700_000_000));
        let get = |k: &str| text.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("Thumb::Size").as_deref(), Some("1234"));
        assert_eq!(get("Thumb::Image::Width").as_deref(), Some("800"));
        assert_eq!(get("Thumb::Mimetype").as_deref(), Some("image/jpeg"));
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // No temp files left behind
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn png_text_parsing() {
        assert!(png_text(b"not a png").is_empty());
        // Signature + truncated chunk must not panic
        let mut d = b"\x89PNG\r\n\x1a\n".to_vec();
        d.extend_from_slice(&[0, 0, 1, 0, b't', b'E', b'X', b't', b'a']);
        assert!(png_text(&d).is_empty());
        // A tEXt and an iTXt chunk
        let mut d = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, body) in [
            (&b"tEXt"[..], &b"Thumb::URI\0file:///a.png"[..]),
            (&b"iTXt"[..], &b"Thumb::MTime\0\0\0\0\042"[..]),
        ] {
            d.extend_from_slice(&(body.len() as u32).to_be_bytes());
            d.extend_from_slice(kind);
            d.extend_from_slice(body);
            d.extend_from_slice(&[0; 4]);
        }
        let t = png_text(&d);
        assert!(metadata_matches(&t, "file:///a.png", 42));
    }
}
