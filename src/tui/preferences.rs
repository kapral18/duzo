//! The last browsing view, saved outside the scan history.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum BrowseView {
    #[default]
    Chart,
    List,
}

pub(super) fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("duzo").join("view"))
}

pub(super) fn load(path: &Path) -> BrowseView {
    match fs::read_to_string(path).as_deref().map(str::trim) {
        Ok("list") => BrowseView::List,
        _ => BrowseView::Chart,
    }
}

/// A torn write reads back as the chart, the default, so a plain write is
/// enough.
pub(super) fn save(path: &Path, view: BrowseView) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let value = match view {
        BrowseView::Chart => "chart\n",
        BrowseView::List => "list\n",
    };
    fs::write(path, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("duzo-preferences-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn no_saved_view_or_one_duzo_cannot_read_opens_the_chart() {
        let dir = Dir::new("unreadable");
        let path = dir.0.join("view");
        assert_eq!(load(&path), BrowseView::Chart);

        fs::create_dir_all(&dir.0).unwrap();
        fs::write(&path, "sideways\n").unwrap();
        assert_eq!(load(&path), BrowseView::Chart);
    }

    #[test]
    fn a_saved_view_is_read_back() {
        let dir = Dir::new("round-trip");
        let path = dir.0.join("duzo").join("view");

        save(&path, BrowseView::List).unwrap();
        assert_eq!(load(&path), BrowseView::List);
        save(&path, BrowseView::Chart).unwrap();
        assert_eq!(load(&path), BrowseView::Chart);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn saved_views_are_separate_from_disko() {
        if std::env::var_os("DUZO_TEST_VIEW_ISOLATION").is_some() {
            let path = path().unwrap();
            assert_eq!(load(&path), BrowseView::Chart);
            save(&path, BrowseView::List).unwrap();
            return;
        }

        let dir = Dir::new("isolation");
        let config = if cfg!(target_os = "macos") {
            dir.0.join("Library/Application Support")
        } else {
            dir.0.join("config")
        };
        let legacy = config.join("disko/view");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "list\n").unwrap();

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tui::preferences::tests::saved_views_are_separate_from_disko",
            ])
            .env("HOME", &dir.0)
            .env("XDG_CONFIG_HOME", &config)
            .env("DUZO_TEST_VIEW_ISOLATION", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(
            fs::read_to_string(config.join("duzo/view")).unwrap(),
            "list\n"
        );
        assert_eq!(fs::read_to_string(legacy).unwrap(), "list\n");
    }
}
