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
const MAX_FULL_DECODE_PIXELS: i64 = 40_000_000;
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

    let rendered = if job.mime == "application/pdf" {
        render_pdf(&job.uri, job.size.px()).map(|pb| (pb, None))
    } else {
        render_image(&job.path, job.size.px())
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
    /// MIME types gdk-pixbuf can load (filled on first use)
    mimes: Option<HashSet<String>>,
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
        if mime == "application/pdf" {
            return true;
        }
        self.mimes
            .get_or_insert_with(|| {
                Pixbuf::formats()
                    .iter()
                    .filter(|f| !f.is_disabled())
                    .flat_map(|f| f.mime_types())
                    .map(|m| m.to_string())
                    .collect()
            })
            .contains(mime)
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
