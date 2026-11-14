//! `rfiles --thumbnail <input> <output-png> <size-px> <mime>`: the helper process thumbs.rs starts to decode
//! untrusted files outside the main process. It must work without a display and report only by exit code.

use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(tag: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("rfiles-it-thumb-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

/// Runs the helper with no display and an empty environment; returns its exit status.
fn helper(input: &Path, out: &Path, px: &str, mime: &str) -> std::process::ExitStatus {
    Command::new(env!("CARGO_BIN_EXE_rfiles"))
        .env_clear()
        .env("HOME", std::env::temp_dir())
        .arg("--thumbnail")
        .arg(input)
        .arg(out)
        .args([px, mime])
        .output()
        .unwrap()
        .status
}

/// Width and height from a PNG's IHDR.
fn png_size(path: &Path) -> (u32, u32) {
    let d = std::fs::read(path).unwrap();
    assert!(d.starts_with(b"\x89PNG\r\n\x1a\n") && &d[12..16] == b"IHDR", "not a PNG");
    let n = |i: usize| u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
    (n(16), n(20))
}

#[test]
fn renders_images_to_png() {
    let d = dir("good");
    let pb = Pixbuf::new(Colorspace::Rgb, false, 8, 400, 100).unwrap();
    pb.fill(0x804020ff);
    for (name, kind, mime) in [("a.png", "png", "image/png"), ("a.jpg", "jpeg", "image/jpeg")] {
        let input = d.0.join(name);
        pb.savev(&input, kind, &[]).unwrap();
        let out = d.0.join(format!("{name}.out.png"));
        assert!(helper(&input, &out, "128", mime).success(), "{name}");
        assert_eq!(png_size(&out), (128, 32), "{name}");
    }
}

#[test]
fn renders_pdf_first_page() {
    let d = dir("pdf");
    let input = d.0.join("doc.pdf");
    let body = "<< /Type /Catalog /Pages 2 0 R >>";
    let pdf = format!(
        "%PDF-1.4\n1 0 obj\n{body}\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
         3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 150] >>\nendobj\n\
         trailer\n<< /Root 1 0 R >>\n%%EOF\n"
    );
    std::fs::write(&input, pdf).unwrap();
    let out = d.0.join("out.png");
    assert!(helper(&input, &out, "256", "application/pdf").success());
    assert_eq!(png_size(&out), (256, 128));
}

#[test]
fn bad_input_fails_without_output() {
    let d = dir("bad");
    let pb = Pixbuf::new(Colorspace::Rgb, true, 8, 64, 64).unwrap();
    pb.fill(0);
    let good = d.0.join("good.png");
    pb.savev(&good, "png", &[]).unwrap();
    let bytes = std::fs::read(&good).unwrap();
    let cut = d.0.join("cut.png");
    std::fs::write(&cut, &bytes[..50]).unwrap();
    let text = d.0.join("text.png");
    std::fs::write(&text, "hello\n").unwrap();
    let empty = d.0.join("empty.png");
    std::fs::write(&empty, "").unwrap();
    let out = d.0.join("out.png");
    for (input, px, mime) in [
        (&cut, "128", "image/png"),
        (&text, "128", "image/png"),
        (&text, "128", "application/pdf"),
        (&empty, "128", "image/png"),
        (&d.0.join("missing.png"), "128", "image/png"),
        // Oversized and nonsense requests
        (&good, "100000", "image/png"),
        (&good, "0", "image/png"),
        (&good, "big", "image/png"),
    ] {
        assert!(!helper(input, &out, px, mime).success(), "{} {px} {mime}", input.display());
        assert!(!out.exists(), "{} left output", input.display());
    }
    // Missing arguments
    let st = Command::new(env!("CARGO_BIN_EXE_rfiles")).env_clear().args(["--thumbnail", "x"]).output().unwrap();
    assert!(!st.status.success() && st.stdout.is_empty());
}
