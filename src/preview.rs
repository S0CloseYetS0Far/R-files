//! Right-hand preview pane. Each preview is built only when a file is
//! selected and torn down when the selection changes, so nothing is held in
//! memory for files you are not looking at.

use crate::docs;
use crate::util::*;
use gtk::prelude::*;
use gtk::{gdk, gdk_pixbuf, gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const TEXT_LIMIT: usize = 256 * 1024;
const IMAGE_MAX: i32 = 2048;
/// Touchpad scroll distance, in pixels, that turns one PDF page.
const PDF_SCROLL_PX: f64 = 100.0;

pub struct Preview {
    pub root: gtk::Box,
    body: gtk::Box,
    title: gtk::Label,
    meta: gtk::Label,
    open_btn: gtk::Button,
    gen: Cell<u64>,
    media: RefCell<Option<gtk::Video>>,
    current: RefCell<Option<gio::FileInfo>>,
    pub on_open: RefCell<Option<Rc<dyn Fn(&gio::FileInfo)>>>,
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

        if ct.starts_with("image/") {
            self.image_preview(path, g, info.clone());
        } else if ct.starts_with("video/") {
            self.media_preview(&file, true, info);
        } else if ct.starts_with("audio/") {
            self.media_preview(&file, false, info);
        } else if ct == "application/pdf" || ext == "pdf" {
            self.pdf_preview(&file, info);
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
    fn pdf_preview(&self, file: &gio::File, info: &gio::FileInfo) {
        let doc = match poppler::Document::from_file(&file.uri(), None) {
            Ok(d) => d,
            Err(e) => {
                self.icon_placeholder(info, &format!("Can't open PDF: {}", e.message()));
                return;
            }
        };
        let n = doc.n_pages().max(1);
        let page = Rc::new(Cell::new(0));

        let area = gtk::DrawingArea::new();
        area.set_vexpand(true);
        area.set_hexpand(true);
        let (d, pg) = (doc.clone(), page.clone());
        area.set_draw_func(move |_, cr, w, h| {
            let Some(p) = d.page(pg.get()) else { return };
            let (pw, ph) = p.size();
            if pw <= 0.0 || ph <= 0.0 {
                return;
            }
            let s = (w as f64 / pw).min(h as f64 / ph);
            cr.translate((w as f64 - pw * s) / 2.0, (h as f64 - ph * s) / 2.0);
            cr.scale(s, s);
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.rectangle(0.0, 0.0, pw, ph);
            let _ = cr.fill();
            p.render(cr);
        });

        let prev = gtk::Button::from_icon_name("go-previous-symbolic");
        let next = gtk::Button::from_icon_name("go-next-symbolic");
        let label = gtk::Label::new(Some(&format!("1 / {n}")));
        let nav = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        nav.set_halign(gtk::Align::Center);
        nav.append(&prev);
        nav.append(&label);
        nav.append(&next);

        let go: Rc<dyn Fn(i32)> = {
            let (page, label, area) = (page.clone(), label.clone(), area.downgrade());
            Rc::new(move |delta: i32| {
                let Some(area) = area.upgrade() else { return };
                let np = (page.get() + delta).clamp(0, n - 1);
                if np != page.get() {
                    page.set(np);
                    label.set_text(&format!("{} / {n}", np + 1));
                    area.queue_draw();
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

        self.body.append(&area);
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
}
