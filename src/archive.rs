//! Compress and extract archives: .zip, .tar, .tar.gz/.tgz and .tar.xz/.txz.
//!
//! Everything is streamed (no file is ever read into memory as a whole)
//! and runs on a worker thread, with progress in the status bar.
//!
//! Safety rules when extracting:
//! - Entry paths with `..`, an absolute path or anything else that could
//!   leave the target folder are rejected ("zip slip").
//! - Nothing is ever written through a symlink: every parent folder of an
//!   entry must be a real folder we created inside the target.
//! - Nothing is overwritten. The archive is unpacked into a hidden scratch
//!   folder next to it, then moved to a free name; on failure the scratch
//!   folder is removed, so no partial output is left behind.

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Zip,
    Tar,
    TarGz,
    TarXz,
}

/// Recognised extensions, checked case-insensitively, longest match first.
const EXTS: &[(&str, Format)] = &[
    (".tar.gz", Format::TarGz),
    (".tar.xz", Format::TarXz),
    (".tgz", Format::TarGz),
    (".txz", Format::TarXz),
    (".tar", Format::Tar),
    (".zip", Format::Zip),
];

/// Formats offered in the Compress dialog (the first one is the default).
const CHOICES: &[(&str, Format)] = &[(".zip", Format::Zip), (".tar.gz", Format::TarGz), (".tar.xz", Format::TarXz)];

/// Files that are already compressed: stored as-is in a zip instead of
/// wasting time deflating them again.
const COMPRESSED_EXTS: &[&str] = &[
    "7z", "aac", "apk", "avi", "avif", "bz2", "docx", "epub", "flac", "gif", "gz", "heic", "jar", "jpeg", "jpg",
    "m4a", "mkv", "mov", "mp3", "mp4", "odp", "ods", "odt", "ogg", "opus", "png", "pptx", "rar", "tgz", "txz",
    "webm", "webp", "xlsx", "xz", "zip", "zst",
];

/// Splits "photos.tar.gz" into ("photos", ".tar.gz", TarGz).
fn split_archive_name(name: &str) -> Option<(&str, &str, Format)> {
    let lower = name.to_ascii_lowercase();
    EXTS.iter()
        .find(|(e, _)| lower.ends_with(e))
        .map(|(e, f)| {
            let cut = name.len() - e.len();
            (&name[..cut], &name[cut..], *f)
        })
}

/// Whether "Extract Here" can handle a file with this name.
pub fn is_archive(name: &str) -> bool {
    split_archive_name(name).is_some()
}

// ================= progress & small helpers =================

#[derive(Default)]
pub(crate) struct Progress {
    done: AtomicU64,
    total: AtomicU64,
}

impl Progress {
    fn reset(&self, total: u64) {
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    fn text(&self) -> String {
        let done = self.done.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        match (done.min(total).saturating_mul(100)).checked_div(total) {
            Some(pct) => format!("{pct}%"),
            None => glib::format_size(done).to_string(),
        }
    }
}

/// A reader that adds every byte it reads to a progress counter.
struct Counting<'a, R> {
    inner: R,
    read: u64,
    counter: &'a AtomicU64,
}

impl<'a, R> Counting<'a, R> {
    fn new(inner: R, counter: &'a AtomicU64) -> Self {
        Counting { inner, read: 0, counter }
    }
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        self.counter.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// Prefixes an error with the name of the item it happened on.
fn at(name: &Path) -> impl FnOnce(io::Error) -> io::Error + '_ {
    move |e| io::Error::new(e.kind(), format!("{}: {e}", name.display()))
}

fn zip_err(e: zip::result::ZipError) -> io::Error {
    match e {
        zip::result::ZipError::Io(e) => e,
        e => err(e.to_string()),
    }
}

fn display_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// "name.ext", "name (2).ext", "name (3).ext", …
fn candidates<'a>(stem: &'a OsStr, ext: &'a str) -> impl Iterator<Item = OsString> + 'a {
    (1u32..10_000).map(move |i| {
        let mut n = stem.to_os_string();
        if i > 1 {
            n.push(format!(" ({i})"));
        }
        n.push(ext);
        n
    })
}

/// Creates a new, empty file with a free name in `dir`. Atomic: an
/// existing file is never touched, even if it appears meanwhile.
fn create_unique_file(dir: &Path, stem: &OsStr, ext: &str) -> io::Result<(PathBuf, File)> {
    for n in candidates(stem, ext) {
        let p = dir.join(n);
        match fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(f) => return Ok((p, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(err("no free file name"))
}

/// Creates a new, empty folder with a free name in `dir`.
fn create_unique_dir(dir: &Path, stem: &OsStr) -> io::Result<PathBuf> {
    for n in candidates(stem, "") {
        let p = dir.join(n);
        match fs::DirBuilder::new().mode(0o700).create(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(err("no free folder name"))
}

/// Moves folder `src` to a free name in `dir`. The name is reserved first
/// by creating an empty folder, which the rename then replaces, so an
/// existing folder is never overwritten.
fn move_to_unique(src: &Path, dir: &Path, stem: &OsStr) -> io::Result<PathBuf> {
    let target = create_unique_dir(dir, stem)?;
    match fs::rename(src, &target) {
        Ok(()) => Ok(target),
        Err(e) => {
            let _ = fs::remove_dir(&target);
            Err(e)
        }
    }
}

fn umask() -> u32 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Umask:"))
                .and_then(|v| u32::from_str_radix(v.trim(), 8).ok())
        })
        .unwrap_or(0o022)
}

/// Turns an archive entry path into a safe relative path. Errors on
/// absolute paths and `..`; "./" alone gives an empty path (skip it).
fn safe_rel(p: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsafe path in archive: “{}”", p.display()),
                ))
            }
        }
    }
    Ok(out)
}

fn zip_time(secs: i64) -> zip::DateTime {
    glib::DateTime::from_unix_local(secs)
        .ok()
        .and_then(|d| {
            zip::DateTime::from_date_and_time(
                d.year() as u16,
                d.month() as u8,
                d.day_of_month() as u8,
                d.hour() as u8,
                d.minute() as u8,
                d.second() as u8,
            )
            .ok()
        })
        .unwrap_or_default()
}

fn zip_time_to_system(t: zip::DateTime) -> Option<SystemTime> {
    let d = glib::DateTime::from_local(
        t.year() as i32,
        t.month() as i32,
        t.day() as i32,
        t.hour() as i32,
        t.minute() as i32,
        t.second() as f64,
    )
    .ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(d.to_unix()).ok()?))
}

// ================= compress =================

/// Where the walk puts what it finds.
trait Sink {
    fn dir(&mut self, name: &Path, md: &Metadata) -> io::Result<()>;
    fn file(&mut self, name: &Path, md: &Metadata, data: &mut dyn Read) -> io::Result<()>;
    fn symlink(&mut self, name: &Path, md: &Metadata, target: &Path) -> io::Result<()>;
}

fn tar_header(md: &Metadata) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_metadata_in_mode(md, tar::HeaderMode::Complete);
    h
}

impl<W: Write> Sink for tar::Builder<W> {
    fn dir(&mut self, name: &Path, md: &Metadata) -> io::Result<()> {
        let mut h = tar_header(md);
        h.set_size(0);
        self.append_data(&mut h, name, io::empty())
    }
    fn file(&mut self, name: &Path, md: &Metadata, data: &mut dyn Read) -> io::Result<()> {
        let mut h = tar_header(md);
        self.append_data(&mut h, name, data)
    }
    fn symlink(&mut self, name: &Path, md: &Metadata, target: &Path) -> io::Result<()> {
        let mut h = tar_header(md);
        h.set_size(0);
        self.append_link(&mut h, name, target)
    }
}

struct ZipSink<W: Write + io::Seek>(zip::ZipWriter<W>);

fn zip_name(name: &Path) -> io::Result<&str> {
    name.to_str()
        .ok_or_else(|| err(format!("“{}” has a name zip can't store; use .tar.gz", name.display())))
}

fn zip_options(md: &Metadata) -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .unix_permissions(md.mode() & 0o7777)
        .last_modified_time(zip_time(md.mtime()))
}

impl<W: Write + io::Seek> Sink for ZipSink<W> {
    fn dir(&mut self, name: &Path, md: &Metadata) -> io::Result<()> {
        self.0.add_directory(format!("{}/", zip_name(name)?), zip_options(md)).map_err(zip_err)
    }
    fn file(&mut self, name: &Path, md: &Metadata, data: &mut dyn Read) -> io::Result<()> {
        let mut opts = zip_options(md).large_file(md.len() >= u32::MAX as u64);
        let stored = name
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| COMPRESSED_EXTS.contains(&e.to_ascii_lowercase().as_str()));
        if stored {
            opts = opts.compression_method(zip::CompressionMethod::Stored);
        }
        self.0.start_file(zip_name(name)?, opts).map_err(zip_err)?;
        io::copy(data, &mut self.0)?;
        Ok(())
    }
    fn symlink(&mut self, name: &Path, md: &Metadata, target: &Path) -> io::Result<()> {
        self.0.add_symlink(zip_name(name)?, zip_name(target)?, zip_options(md)).map_err(zip_err)
    }
}

/// Total size of the regular files under `p` (symlinks are not followed).
fn tree_size(p: &Path) -> u64 {
    let Ok(md) = fs::symlink_metadata(p) else { return 0 };
    if md.is_dir() {
        fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| tree_size(&e.path())).sum())
            .unwrap_or(0)
    } else if md.is_file() {
        md.len()
    } else {
        0
    }
}

/// Adds `src` to the archive as `name`, recursing into folders.
/// `skip` is the (device, inode) of the archive being written.
fn walk(src: &Path, name: &mut PathBuf, sink: &mut dyn Sink, skip: (u64, u64), p: &Progress) -> io::Result<()> {
    let md = fs::symlink_metadata(src).map_err(at(name))?;
    if (md.dev(), md.ino()) == skip {
        return Ok(()); // never put the archive inside itself
    }
    let ft = md.file_type();
    if ft.is_symlink() {
        let target = fs::read_link(src).map_err(at(name))?;
        sink.symlink(name, &md, &target).map_err(at(name))
    } else if ft.is_dir() {
        sink.dir(name, &md).map_err(at(name))?;
        let mut kids = fs::read_dir(src)
            .and_then(|rd| rd.map(|e| e.map(|e| e.file_name())).collect::<io::Result<Vec<_>>>())
            .map_err(at(name))?;
        kids.sort();
        for k in kids {
            name.push(&k);
            let r = walk(&src.join(&k), name, sink, skip, p);
            name.pop();
            r?;
        }
        Ok(())
    } else if ft.is_file() {
        let f = File::open(src).map_err(at(name))?;
        let mut r = Counting::new(f.take(md.len()), &p.done);
        sink.file(name, &md, &mut r).map_err(at(name))?;
        if r.read != md.len() {
            return Err(err(format!("{}: file changed while compressing", name.display())));
        }
        Ok(())
    } else {
        Ok(()) // sockets, pipes, devices: skipped (reading a pipe could block forever)
    }
}

fn walk_all(sources: &[PathBuf], sink: &mut dyn Sink, skip: (u64, u64), p: &Progress) -> io::Result<()> {
    for src in sources {
        let mut name = PathBuf::from(src.file_name().ok_or_else(|| err("can't compress “/”"))?);
        walk(src, &mut name, sink, skip, p)?;
    }
    Ok(())
}

fn tar_into<W: Write>(w: W, sources: &[PathBuf], skip: (u64, u64), p: &Progress) -> io::Result<W> {
    let mut b = tar::Builder::new(w);
    walk_all(sources, &mut b, skip, p)?;
    b.into_inner()
}

fn write_archive(file: File, fmt: Format, sources: &[PathBuf], p: &Progress) -> io::Result<()> {
    let md = file.metadata()?;
    let skip = (md.dev(), md.ino());
    let w = BufWriter::with_capacity(128 * 1024, file);
    let w = match fmt {
        Format::Zip => {
            let mut z = ZipSink(zip::ZipWriter::new(w));
            walk_all(sources, &mut z, skip, p)?;
            z.0.finish().map_err(zip_err)?
        }
        Format::Tar => tar_into(w, sources, skip, p)?,
        Format::TarGz => tar_into(flate2::write::GzEncoder::new(w, flate2::Compression::default()), sources, skip, p)?
            .finish()?,
        Format::TarXz => {
            let xz = lzma_rust2::XzWriter::new(w, lzma_rust2::XzOptions::with_preset(6))?;
            tar_into(xz, sources, skip, p)?.finish()?
        }
    };
    w.into_inner().map_err(|e| e.into_error())?;
    Ok(())
}

/// Creates `<stem><ext>` (or a free variant of it) in `dir` holding
/// `sources`. Returns the archive's path. Runs on a worker thread.
pub(crate) fn compress(sources: &[PathBuf], dir: &Path, stem: &str, ext: &str, fmt: Format, p: &Progress) -> io::Result<PathBuf> {
    p.reset(sources.iter().map(|s| tree_size(s)).sum());
    let (out, file) = create_unique_file(dir, OsStr::new(stem), ext)?;
    match write_archive(file, fmt, sources, p) {
        Ok(()) => Ok(out),
        Err(e) => {
            let _ = fs::remove_file(&out);
            Err(e)
        }
    }
}

// ================= extract =================

/// Writes entries below `root`, refusing anything that could escape it.
struct Extractor {
    root: PathBuf,
    /// The parent folder (relative) last checked to be a real folder.
    checked: PathBuf,
    /// Folders with their mode and mtime, applied at the very end (a
    /// read-only folder would otherwise block writing its contents).
    dirs: Vec<(PathBuf, u32, Option<SystemTime>)>,
    buf: Vec<u8>,
    skipped: usize,
}

impl Extractor {
    fn new(root: PathBuf) -> Self {
        Extractor { root, checked: PathBuf::new(), dirs: Vec::new(), buf: vec![0; 64 * 1024], skipped: 0 }
    }

    /// Makes sure every parent of `rel` is a real folder inside the root
    /// (creating missing ones; a symlink or file there is an error), and
    /// returns the full path for `rel`.
    fn prepare(&mut self, rel: &Path) -> io::Result<PathBuf> {
        let parent = rel.parent().unwrap_or(Path::new(""));
        if parent != self.checked {
            let mut cur = self.root.clone();
            for c in parent.components() {
                cur.push(c);
                match fs::symlink_metadata(&cur) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => return Err(err(format!("{}: a parent isn't a folder", rel.display()))),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        fs::DirBuilder::new().mode(0o755).create(&cur).map_err(at(rel))?
                    }
                    Err(e) => return Err(at(rel)(e)),
                }
            }
            // Nothing is ever replaced, so a checked folder stays a folder.
            self.checked = parent.to_path_buf();
        }
        Ok(self.root.join(rel))
    }

    fn dir(&mut self, rel: &Path, mode: u32, mtime: Option<SystemTime>) -> io::Result<()> {
        let path = self.prepare(rel)?;
        match fs::DirBuilder::new().mode(0o755).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && fs::symlink_metadata(&path)?.is_dir() => {}
            Err(e) => return Err(at(rel)(e)),
        }
        self.dirs.push((rel.to_path_buf(), mode, mtime));
        Ok(())
    }

    fn file(&mut self, rel: &Path, mode: u32, mtime: Option<SystemTime>, data: &mut dyn Read) -> io::Result<()> {
        let path = self.prepare(rel)?;
        // create_new: fails on anything existing, and never follows a symlink
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode & 0o777)
            .open(&path)
            .map_err(at(rel))?;
        loop {
            let n = match data.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(at(rel)(e)),
            };
            f.write_all(&self.buf[..n]).map_err(at(rel))?;
        }
        if let Some(t) = mtime {
            let _ = f.set_modified(t);
        }
        Ok(())
    }

    fn symlink(&mut self, rel: &Path, target: &Path) -> io::Result<()> {
        let path = self.prepare(rel)?;
        // Created as-is but never followed: writes only go through
        // folders checked by `prepare`.
        std::os::unix::fs::symlink(target, &path).map_err(at(rel))
    }

    fn hardlink(&mut self, rel: &Path, target: &Path) -> io::Result<()> {
        let bad = || err(format!("{}: bad hard link to “{}”", rel.display(), target.display()));
        if target.as_os_str().is_empty() {
            return Err(bad());
        }
        // The link source must be an earlier entry, reached via real folders
        let mut cur = self.root.clone();
        for c in target.parent().unwrap_or(Path::new("")).components() {
            cur.push(c);
            if !fs::symlink_metadata(&cur).is_ok_and(|m| m.is_dir()) {
                return Err(bad());
            }
        }
        cur.push(target.file_name().ok_or_else(bad)?);
        if !fs::symlink_metadata(&cur).is_ok_and(|m| !m.is_dir()) {
            return Err(bad());
        }
        let path = self.prepare(rel)?;
        fs::hard_link(&cur, &path).map_err(at(rel))
    }

    /// Applies folder modes and times once the content is in `base`.
    /// `top` is set when the archive's single top folder became `base`.
    fn finish(self, base: &Path, top: Option<&OsStr>) {
        let mask = 0o777 & !umask();
        for (rel, mode, mtime) in self.dirs.iter().rev() {
            let path = match top {
                Some(t) => match rel.strip_prefix(t) {
                    Ok(r) => base.join(r),
                    Err(_) => continue,
                },
                None => base.join(rel),
            };
            if let (Some(t), Ok(f)) = (mtime, File::open(&path)) {
                let _ = f.set_modified(*t);
            }
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode & mask));
        }
    }
}

fn extract_tar<R: Read>(r: R, ex: &mut Extractor) -> io::Result<()> {
    let mut ar = tar::Archive::new(r);
    for entry in ar.entries()? {
        let mut e = entry?;
        let rel = safe_rel(&e.path()?)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let h = e.header();
        let mode = h.mode().unwrap_or(0o644);
        let mtime = h.mtime().ok().map(|s| UNIX_EPOCH + Duration::from_secs(s));
        match h.entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                ex.file(&rel, mode, mtime, &mut e)?
            }
            tar::EntryType::Directory => ex.dir(&rel, mode, mtime)?,
            tar::EntryType::Symlink => {
                let target = e.link_name()?.ok_or_else(|| err(format!("{}: symlink without target", rel.display())))?;
                ex.symlink(&rel, &target)?
            }
            tar::EntryType::Link => {
                let target = e.link_name()?.ok_or_else(|| err(format!("{}: hard link without target", rel.display())))?;
                let target = safe_rel(&target)?;
                ex.hardlink(&rel, &target)?
            }
            tar::EntryType::XGlobalHeader => {}
            _ => ex.skipped += 1, // devices, fifos, …
        }
    }
    Ok(())
}

fn extract_zip(file: File, ex: &mut Extractor, p: &Progress) -> io::Result<()> {
    let mut z = zip::ZipArchive::new(BufReader::new(file)).map_err(zip_err)?;
    p.reset(z.decompressed_size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0));
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(zip_err)?;
        let rel = match f.enclosed_name() {
            Some(n) => safe_rel(&n)?,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsafe path in archive: “{}”", f.name().unwrap_or_default()),
                ))
            }
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let mtime = f.last_modified().and_then(zip_time_to_system);
        // Zips made on Windows carry no Unix mode
        let mode = f.unix_mode().filter(|m| m & 0o777 != 0);
        if f.is_symlink() {
            let mut t = Vec::new();
            f.by_ref().take(4097).read_to_end(&mut t)?;
            if t.is_empty() || t.len() > 4096 {
                return Err(err(format!("{}: bad symlink", rel.display())));
            }
            ex.symlink(&rel, Path::new(OsStr::from_bytes(&t)))?;
        } else if f.is_dir() {
            ex.dir(&rel, mode.unwrap_or(0o755), mtime)?;
        } else {
            ex.file(&rel, mode.unwrap_or(0o644), mtime, &mut Counting::new(&mut f, &p.done))?;
        }
    }
    Ok(())
}

/// Moves the unpacked content of `tmp` into `dest`: a single top folder
/// is moved as-is, anything else goes into a folder named `stem`.
fn place(tmp: &Path, dest: &Path, stem: &OsStr) -> io::Result<(PathBuf, Option<OsString>)> {
    let mut it = fs::read_dir(tmp)?;
    let first = it.next().transpose()?;
    let only_one = it.next().is_none();
    match first {
        None => Err(err("the archive is empty")),
        Some(e) if only_one && e.file_type()?.is_dir() => {
            let top = e.file_name();
            let out = move_to_unique(&e.path(), dest, &top)?;
            let _ = fs::remove_dir(tmp);
            Ok((out, Some(top)))
        }
        Some(_) => Ok((move_to_unique(tmp, dest, stem)?, None)),
    }
}

/// Extracts `archive` into `dest`. Returns the created folder and the
/// number of skipped special entries. Runs on a worker thread.
pub(crate) fn extract(archive: &Path, dest: &Path, p: &Progress) -> io::Result<(PathBuf, usize)> {
    let name = archive.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let (stem, _, fmt) = split_archive_name(name).ok_or_else(|| err("not a supported archive"))?;
    let stem = if stem.is_empty() { "Extracted" } else { stem };
    let file = File::open(archive)?;
    let tmp = create_unique_dir(dest, OsStr::new(&format!(".{name}.extracting")))?;
    let mut ex = Extractor::new(tmp.clone());
    let res = (|| {
        if fmt == Format::Zip {
            extract_zip(file, &mut ex, p)?;
        } else {
            p.reset(file.metadata()?.len());
            let raw = BufReader::with_capacity(64 * 1024, Counting::new(file, &p.done));
            match fmt {
                Format::TarGz => extract_tar(flate2::bufread::MultiGzDecoder::new(raw), &mut ex)?,
                Format::TarXz => extract_tar(lzma_rust2::XzReader::new(raw, true), &mut ex)?,
                _ => extract_tar(raw, &mut ex)?,
            }
        }
        place(&tmp, dest, OsStr::new(stem))
    })();
    match res {
        Ok((out, top)) => {
            let skipped = ex.skipped;
            ex.finish(&out, top.as_deref());
            Ok((out, skipped))
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

// ================= UI =================

/// Runs `work` on a worker thread, showing `busy` plus progress in the
/// status bar, then the result message.
fn run_job(status: &gtk::Label, busy: String, work: impl FnOnce(&Progress) -> String + Send + 'static) {
    let progress = Arc::new(Progress::default());
    status.set_text(&busy);
    let (st, p) = (status.clone(), progress.clone());
    let timer = glib::timeout_add_local(Duration::from_millis(250), move || {
        st.set_text(&format!("{busy}  {}", p.text()));
        glib::ControlFlow::Continue
    });
    let status = status.clone();
    glib::spawn_future_local(async move {
        let res = gio::spawn_blocking(move || work(&progress)).await;
        timer.remove();
        status.set_text(&res.unwrap_or_else(|_| "Archive operation failed".into()));
    });
}

/// Asks for a name and format, then compresses `sources` into `dir`.
pub fn compress_dialog(parent: &impl IsA<gtk::Window>, status: &gtk::Label, dir: PathBuf, sources: Vec<PathBuf>) {
    if sources.is_empty() {
        return;
    }
    let initial = match sources.as_slice() {
        [one] if one.is_dir() => display_name(one),
        [one] => one
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| display_name(one)),
        _ => "Archive".to_string(),
    };
    let win = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Compress")
        .default_width(420)
        .resizable(false)
        .build();
    win.add_css_class("rfiles");
    let entry = gtk::Entry::builder().text(initial.as_str()).hexpand(true).build();
    let names: Vec<&str> = CHOICES.iter().map(|c| c.0).collect();
    let format = gtk::DropDown::from_strings(&names);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&entry);
    row.append(&format);
    let errl = gtk::Label::new(None);
    errl.add_css_class("error");
    errl.set_xalign(0.0);
    errl.set_visible(false);
    let cancel = gtk::Button::with_label("Cancel");
    let okb = gtk::Button::with_label("Create");
    okb.add_css_class("suggested-action");
    let btns = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    btns.set_halign(gtk::Align::End);
    btns.append(&cancel);
    btns.append(&okb);
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 12);
    for m in [gtk::Box::set_margin_top, gtk::Box::set_margin_bottom, gtk::Box::set_margin_start, gtk::Box::set_margin_end] {
        m(&bx, 18);
    }
    bx.append(&row);
    bx.append(&errl);
    bx.append(&btns);
    win.set_child(Some(&bx));

    let sources = std::rc::Rc::new(sources);
    let submit = {
        // Weak: these closures live on widgets inside `win`
        let (win, entry, format, errl) = (win.downgrade(), entry.downgrade(), format.downgrade(), errl.downgrade());
        let status = status.clone();
        std::rc::Rc::new(move || {
            let (Some(win), Some(entry), Some(format), Some(errl)) = (win.upgrade(), entry.upgrade(), format.upgrade(), errl.upgrade()) else {
                return;
            };
            let text = entry.text().trim().to_string();
            // A typed extension wins over the drop-down
            let (stem, ext, fmt) = match split_archive_name(&text) {
                Some((s, e, f)) => (s.trim().to_string(), e.to_string(), f),
                None => {
                    let (e, f) = CHOICES[(format.selected() as usize).min(CHOICES.len() - 1)];
                    (text.clone(), e.to_string(), f)
                }
            };
            let problem = if stem.is_empty() {
                Some("Enter a name")
            } else if stem.contains('/') || stem == "." || stem == ".." {
                Some("The name can't contain “/”")
            } else {
                None
            };
            if let Some(p) = problem {
                errl.set_text(p);
                errl.set_visible(true);
                return;
            }
            win.close();
            let (sources, dir) = (sources.as_ref().clone(), dir.clone());
            run_job(&status, format!("Compressing “{stem}{ext}”…"), move |p| {
                match compress(&sources, &dir, &stem, &ext, fmt, p) {
                    Ok(out) => format!("Created “{}”", display_name(&out)),
                    Err(e) => format!("Couldn't compress — {e}"),
                }
            });
        })
    };
    let s = submit.clone();
    okb.connect_clicked(move |_| s());
    entry.connect_activate(move |_| submit());
    let w = win.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(w) = w.upgrade() {
            w.close()
        }
    });
    let keys = gtk::EventControllerKey::new();
    let w = win.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(w) = w.upgrade() {
                w.close();
            }
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    win.add_controller(keys);
    win.present();
    entry.grab_focus();
    entry.select_region(0, -1);
}

/// Extracts each archive next to itself.
pub fn extract_here(status: &gtk::Label, archives: Vec<PathBuf>) {
    let busy = match archives.as_slice() {
        [] => return,
        [one] => format!("Extracting “{}”…", display_name(one)),
        many => format!("Extracting {} archives…", many.len()),
    };
    run_job(status, busy, move |p| {
        let (mut done, mut skipped, mut errors) = (Vec::new(), 0, Vec::new());
        for a in &archives {
            let res = match a.parent() {
                Some(dest) => extract(a, dest, p),
                None => Err(err("no parent folder")),
            };
            match res {
                Ok((out, s)) => {
                    done.push(out);
                    skipped += s;
                }
                Err(e) => errors.push(format!("{}: {e}", display_name(a))),
            }
        }
        let mut msg = match (done.as_slice(), errors.is_empty()) {
            ([one], true) => format!("Extracted to “{}”", display_name(one)),
            (_, true) => format!("Extracted {} archives", done.len()),
            _ => format!("Couldn't extract {}", errors.join("; ")),
        };
        if skipped > 0 {
            msg.push_str(&format!(" ({skipped} special file(s) skipped)"));
        }
        msg
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn scratch() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "rfiles-archive-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn mode(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().mode() & 0o777
    }

    /// proj/{run.sh (755), sub/data.txt, .hidden, link -> sub/data.txt, empty/}
    fn make_tree(root: &Path) -> PathBuf {
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("sub")).unwrap();
        fs::create_dir(proj.join("empty")).unwrap();
        fs::write(proj.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(proj.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(proj.join("sub/data.txt"), &big).unwrap();
        fs::write(proj.join(".hidden"), "h").unwrap();
        std::os::unix::fs::symlink("sub/data.txt", proj.join("link")).unwrap();
        proj
    }

    fn check_tree(proj: &Path) {
        assert_eq!(fs::read_to_string(proj.join("run.sh")).unwrap(), "#!/bin/sh\necho hi\n");
        assert_eq!(mode(&proj.join("run.sh")) & 0o100, 0o100, "executable bit lost");
        assert_eq!(fs::read(proj.join("sub/data.txt")).unwrap().len(), 300_000);
        assert_eq!(fs::read_to_string(proj.join(".hidden")).unwrap(), "h");
        assert!(proj.join("empty").is_dir());
        assert_eq!(fs::read_link(proj.join("link")).unwrap(), Path::new("sub/data.txt"));
    }

    #[test]
    fn round_trip_all_formats() {
        for (ext, fmt) in CHOICES.iter().chain([(".tar", Format::Tar)].iter()) {
            let d = scratch();
            let proj = make_tree(&d);
            let p = Progress::default();
            let out = compress(&[proj], &d, "proj", ext, *fmt, &p).unwrap();
            assert_eq!(display_name(&out), format!("proj{ext}"));
            assert!(p.done.load(Ordering::Relaxed) > 0);
            // A single top folder is extracted as-is; "proj" exists, so "proj (2)"
            let (res, skipped) = extract(&out, &d, &p).unwrap();
            assert_eq!(res, d.join("proj (2)"), "{ext}");
            assert_eq!(skipped, 0);
            check_tree(&res);
            // No scratch folder left behind
            let left: Vec<_> = fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
            assert_eq!(left.len(), 3, "{left:?}");
            fs::remove_dir_all(&d).unwrap();
        }
    }

    #[test]
    fn several_items_go_into_a_folder_named_after_the_archive() {
        let d = scratch();
        fs::write(d.join("a.txt"), "a").unwrap();
        fs::write(d.join("b.txt"), "b").unwrap();
        let p = Progress::default();
        let out = compress(&[d.join("a.txt"), d.join("b.txt")], &d, "Archive", ".zip", Format::Zip, &p).unwrap();
        let (res, _) = extract(&out, &d, &p).unwrap();
        assert_eq!(res, d.join("Archive"));
        assert_eq!(fs::read_to_string(res.join("b.txt")).unwrap(), "b");
        // Again: never overwrite, pick a free name
        let (res2, _) = extract(&out, &d, &p).unwrap();
        assert_eq!(res2, d.join("Archive (2)"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn single_file_goes_into_a_folder() {
        let d = scratch();
        fs::write(d.join("only.txt"), "x").unwrap();
        let p = Progress::default();
        let out = compress(&[d.join("only.txt")], &d, "only", ".tar.gz", Format::TarGz, &p).unwrap();
        let (res, _) = extract(&out, &d, &p).unwrap();
        assert_eq!(res, d.join("only"));
        assert!(res.join("only.txt").is_file());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn unique_names() {
        let d = scratch();
        let (a, _) = create_unique_file(&d, OsStr::new("photos"), ".tar.gz").unwrap();
        let (b, _) = create_unique_file(&d, OsStr::new("photos"), ".tar.gz").unwrap();
        let (c, _) = create_unique_file(&d, OsStr::new("photos"), ".tar.gz").unwrap();
        assert_eq!(display_name(&a), "photos.tar.gz");
        assert_eq!(display_name(&b), "photos (2).tar.gz");
        assert_eq!(display_name(&c), "photos (3).tar.gz");
        fs::create_dir(d.join("x")).unwrap();
        fs::write(d.join("x/keep"), "keep").unwrap();
        let src = d.join("src");
        fs::create_dir(&src).unwrap();
        assert_eq!(move_to_unique(&src, &d, OsStr::new("x")).unwrap(), d.join("x (2)"));
        assert_eq!(fs::read_to_string(d.join("x/keep")).unwrap(), "keep");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn archive_names() {
        assert_eq!(split_archive_name("Photos.TAR.GZ"), Some(("Photos", ".TAR.GZ", Format::TarGz)));
        assert_eq!(split_archive_name("a.b.tgz"), Some(("a.b", ".tgz", Format::TarGz)));
        assert_eq!(split_archive_name("x.tar.xz"), Some(("x", ".tar.xz", Format::TarXz)));
        assert_eq!(split_archive_name("x.zip"), Some(("x", ".zip", Format::Zip)));
        assert!(is_archive("y.tar"));
        assert!(!is_archive("notes.txt"));
        assert!(!is_archive("file.gz"));
    }

    #[test]
    fn safe_paths() {
        assert_eq!(safe_rel(Path::new("./a/b")).unwrap(), Path::new("a/b"));
        assert!(safe_rel(Path::new("./")).unwrap().as_os_str().is_empty());
        assert!(safe_rel(Path::new("../evil")).is_err());
        assert!(safe_rel(Path::new("a/../../evil")).is_err());
        assert!(safe_rel(Path::new("/etc/passwd")).is_err());
    }

    /// A tar with one raw entry, bypassing the tar crate's own path checks.
    fn evil_tar(path: &str, kind: tar::EntryType, link: Option<&str>, body: &[u8]) -> Vec<u8> {
        let mut h = tar::Header::new_old();
        h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        if let Some(l) = link {
            h.as_old_mut().linkname[..l.len()].copy_from_slice(l.as_bytes());
        }
        h.set_entry_type(kind);
        h.set_mode(0o644);
        h.set_size(body.len() as u64);
        h.set_cksum();
        let mut b = tar::Builder::new(Vec::new());
        b.append(&h, body).unwrap();
        b.into_inner().unwrap()
    }

    fn concat_tars(parts: &[Vec<u8>]) -> Vec<u8> {
        // Drop each part's two-block end marker except the last one's
        let mut out = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            let end = if i + 1 == parts.len() { p.len() } else { p.len() - 1024 };
            out.extend_from_slice(&p[..end]);
        }
        out
    }

    fn assert_rejected(d: &Path, archive_name: &str, data: &[u8]) {
        let dest = d.join("dest");
        fs::create_dir_all(&dest).unwrap();
        let a = d.join(archive_name);
        fs::write(&a, data).unwrap();
        let r = extract(&a, &dest, &Progress::default());
        assert!(r.is_err(), "{archive_name} was accepted");
        // Partial output cleaned up
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn zip_slip_is_rejected() {
        let d = scratch();
        assert_rejected(&d, "dotdot.tar", &evil_tar("../evil.txt", tar::EntryType::Regular, None, b"x"));
        assert_rejected(&d, "abs.tar", &evil_tar("/tmp/rfiles-evil.txt", tar::EntryType::Regular, None, b"x"));
        assert!(!d.join("evil.txt").exists());

        // Write through a symlink pointing outside: link -> <outside>, then link/pwned
        let outside = d.join("outside");
        fs::create_dir(&outside).unwrap();
        let link = evil_tar("link", tar::EntryType::Symlink, Some(outside.to_str().unwrap()), b"");
        let file = evil_tar("link/pwned", tar::EntryType::Regular, None, b"x");
        assert_rejected(&d, "symlink.tar", &concat_tars(&[link, file]));
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

        // Hard link to a file outside
        let hl = evil_tar("hl", tar::EntryType::Link, Some("../outside/x"), b"");
        assert_rejected(&d, "hardlink.tar", &hl);

        // Zip with a ".." entry
        let mut z = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        z.start_file("../evil.txt", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"x").unwrap();
        let data = z.finish().unwrap().into_inner();
        assert_rejected(&d, "evil.zip", &data);
        assert!(!d.join("evil.txt").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn corrupt_archive_leaves_nothing() {
        let d = scratch();
        let proj = make_tree(&d);
        let p = Progress::default();
        let out = compress(&[proj], &d, "proj", ".tar.gz", Format::TarGz, &p).unwrap();
        let mut data = fs::read(&out).unwrap();
        data.truncate(data.len() / 2);
        assert_rejected(&d, "cut.tar.gz", &data);
        fs::remove_dir_all(&d).unwrap();
    }
}
