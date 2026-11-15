//! Opening files in other apps, with RFiles' environment minus what it set
//! only for itself (see `gpu::set_here`). `gtk::FileLauncher` can't do that:
//! it gives no say over the app's environment.

use gtk::{gio, glib, prelude::*};

/// Removes from `ctx` what RFiles set only for itself. Apps GIO spawns
/// (also through gio-launch-desktop) get exactly the context's environment;
/// apps started over D-Bus don't get RFiles' environment at all.
pub fn strip(ctx: &impl IsA<gio::AppLaunchContext>) {
    if let Some(var) = crate::gpu::set_here() {
        ctx.unsetenv(var);
    }
}

/// Starts `app` with `file`, telling the status bar when that fails.
fn start(parent: &gtk::Window, app: &gio::AppInfo, file: &gio::File, status: &gtk::Label) {
    // The display's context gives the app startup notification and focus
    let ctx = WidgetExt::display(parent).app_launch_context();
    strip(&ctx);
    let launched = app.launch_uris_future(&[&file.uri()], Some(&ctx));
    let status = status.downgrade();
    glib::spawn_future_local(async move {
        if let (Err(e), Some(status)) = (launched.await, status.upgrade()) {
            status.set_text(&format!("Couldn't open: {}", e.message()));
        }
    });
}

/// Opens `file` in its default app, or lets the user pick one when there is none.
pub fn open(parent: &gtk::Window, file: &gio::File, status: &gtk::Label) {
    let (parent, status, file) = (parent.downgrade(), status.downgrade(), file.clone());
    glib::spawn_future_local(async move {
        let app = file.query_default_handler_future(glib::Priority::DEFAULT).await;
        let (Some(parent), Some(status)) = (parent.upgrade(), status.upgrade()) else { return };
        match app {
            Ok(app) => start(&parent, &app, &file, &status),
            // "No application is registered as handling this file"
            Err(e) if e.matches(gio::IOErrorEnum::NotSupported) => choose(&parent, &file, &status),
            Err(e) => status.set_text(&format!("Couldn't open: {}", e.message())),
        }
    });
}

/// "Open With…": a small window with the apps for the file's type, built like
/// Properties' "Opens with" list (the GTK app chooser widgets are deprecated).
pub fn choose(parent: &gtk::Window, file: &gio::File, status: &gtk::Label) {
    let (parent, status, file) = (parent.downgrade(), status.downgrade(), file.clone());
    glib::spawn_future_local(async move {
        let attrs = "standard::content-type,standard::display-name";
        let info = file.query_info_future(attrs, gio::FileQueryInfoFlags::NONE, glib::Priority::DEFAULT).await;
        let (Some(parent), Some(status)) = (parent.upgrade(), status.upgrade()) else { return };
        let info = match info {
            Ok(i) => i,
            Err(e) => {
                status.set_text(&format!("Couldn't open: {}", e.message()));
                return;
            }
        };
        let ct = info.content_type().map(|c| c.to_string()).unwrap_or_else(|| "application/octet-stream".into());
        chooser_window(&parent, &file, &info.display_name(), crate::properties::apps_for_type(&ct), &status);
    });
}

fn chooser_window(parent: &gtk::Window, file: &gio::File, name: &str, apps: Vec<gio::AppInfo>, status: &gtk::Label) {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.set_activate_on_single_click(true);
    for a in &apps {
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        line.set_margin_top(4);
        line.set_margin_bottom(4);
        line.set_margin_start(6);
        line.set_margin_end(6);
        let img = gtk::Image::new();
        img.set_pixel_size(24);
        match a.icon() {
            Some(i) => img.set_from_gicon(&i),
            None => img.set_icon_name(Some("application-x-executable-symbolic")),
        }
        line.append(&img);
        line.append(&gtk::Label::new(Some(&a.name())));
        list.append(&line);
    }
    let title = gtk::Label::new(Some(&format!("Open “{name}” with")));
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    title.set_max_width_chars(40);
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 10);
    bx.set_margin_top(12);
    bx.set_margin_bottom(12);
    bx.set_margin_start(12);
    bx.set_margin_end(12);
    bx.append(&title);
    if apps.is_empty() {
        let none = gtk::Label::new(Some("No apps found for this file type."));
        none.add_css_class("dim");
        none.set_xalign(0.0);
        bx.append(&none);
    } else {
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(360)
            .child(&list)
            .build();
        bx.append(&scroll);
    }
    let win = gtk::Window::builder()
        .title("Open With")
        .modal(true)
        .destroy_with_parent(true)
        .default_width(360)
        .child(&bx)
        .build();
    win.set_transient_for(Some(parent));
    win.add_css_class("rfiles");
    let keys = gtk::ShortcutController::new();
    keys.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::NamedAction::new("window.close")),
    ));
    win.add_controller(keys);

    let (parent, status, w, file) = (parent.downgrade(), status.downgrade(), win.downgrade(), file.clone());
    list.connect_row_activated(move |_, r| {
        let (Some(app), Some(parent), Some(status)) = (apps.get(r.index() as usize), parent.upgrade(), status.upgrade())
        else {
            return;
        };
        start(&parent, app, &file, &status);
        if let Some(w) = w.upgrade() {
            w.close();
        }
    });
    win.present();
    if let Some(first) = list.row_at_index(0) {
        first.grab_focus();
    }
}

#[cfg(test)]
mod child_env_tests {
    use super::*;

    const VAR: &str = "VK_LOADER_DRIVERS_SELECT";
    const MARK: &str = "RFILES_CHILD_ENV_TEST";

    /// Starts this test again in a fresh process whose environment has the
    /// variable from the start, as RFiles' has after `gpu` set it (setting it
    /// here would race with the other test threads).
    #[test]
    fn children_dont_inherit_what_gpu_set() {
        if std::env::var_os(MARK).is_some() {
            in_child();
            return;
        }
        let dir = std::env::temp_dir().join(format!("rfiles-child-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["launch::child_env_tests::children_dont_inherit_what_gpu_set", "--exact", "--test-threads=1", "--nocapture"])
            .env(MARK, &dir)
            .env("XDG_DATA_HOME", &dir)
            .env(VAR, "*rfiles-test*")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{text}");
        // The filter matched the test, so it really ran in the child
        assert!(text.contains("1 passed"), "{text}");
        println!("{text}");
    }

    fn in_child() {
        let dir = std::path::PathBuf::from(std::env::var_os(MARK).unwrap());
        crate::gpu::remember(VAR);
        // Still in RFiles' own environment, for GTK's later renderers
        assert_eq!(std::env::var(VAR).as_deref(), Ok("*rfiles-test*"));

        // "Open in Terminal": the same command, with `env` as the terminal
        let mut cmd = crate::fileops::terminal_command("sh", &["-c".into(), "env; pwd".into()], &dir);
        let removed: Vec<_> = cmd.get_envs().filter(|(_, v)| v.is_none()).map(|(k, _)| k.to_owned()).collect();
        assert_eq!(removed, [VAR]);
        let out = String::from_utf8(cmd.output().map(|o| o.stdout).unwrap()).unwrap();
        assert!(out.contains(&format!("{MARK}=")), "child got the rest of the environment");
        assert!(!out.contains(VAR), "{out}");
        assert!(out.ends_with(&format!("{}\n", dir.display())), "{out}");

        // Opening a file: a desktop file whose Exec writes its environment,
        // started the way `start` does (GIO spawns it, through gio-launch-desktop when installed)
        let env_file = dir.join("env.txt");
        std::fs::create_dir_all(dir.join("applications")).unwrap();
        let desktop = dir.join("applications/rfiles-env-test.desktop");
        let exec = format!("sh -c 'env > {}.part; mv {0}.part {0}' sh %f", env_file.display());
        std::fs::write(&desktop, format!("[Desktop Entry]\nType=Application\nName=Env test\nExec={exec}\n")).unwrap();
        // Found like any installed app (XDG_DATA_HOME points here)
        let app = gio::AppInfo::all().into_iter().find(|a| a.id().as_deref() == Some("rfiles-env-test.desktop"));
        let app = app.expect("test app installed");
        let ctx = gio::AppLaunchContext::new();
        strip(&ctx);
        let file = gio::File::for_path(&desktop);
        let launched = app.launch_uris_future(&[&file.uri()], Some(&ctx));
        glib::MainContext::default().block_on(launched).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !env_file.exists() {
            assert!(std::time::Instant::now() < deadline, "the test app never ran");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let env = std::fs::read_to_string(&env_file).unwrap();
        assert!(env.contains(&format!("{MARK}=")), "{env}");
        assert!(!env.contains(VAR), "{env}");
        assert!(env.contains("GIO_LAUNCHED_DESKTOP_FILE="), "started as a desktop app: {env}");
        let wrapper = env.contains("GIO_LAUNCHED_DESKTOP_FILE_PID=");
        println!("started through gio-launch-desktop: {wrapper}");
    }

    #[test]
    fn strip_without_gpu_choice_changes_nothing() {
        // This process set nothing, so a variable the user set reaches children
        let ctx = gio::AppLaunchContext::new();
        let before = ctx.environment();
        strip(&ctx);
        assert_eq!(ctx.environment(), before);
    }
}
