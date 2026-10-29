#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use duzo_core::{DiskEntry, EntryType, Store};

struct Fixture {
    home: PathBuf,
    data: PathBuf,
    scanned: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = std::env::temp_dir().join(format!("duzo-identity-{}", std::process::id()));
        fs::create_dir(&home).unwrap();
        let scanned = home.join("scanned");
        fs::create_dir(&scanned).unwrap();
        fs::write(scanned.join("payload"), vec![b'x'; 4096]).unwrap();
        let data = if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else {
            home.join("data")
        };
        Self {
            home,
            data,
            scanned: scanned.canonicalize().unwrap(),
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_duzo"))
            .args(arguments)
            .arg(&self.scanned)
            .arg("--json")
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

#[test]
fn scans_keep_their_history_separate_from_disko() {
    let fixture = Fixture::new();
    let legacy = Store::at(fixture.data.join("disko/snapshots")).unwrap();
    let mut tree = DiskEntry::new(&fixture.scanned, EntryType::Directory);
    tree.allocated_size = 8192;
    tree.apparent_size = 8192;
    legacy.record_at(&tree, 1).unwrap();

    let before = fixture.run(&["history"]);
    let before: serde_json::Value = serde_json::from_slice(&before.stdout).unwrap();
    assert_eq!(before, serde_json::json!([]));

    fixture.run(&["--apparent"]);
    let after = fixture.run(&["history"]);
    let after: serde_json::Value = serde_json::from_slice(&after.stdout).unwrap();
    assert_eq!(after.as_array().unwrap().len(), 1);
    assert_eq!(after[0]["total_apparent"], 4096);
    assert_eq!(legacy.list(&fixture.scanned).len(), 1);
    assert_eq!(
        legacy.latest(&fixture.scanned).unwrap().total_apparent,
        8192
    );
}
