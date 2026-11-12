use crate::util::SortKey;
use gtk::glib;
use std::path::PathBuf;

/// Small plain-text settings in ~/.config/rfiles/.
pub struct Config {
    pub pins: Vec<String>,
    pub starred: Vec<String>,
    pub width: i32,
    pub height: i32,
    pub sidebar_pos: i32,
    pub files_pos: i32,
    /// Width of the right preview pane, in pixels
    pub preview_width: i32,
    pub maximized: bool,
    pub list_view: bool,
    pub show_hidden: bool,
    /// "blue", "system" or "custom:/path/to/image"
    pub folder_icon: String,
    pub icon_size: i32,
    /// Order of the built-in sidebar places, by key
    pub places_order: Vec<String>,
    /// What the file views are sorted by, and whether reversed
    pub sort: SortKey,
    pub sort_desc: bool,
}

fn dir() -> PathBuf {
    glib::user_config_dir().join("rfiles")
}

fn read_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(dir().join(name))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

impl Config {
    pub fn load() -> Self {
        let mut c = Config {
            pins: read_lines("pins"),
            starred: read_lines("starred"),
            width: 1300,
            height: 820,
            sidebar_pos: 220,
            files_pos: 700,
            preview_width: 420,
            maximized: false,
            list_view: false,
            show_hidden: false,
            folder_icon: "blue".into(),
            icon_size: 40,
            places_order: Vec::new(),
            sort: SortKey::Name,
            sort_desc: false,
        };
        for line in read_lines("settings") {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "folder_icon" => {
                    c.folder_icon = v.trim().to_string();
                    continue;
                }
                "places" => {
                    c.places_order = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    continue;
                }
                "sort" => {
                    c.sort = SortKey::parse(v.trim()).unwrap_or(SortKey::Name);
                    continue;
                }
                _ => {}
            }
            let n = v.trim().parse::<i32>().ok();
            match (k.trim(), n) {
                ("width", Some(n)) if n > 300 => c.width = n,
                ("height", Some(n)) if n > 200 => c.height = n,
                ("sidebar", Some(n)) if n > 60 => c.sidebar_pos = n,
                ("files", Some(n)) if n > 150 => c.files_pos = n,
                ("preview", Some(n)) if n > 100 => c.preview_width = n,
                ("maximized", Some(n)) => c.maximized = n != 0,
                ("list_view", Some(n)) => c.list_view = n != 0,
                ("show_hidden", Some(n)) => c.show_hidden = n != 0,
                ("sort_desc", Some(n)) => c.sort_desc = n != 0,
                ("icon_size", Some(n)) if (24..=128).contains(&n) => c.icon_size = n,
                _ => {}
            }
        }
        c
    }

    pub fn save(&self) {
        let d = dir();
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(d.join("pins"), self.pins.join("\n"));
        let _ = std::fs::write(d.join("starred"), self.starred.join("\n"));
        let _ = std::fs::write(
            d.join("settings"),
            format!(
                "width={}\nheight={}\nmaximized={}\nsidebar={}\npreview={}\nfiles={}\nlist_view={}\nshow_hidden={}\nfolder_icon={}\nicon_size={}\nplaces={}\nsort={}\nsort_desc={}\n",
                self.width,
                self.height,
                self.maximized as i32,
                self.sidebar_pos,
                self.preview_width,
                self.files_pos,
                self.list_view as i32,
                self.show_hidden as i32,
                self.folder_icon,
                self.icon_size,
                self.places_order.join(","),
                self.sort.as_str(),
                self.sort_desc as i32
            ),
        );
    }
}
