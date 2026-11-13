//! Right-hand preview pane. Each preview is built only when a file is
//! selected and torn down when the selection changes, so nothing is held in
//! memory for files you are not looking at.

use crate::docs;
use crate::util::*;
use gtk::prelude::*;
use gtk::{cairo, gdk, gdk_pixbuf, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

const TEXT_LIMIT: usize = 256 * 1024;
const IMAGE_MAX: i32 = 2048;
/// Touchpad scroll distance, in pixels, that turns one PDF page.
const PDF_SCROLL_PX: f64 = 100.0;
/// Largest side, in device pixels, a PDF page is rendered at, so a huge pane can't take hundreds of MB.
const PDF_MAX_PX: f64 = 4096.0;
/// How long the pane size must stay put before the page is rendered again at the new size.
const PDF_SETTLE: Duration = Duration::from_millis(150);

/// Called with the previewed file when its Open button is pressed.
pub type OpenHandler = Rc<dyn Fn(&gio::FileInfo)>;

pub struct Preview {
    pub root: gtk::Box,
    body: gtk::Box,
    title: gtk::Label,
    meta: gtk::Label,
    open_btn: gtk::Button,
    gen: Cell<u64>,
    media: RefCell<Option<gtk::Video>>,
    current: RefCell<Option<gio::FileInfo>>,
    pub on_open: RefCell<Option<OpenHandler>>,
}

impl Preview {
    pub fn new() -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
        root.add_css_class("preview");
        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_vexpand(true);
        body.add_css_class("preview-body");

        let title = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .selectable(true)
            .build();
        title.add_css_class("preview-title");
        let meta = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(pango::WrapMode::WordChar)
            .use_markup(true)
            .selectable(true)
            .build();
        meta.add_css_class("meta");
        let open_btn = gtk::Button::with_label("Open");
        open_btn.add_css_class("suggested-action");
        open_btn.set_halign(gtk::Align::Start);

        root.append(&body);
        root.append(&title);
        root.append(&meta);
        root.append(&open_btn);

        let p = Rc::new(Preview {
            root,
            body,
            title,
            meta,
            open_btn,
            gen: Cell::new(0),
            media: RefCell::new(None),
            current: RefCell::new(None),
            on_open: RefCell::new(None),
        });
        let p2 = Rc::downgrade(&p);
        p.open_btn.connect_clicked(move |_| {
            let Some(p2) = p2.upgrade() else { return };
            let info = p2.current.borrow().clone();
            let cb = p2.on_open.borrow().clone();
            if let (Some(info), Some(cb)) = (info, cb) {
                cb(&info);
            }
        });
        p.clear();
        p
    }

    fn stop_media(&self) {
        if let Some(v) = self.media.take() {
            if let Some(s) = v.media_stream() {
                s.pause();
            }
            v.set_file(None::<&gio::File>);
        }
    }

    fn clear_body(&self) {
        self.stop_media();
        while let Some(c) = self.body.first_child() {
            self.body.remove(&c);
        }
    }

    pub fn clear(&self) {
        self.gen.set(self.gen.get() + 1);
        self.clear_body();
        *self.current.borrow_mut() = None;
        self.title.set_text("");
        self.meta.set_text("");
        self.open_btn.set_visible(false);
        let img = gtk::Image::from_icon_name("view-paged-symbolic");
        img.set_pixel_size(96);
        img.set_opacity(0.25);
        img.set_vexpand(true);
        img.set_valign(gtk::Align::End);
        let lbl = gtk::Label::new(Some("Click a file or folder to preview it"));
        lbl.add_css_class("dim");
        lbl.set_vexpand(true);
        lbl.set_valign(gtk::Align::Start);
        self.body.append(&img);
        self.body.append(&lbl);
    }

    /// Shown when several files are selected.
    pub fn show_summary(&self, count: usize, size: u64) {
        self.clear();
        self.title.set_text(&format!("{count} items selected"));
        if size > 0 {
            self.meta.set_markup(&format!("<b>Total size</b>   {}", glib::format_size(size)));
        }
    }

    pub fn show(self: &Rc<Self>, info: &gio::FileInfo) {
        self.clear_body();
        let g = self.gen.get() + 1;
        self.gen.set(g);
        *self.current.borrow_mut() = Some(info.clone());

        let file = item_file(info);
        let ct = info.content_type().map(|s| s.to_string()).unwrap_or_default();
        let path = file.path();
        let ext = path.as_deref().map(docs::ext_of).unwrap_or_default();

        self.title.set_text(&info.display_name());
        self.set_meta(info, &file, None);
        self.open_btn.set_visible(true);
        self.open_btn
            .set_label(if is_dir_like(info) { "Open folder" } else { "Open" });

        if is_dir_like(info) {
            self.folder_preview(info, &file);
            return;
        }
        let Some(path) = path else {
            self.icon_placeholder(info, "No preview for remote files");
            return;
        };
        // Pipes, sockets and devices: opening a pipe blocks until something writes to it
        if info.file_type() == gio::FileType::Special {
            self.icon_placeholder(info, "No preview for special files");
            return;
        }

        if ct.starts_with("image/") {
            self.image_preview(path, g, info.clone());
        } else if ct.starts_with("video/") {
            self.media_preview(&file, true, info);
        } else if ct.starts_with("audio/") {
            self.media_preview(&file, false, info);
        } else if ct == "application/pdf" || ext == "pdf" {
            self.pdf_preview(&file, info, g);
        } else if docs::is_office(&ext) {
            self.async_text(info, g, false, move || docs::extract_text(&path));
        } else if ct == "application/zip" || ext == "zip" {
            self.async_text(info, g, true, move || docs::zip_listing(&path));
        } else {
            self.async_text(info, g, true, move || read_text(&path));
        }
    }

    fn set_meta(&self, info: &gio::FileInfo, file: &gio::File, extra: Option<&str>) {
        let mut rows: Vec<(&str, String)> = vec![("Type", type_description(info))];
        let size = format_size(info);
        if !size.is_empty() {
            rows.push(("Size", size));
        }
        if let Some(e) = extra {
            rows.push(("Contains", e.to_string()));
        }
        let t = format_time(info);
        if !t.is_empty() {
            rows.push(("Modified", t));
        }
        if let Some(orig) = info.attribute_byte_string("trash::orig-path") {
            rows.push(("Original", orig.to_string()));
        } else if let Some(parent) = file.parent() {
            rows.push(("Location", display_path(&parent)));
        }
        let markup = rows
            .iter()
            .map(|(k, v)| format!("<b>{k}</b>   {}", glib::markup_escape_text(v)))
            .collect::<Vec<_>>()
            .join("\n");
        self.meta.set_markup(&markup);
    }

    fn icon_placeholder(&self, info: &gio::FileInfo, msg: &str) {
        let img = gtk::Image::new();
        set_icon(&img, info, false);
        img.set_pixel_size(160);
        img.set_vexpand(true);
        img.set_valign(gtk::Align::End);
        let lbl = gtk::Label::new(Some(msg));
        lbl.add_css_class("dim");
        lbl.set_vexpand(true);
        lbl.set_valign(gtk::Align::Start);
        self.body.append(&img);
        self.body.append(&lbl);
    }

    // ---------- folders ----------
    fn folder_preview(self: &Rc<Self>, info: &gio::FileInfo, file: &gio::File) {
        let dl = gtk::DirectoryList::new(
            Some("standard::name,standard::display-name,standard::icon,standard::type,standard::is-hidden,standard::is-backup,thumbnail::path,standard::target-uri,standard::content-type,standard::size,time::modified,metadata::custom-icon"),
            Some(file),
        );
        dl.set_monitored(false);
        let filter = gtk::CustomFilter::new(|o| {
            o.downcast_ref::<gio::FileInfo>()
                .map(|i| !i.is_hidden() && !i.is_backup())
                .unwrap_or(false)
        });
        let fm = gtk::FilterListModel::new(Some(dl.clone()), Some(filter));
        let sm = gtk::SortListModel::new(Some(fm), Some(gtk::CustomSorter::new(sort_infos)));
        let sel = gtk::NoSelection::new(Some(sm.clone()));

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, obj| {
            let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let bx = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let img = gtk::Image::new();
            img.set_pixel_size(48);
            let lbl = gtk::Label::builder()
                .ellipsize(pango::EllipsizeMode::Middle)
                .max_width_chars(11)
                .width_chars(11)
                .build();
            lbl.add_css_class("small");
            bx.append(&img);
            bx.append(&lbl);
            li.set_child(Some(&bx));
        });
        factory.connect_bind(|_, obj| {
            let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(info) = li.item().and_downcast::<gio::FileInfo>() else { return };
            let bx = li.child().and_downcast::<gtk::Box>().unwrap();
            let img = bx.first_child().and_downcast::<gtk::Image>().unwrap();
            let lbl = img.next_sibling().and_downcast::<gtk::Label>().unwrap();
            set_icon(&img, &info, true);
            lbl.set_text(&info.display_name());
        });

        let grid = gtk::GridView::new(Some(sel), Some(factory));
        grid.set_max_columns(12);
        grid.add_css_class("files");
        grid.add_css_class("preview-grid");
        let p = Rc::downgrade(self);
        grid.connect_activate(move |g, pos| {
            let Some(p) = p.upgrade() else { return };
            let info = g.model().and_then(|m| m.item(pos)).and_downcast::<gio::FileInfo>();
            let cb = p.on_open.borrow().clone();
            if let (Some(info), Some(cb)) = (info, cb) {
                cb(&info);
            }
        });
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&grid)
            .build();
        scroll.add_css_class("files");

        let empty = gtk::Label::new(Some("Loading…"));
        empty.add_css_class("dim");
        empty.set_vexpand(true);

        let stack = gtk::Stack::new();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&scroll, Some("grid"));
        stack.set_vexpand(true);
        self.body.append(&stack);

        // Everything here is held weakly: these handlers live on the models,
        // which the widgets below keep alive.
        let p = Rc::downgrade(self);
        let (stack, empty) = (stack.downgrade(), empty.downgrade());
        let info = info.clone();
        let file = file.clone();
        let g = self.gen.get();
        let update = move |dl: &gtk::DirectoryList, n: u32| {
            let (Some(p), Some(stack), Some(empty)) = (p.upgrade(), stack.upgrade(), empty.upgrade()) else { return };
            if p.gen.get() != g {
                return;
            }
            if n > 0 {
                stack.set_visible_child_name("grid");
            } else if !dl.is_loading() {
                empty.set_text(match dl.error() {
                    Some(_) => "Can't read this folder",
                    None => "Empty folder",
                });
                stack.set_visible_child_name("empty");
            }
            let unit = if n == 1 { "item" } else { "items" };
            let txt = if dl.is_loading() { format!("{n} {unit}…") } else { format!("{n} {unit}") };
            p.set_meta(&info, &file, Some(&txt));
        };
        let update = Rc::new(update);
        let (u1, dl1) = (update.clone(), dl.downgrade());
        sm.connect_items_changed(move |m, _, _, _| {
            if let Some(dl) = dl1.upgrade() {
                u1(&dl, m.n_items())
            }
        });
        let sm3 = sm.downgrade();
        dl.connect_loading_notify(move |dl| {
            if let Some(sm) = sm3.upgrade() {
                update(dl, sm.n_items())
            }
        });
    }

    // ---------- images ----------
    fn image_preview(self: &Rc<Self>, path: PathBuf, g: u64, info: gio::FileInfo) {
        let pic = gtk::Picture::new();
        pic.set_content_fit(gtk::ContentFit::Contain);
        pic.set_can_shrink(true);
        pic.set_vexpand(true);
        pic.set_hexpand(true);
        self.body.append(&pic);
        let p = self.clone();
        glib::spawn_future_local(async move {
            // Decode off the main thread, and never bigger than 2048px so a
            // huge photo doesn't eat hundreds of MB.
            let res = gio::spawn_blocking(move || load_scaled(&path)).await;
            if p.gen.get() != g {
                return;
            }
            match res {
                Ok(Some((bytes, w, h, stride, alpha, (ow, oh)))) => {
                    let fmt = if alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
                    let tex = gdk::MemoryTexture::new(w, h, fmt, &bytes, stride as usize);
                    pic.set_paintable(Some(&tex));
                    let file = item_file(&info);
                    p.set_meta(&info, &file, None);
                    let extra = format!("\n<b>Dimensions</b>   {ow} × {oh}");
                    p.meta.set_markup(&format!("{}{}", p.meta.label(), extra));
                }
                _ => {
                    p.body.remove(&pic);
                    p.icon_placeholder(&info, "Can't preview this image");
                }
            }
        });
    }

    // ---------- audio / video ----------
    fn media_preview(&self, file: &gio::File, is_video: bool, info: &gio::FileInfo) {
        let v = gtk::Video::new();
        v.set_autoplay(false);
        v.set_file(Some(file));
        v.add_css_class("media");
        if is_video {
            v.set_vexpand(true);
        } else {
            let img = gtk::Image::new();
            set_icon(&img, info, false);
            img.set_pixel_size(160);
            img.set_vexpand(true);
            self.body.append(&img);
            v.set_size_request(-1, 56);
        }
        self.body.append(&v);
        *self.media.borrow_mut() = Some(v);
    }

    // ---------- PDF ----------
    fn pdf_preview(self: &Rc<Self>, file: &gio::File, info: &gio::FileInfo, g: u64) {
        let spinner = gtk::Spinner::new();
        spinner.start();
        spinner.set_vexpand(true);
        self.body.append(&spinner);
        // poppler::Document isn't Send, so one thread opens it and keeps it for this preview's page renders.
        // A plain thread rather than the GIO pool, since it lives as long as the preview.
        let (tx, rx) = mpsc::channel();
        let (otx, orx) = mpsc::channel();
        let uri = file.uri().to_string();
        let spawned = std::thread::Builder::new()
            .name("pdf-preview".into())
            .spawn(move || pdf_worker(&uri, otx, rx));
        let p = self.clone();
        let info = info.clone();
        glib::spawn_future_local(async move {
            let res = match spawned {
                Ok(_) => gio::spawn_blocking(move || orx.recv()).await.ok().and_then(|r| r.ok()),
                Err(e) => Some(Err(e.to_string())),
            };
            // Returning drops `tx`, which ends the worker
            if p.gen.get() != g {
                return;
            }
            p.body.remove(&spinner);
            match res {
                Some(Ok(n)) => p.pdf_view(n, tx),
                Some(Err(msg)) => p.icon_placeholder(&info, &format!("Can't open PDF: {msg}")),
                None => p.icon_placeholder(&info, "Can't open PDF"),
            }
        });
    }

    fn pdf_view(&self, n: i32, tx: mpsc::Sender<PdfReq>) {
        let area = gtk::DrawingArea::new();
        area.set_vexpand(true);
        area.set_hexpand(true);
        let spinner = gtk::Spinner::new();
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        spinner.set_size_request(32, 32);
        spinner.set_visible(false);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&area));
        overlay.add_overlay(&spinner);

        // The widget closures own this (and so the worker's sender); it holds the widgets weakly
        let v = Rc::new(PdfView {
            tx,
            page: Cell::new(0),
            shown: RefCell::new(None),
            flight: RefCell::new(Latest::default()),
            settle: RefCell::new(None),
            area: area.downgrade(),
            spinner: spinner.downgrade(),
        });
        // Only paints the cached page: rendering happens on the worker
        let v1 = v.clone();
        area.set_draw_func(move |_, cr, w, h| {
            let shown = v1.shown.borrow();
            let Some((key, surf)) = shown.as_ref() else { return };
            let sf = key.scale.max(1) as f64;
            let (lw, lh) = (surf.width() as f64 / sf, surf.height() as f64 / sf);
            // 1 when the page was rendered for this size; otherwise stretched until the re-render lands
            let s = (w as f64 / lw).min(h as f64 / lh);
            cr.translate(((w as f64 - lw * s) / 2.0).round(), ((h as f64 - lh * s) / 2.0).round());
            cr.scale(s, s);
            if cr.set_source_surface(surf, 0.0, 0.0).is_ok() {
                let _ = cr.paint();
            }
        });
        let v2 = v.clone();
        area.connect_resize(move |_, _, _| v2.schedule());
        let v3 = v.clone();
        area.connect_scale_factor_notify(move |_| v3.schedule());

        let prev = gtk::Button::from_icon_name("go-previous-symbolic");
        let next = gtk::Button::from_icon_name("go-next-symbolic");
        let label = gtk::Label::new(Some(&format!("1 / {n}")));
        let nav = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        nav.set_halign(gtk::Align::Center);
        nav.append(&prev);
        nav.append(&label);
        nav.append(&next);

        let go: Rc<dyn Fn(i32)> = {
            let label = label.clone();
            Rc::new(move |delta: i32| {
                let np = (v.page.get() + delta).clamp(0, n - 1);
                if np != v.page.get() {
                    v.page.set(np);
                    label.set_text(&format!("{} / {n}", np + 1));
                    v.update();
                }
            })
        };
        let g1 = go.clone();
        prev.connect_clicked(move |_| g1(-1));
        let g2 = go.clone();
        next.connect_clicked(move |_| g2(1));
        // A touchpad sends many small deltas per swipe, so add them up and turn a page only per full step
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let acc = Rc::new(Cell::new(0.0));
        let a = acc.clone();
        scroll.connect_scroll_begin(move |_| a.set(0.0));
        scroll.connect_scroll(move |c, _, dy| {
            let step = if c.unit() == gdk::ScrollUnit::Surface { PDF_SCROLL_PX } else { 1.0 };
            let (pages, rest) = scroll_pages(acc.get(), dy, step);
            acc.set(rest);
            if pages != 0 {
                go(pages)
            }
            glib::Propagation::Stop
        });
        area.add_controller(scroll);

        self.body.append(&overlay);
        self.body.append(&nav);
    }

    // ---------- text-like ----------
    fn async_text<F>(self: &Rc<Self>, info: &gio::FileInfo, g: u64, mono: bool, f: F)
    where
        F: FnOnce() -> Result<String, String> + Send + 'static,
    {
        let spinner = gtk::Spinner::new();
        spinner.start();
        spinner.set_vexpand(true);
        self.body.append(&spinner);
        let p = self.clone();
        let info = info.clone();
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(f).await;
            if p.gen.get() != g {
                return;
            }
            p.body.remove(&spinner);
            match res {
                Ok(Ok(text)) => p.body.append(&text_view(&text, mono)),
                _ => p.icon_placeholder(&info, "No preview available"),
            }
        });
    }
}

/// Decoded pixels (bytes, width, height, stride, has_alpha). Raw bytes
/// because a Pixbuf can't cross threads; the texture is made on the UI side.
type Pixels = (glib::Bytes, i32, i32, i32, bool, (i32, i32));

fn load_scaled(path: &Path) -> Option<Pixels> {
    let (_, w, h) = gdk_pixbuf::Pixbuf::file_info(path)?;
    let pb = if w > IMAGE_MAX || h > IMAGE_MAX {
        gdk_pixbuf::Pixbuf::from_file_at_scale(path, IMAGE_MAX, IMAGE_MAX, true).ok()?
    } else {
        gdk_pixbuf::Pixbuf::from_file(path).ok()?
    };
    // Respect camera rotation
    let pb = pb.apply_embedded_orientation().unwrap_or(pb);
    Some((pb.read_pixel_bytes(), pb.width(), pb.height(), pb.rowstride(), pb.has_alpha(), (w, h)))
}

/// Reads the start of a file and returns it if it looks like text.
fn read_text(path: &Path) -> Result<String, String> {
    // Only regular files: a pipe would block the reader thread, and a device may never end
    if !std::fs::metadata(path).map_err(|e| e.to_string())?.is_file() {
        return Err("not a regular file".into());
    }
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(16 * 1024);
    f.by_ref()
        .take(TEXT_LIMIT as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    let truncated = buf.len() > TEXT_LIMIT;
    buf.truncate(TEXT_LIMIT);
    if buf.is_empty() {
        return Ok("(empty file)".into());
    }
    if buf.iter().take(8192).any(|&b| b == 0) {
        return Err("binary".into());
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    // Too many replacement chars means it isn't really text
    let bad = s.chars().take(4096).filter(|&c| c == '\u{FFFD}').count();
    if bad > 32 {
        return Err("binary".into());
    }
    if truncated {
        s.push_str("\n\n… (preview truncated)");
    }
    Ok(s)
}

/// Adds a scroll delta to the running total and returns (whole pages to turn, what is left over).
/// Turning back the other way drops what was gathered in the old direction.
fn scroll_pages(acc: f64, dy: f64, step: f64) -> (i32, f64) {
    let acc = if acc * dy < 0.0 { dy } else { acc + dy };
    let pages = (acc / step).trunc();
    (pages as i32, acc - pages * step)
}

/// A page and the area (logical size and scale factor) it is rendered for.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
struct PdfKey {
    page: i32,
    w: i32,
    h: i32,
    scale: i32,
}

struct PdfReq {
    key: PdfKey,
    reply: mpsc::Sender<Option<PdfPage>>,
}

/// A rendered page as cairo RGB24 bytes; a cairo surface can't cross threads.
struct PdfPage {
    data: Vec<u8>,
    w: i32,
    h: i32,
    stride: i32,
}

/// Keeps at most one render in flight and, meanwhile, only the newest request.
#[derive(Default)]
struct Latest<T> {
    flying: Option<T>,
    pending: Option<T>,
}

impl<T: Clone + PartialEq> Latest<T> {
    /// Asks for `t` (with `have` already shown) and returns what to send now, if anything.
    fn want(&mut self, t: T, have: Option<T>) -> Option<T> {
        if have.as_ref() == Some(&t) || self.flying.as_ref() == Some(&t) {
            self.pending = None;
            return None;
        }
        if self.flying.is_some() {
            self.pending = Some(t);
            return None;
        }
        self.flying = Some(t.clone());
        Some(t)
    }

    /// The flying request is answered; returns the next one to send, if any.
    fn done(&mut self) -> Option<T> {
        self.flying = self.pending.take();
        self.flying.clone()
    }

    fn busy(&self) -> bool {
        self.flying.is_some()
    }
}

/// State of one PDF preview, owned by its widgets' closures. Dropping it drops the sender and ends the worker.
struct PdfView {
    tx: mpsc::Sender<PdfReq>,
    page: Cell<i32>,
    shown: RefCell<Option<(PdfKey, cairo::ImageSurface)>>,
    flight: RefCell<Latest<PdfKey>>,
    settle: RefCell<Option<glib::SourceId>>,
    area: glib::WeakRef<gtk::DrawingArea>,
    spinner: glib::WeakRef<gtk::Spinner>,
}

impl PdfView {
    /// What should be on screen now; None until the area has a size.
    fn wanted(&self) -> Option<PdfKey> {
        let area = self.area.upgrade()?;
        let (w, h) = (area.width(), area.height());
        (w > 0 && h > 0).then(|| PdfKey { page: self.page.get(), w, h, scale: area.scale_factor() })
    }

    /// After a resize: render right away the first time, otherwise once the size stops changing.
    fn schedule(self: &Rc<Self>) {
        if let Some(id) = self.settle.take() {
            id.remove();
        }
        if self.shown.borrow().is_none() {
            self.update();
            return;
        }
        let v = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(PDF_SETTLE, move || {
            if let Some(v) = v.upgrade() {
                v.settle.take();
                v.update();
            }
        });
        *self.settle.borrow_mut() = Some(id);
    }

    fn update(self: &Rc<Self>) {
        let Some(key) = self.wanted() else { return };
        let have = self.shown.borrow().as_ref().map(|(k, _)| *k);
        let send = self.flight.borrow_mut().want(key, have);
        if let Some(k) = send {
            self.send(k);
        }
        self.sync_spinner();
    }

    fn send(self: &Rc<Self>, key: PdfKey) {
        let (reply, rx) = mpsc::channel();
        // Only fails if the worker is gone, and then the reply below comes back empty
        let _ = self.tx.send(PdfReq { key, reply });
        let v = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || rx.recv().ok().flatten()).await.ok().flatten();
            if let Some(v) = v.upgrade() {
                v.got(key, res);
            }
        });
    }

    fn got(self: &Rc<Self>, key: PdfKey, res: Option<PdfPage>) {
        // A late answer for a page turned away from is dropped
        if key.page == self.page.get() {
            let surf = res.and_then(|pg| {
                let s = cairo::ImageSurface::create_for_data(pg.data, cairo::Format::Rgb24, pg.w, pg.h, pg.stride);
                let s = s.ok()?;
                s.set_device_scale(key.scale.max(1) as f64, key.scale.max(1) as f64);
                Some(s)
            });
            // A page that fails to render is left blank rather than showing the one before
            *self.shown.borrow_mut() = surf.map(|s| (key, s));
            if let Some(a) = self.area.upgrade() {
                a.queue_draw();
            }
        }
        let next = self.flight.borrow_mut().done();
        if let Some(k) = next {
            self.send(k);
        }
        self.sync_spinner();
    }

    fn sync_spinner(&self) {
        let Some(sp) = self.spinner.upgrade() else { return };
        // While resizing, the old render is still the right page, so only page turns and the first render spin
        let have = self.shown.borrow().as_ref().map(|(k, _)| k.page);
        let busy = self.flight.borrow().busy() && have != Some(self.page.get());
        sp.set_visible(busy);
        sp.set_spinning(busy);
    }
}

/// Owns the document for one preview: opens it, reports the page count, then renders pages until the preview
/// drops its sender.
fn pdf_worker(uri: &str, opened: mpsc::Sender<Result<i32, String>>, rx: mpsc::Receiver<PdfReq>) {
    let doc = match poppler::Document::from_file(uri, None) {
        Ok(d) => d,
        Err(e) => {
            let _ = opened.send(Err(e.message().to_string()));
            return;
        }
    };
    if opened.send(Ok(doc.n_pages().max(1))).is_err() {
        return;
    }
    while let Ok(req) = rx.recv() {
        let _ = req.reply.send(render_pdf_page(&doc, req.key));
    }
}

fn render_pdf_page(doc: &poppler::Document, key: PdfKey) -> Option<PdfPage> {
    // Poppler logs a critical for an out-of-range index
    if !(0..doc.n_pages()).contains(&key.page) {
        return None;
    }
    let page = doc.page(key.page)?;
    let (pw, ph) = page.size();
    let (w, h, s) = pdf_fit(pw, ph, key.w, key.h, key.scale)?;
    let mut surf = cairo::ImageSurface::create(cairo::Format::Rgb24, w, h).ok()?;
    {
        let cr = cairo::Context::new(&surf).ok()?;
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.paint().ok()?;
        cr.scale(s, s);
        page.render(&cr);
    }
    surf.flush();
    let stride = surf.stride();
    let data = surf.data().ok()?.to_vec();
    Some(PdfPage { data, w, h, stride })
}

/// Device-pixel size of a `pw` x `ph` point page fitted into a `w` x `h` area at `scale`, plus the points to
/// pixels factor. Capped at PDF_MAX_PX a side.
fn pdf_fit(pw: f64, ph: f64, w: i32, h: i32, scale: i32) -> Option<(i32, i32, f64)> {
    if !(pw > 0.0 && ph > 0.0) || w <= 0 || h <= 0 {
        return None;
    }
    let (aw, ah) = ((w * scale.max(1)) as f64, (h * scale.max(1)) as f64);
    let s = (aw / pw).min(ah / ph).min(PDF_MAX_PX / pw.max(ph));
    let px = |v: f64, max: f64| ((v * s).round() as i32).clamp(1, max.min(PDF_MAX_PX) as i32);
    Some((px(pw, aw), px(ph, ah), s))
}

fn text_view(text: &str, mono: bool) -> gtk::ScrolledWindow {
    let tv = gtk::TextView::new();
    tv.set_editable(false);
    tv.set_cursor_visible(false);
    tv.set_monospace(mono);
    tv.set_wrap_mode(gtk::WrapMode::WordChar);
    tv.set_left_margin(12);
    tv.set_right_margin(12);
    tv.set_top_margin(10);
    tv.set_bottom_margin(10);
    tv.add_css_class("preview-text");
    tv.buffer().set_text(text);
    gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&tv)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_files_are_not_read_as_text() {
        let base = std::env::temp_dir().join(format!("rfiles-preview-special-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let pipe = base.join("pipe");
        assert!(std::process::Command::new("mkfifo").arg(&pipe).status().unwrap().success());
        let link = base.join("link");
        std::os::unix::fs::symlink(&pipe, &link).unwrap();

        // On a thread, so a blocked read fails the test instead of hanging it
        let (tx, rx) = std::sync::mpsc::channel();
        let (p, l) = (pipe.clone(), link.clone());
        std::thread::spawn(move || {
            let _ = tx.send((read_text(&p).is_err(), read_text(&l).is_err()));
        });
        let res = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("read_text blocked");
        assert_eq!(res, (true, true));

        // `show` relies on GIO calling a pipe (also through a link) a special file
        for p in [&pipe, &link] {
            let t = gio::File::for_path(p).query_file_type(gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>);
            assert_eq!(t, gio::FileType::Special);
        }

        std::fs::write(base.join("a.txt"), b"hello").unwrap();
        assert_eq!(read_text(&base.join("a.txt")).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn wheel_notch_turns_one_page() {
        assert_eq!(scroll_pages(0.0, 1.0, 1.0), (1, 0.0));
        assert_eq!(scroll_pages(0.0, -1.0, 1.0), (-1, 0.0));
        assert_eq!(scroll_pages(0.0, 0.0, 1.0), (0, 0.0));
    }

    #[test]
    fn small_deltas_add_up_to_one_page() {
        let mut acc = 0.0;
        let mut turned = 0;
        for _ in 0..25 {
            let (p, rest) = scroll_pages(acc, 4.0, PDF_SCROLL_PX);
            turned += p;
            acc = rest;
        }
        assert_eq!(turned, 1);
        assert_eq!(acc, 0.0);
        let (p, rest) = scroll_pages(0.0, 250.0, PDF_SCROLL_PX);
        assert_eq!((p, rest), (2, 50.0));
    }

    #[test]
    fn direction_change_drops_leftover() {
        let (p, rest) = scroll_pages(90.0, -5.0, PDF_SCROLL_PX);
        assert_eq!((p, rest), (0, -5.0));
        let (p, rest) = scroll_pages(-0.5, 1.0, 1.0);
        assert_eq!((p, rest), (1, 0.0));
    }

    #[test]
    fn pdf_page_fits_area_in_device_pixels() {
        // A4 portrait into a wide area: height decides
        let (w, h, s) = pdf_fit(595.0, 842.0, 800, 421, 1).unwrap();
        assert_eq!(h, 421);
        assert!(w < 800 && (w - 298).abs() <= 1);
        assert!((s - 0.5).abs() < 0.001);
        // Scale factor 2 renders twice the pixels
        assert_eq!(pdf_fit(200.0, 100.0, 100, 100, 2).unwrap(), (200, 100, 1.0));
        // Never more than PDF_MAX_PX a side
        let (w, h, _) = pdf_fit(100.0, 100.0, 5000, 5000, 2).unwrap();
        assert_eq!((w, h), (4096, 4096));
        assert_eq!(pdf_fit(0.0, 100.0, 10, 10, 1), None);
        assert_eq!(pdf_fit(f64::NAN, 100.0, 10, 10, 1), None);
        assert_eq!(pdf_fit(100.0, 100.0, 0, 10, 1), None);
        // Absurd page sizes still give at least one pixel
        let (w, h, _) = pdf_fit(1e9, 1.0, 10, 10, 1).unwrap();
        assert_eq!((w, h), (10, 1));
    }

    #[test]
    fn only_latest_pdf_request_is_sent() {
        let mut l = Latest::default();
        assert_eq!(l.want(1, None), Some(1));
        assert!(l.busy());
        // Quick turns while 1 renders: only the last one goes next
        assert_eq!(l.want(2, None), None);
        assert_eq!(l.want(3, None), None);
        assert_eq!(l.done(), Some(3));
        assert_eq!(l.done(), None);
        assert!(!l.busy());
    }

    #[test]
    fn pdf_request_for_what_is_shown_or_flying_is_skipped() {
        let mut l = Latest::default();
        assert_eq!(l.want(1, Some(1)), None);
        assert!(!l.busy());
        assert_eq!(l.want(2, Some(1)), Some(2));
        assert_eq!(l.want(2, Some(1)), None);
        // Back to the shown page while 2 renders: nothing more to do after it
        assert_eq!(l.want(3, Some(1)), None);
        assert_eq!(l.want(1, Some(1)), None);
        assert_eq!(l.done(), None);
    }

    /// A two-page PDF with valid cross references, 200 x 100 points a page.
    fn tiny_pdf() -> Vec<u8> {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>",
        ];
        let mut out = String::from("%PDF-1.4\n");
        let mut offs = vec![];
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
        }
        let xref = out.len();
        out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
        for o in offs {
            out.push_str(&format!("{o:010} 00000 n \n"));
        }
        out.push_str(&format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1));
        out.into_bytes()
    }

    /// Runs the worker on `path` on its own thread; the returned receiver fires when the thread ends.
    fn start_worker(path: &Path) -> (mpsc::Receiver<Result<i32, String>>, mpsc::Sender<PdfReq>, mpsc::Receiver<()>) {
        let uri = gio::File::for_path(path).uri().to_string();
        let (otx, orx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        std::thread::spawn(move || {
            pdf_worker(&uri, otx, rx);
            let _ = etx.send(());
        });
        (orx, tx, erx)
    }

    #[test]
    fn pdf_worker_renders_pages_and_stops_with_the_preview() {
        let base = std::env::temp_dir().join(format!("rfiles-preview-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let t = std::time::Duration::from_secs(10);

        let good = base.join("a.pdf");
        std::fs::write(&good, tiny_pdf()).unwrap();
        let (opened, tx, ended) = start_worker(&good);
        assert_eq!(opened.recv_timeout(t).unwrap(), Ok(2));
        let ask = |page| {
            let (reply, rx) = mpsc::channel();
            tx.send(PdfReq { key: PdfKey { page, w: 100, h: 100, scale: 2 }, reply }).unwrap();
            rx.recv_timeout(t).unwrap()
        };
        let pg = ask(1).unwrap();
        assert_eq!((pg.w, pg.h), (200, 100));
        assert!(pg.data.len() >= (pg.stride * pg.h) as usize);
        // Blank page on white: RGB24 is a native-endian 0x00RRGGBB word
        let px = u32::from_ne_bytes(pg.data[..4].try_into().unwrap());
        assert_eq!(px & 0xFF_FFFF, 0xFF_FFFF);
        assert!(ask(5).is_none());
        // Dropping the sender, as clearing the preview does, ends the thread
        drop(tx);
        ended.recv_timeout(t).expect("worker kept running");

        let bad = base.join("broken.pdf");
        std::fs::write(&bad, b"%PDF-1.4 not really").unwrap();
        let (opened, _tx, ended) = start_worker(&bad);
        assert!(opened.recv_timeout(t).unwrap().is_err());
        ended.recv_timeout(t).expect("worker kept running");
        let _ = std::fs::remove_dir_all(&base);
    }
}
