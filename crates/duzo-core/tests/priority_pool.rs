//! Its own test binary: it holds every thread of rayon's shared pool, which
//! would stall any other scan test running beside it in the same process.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

use duzo_core::ScanOptions;
use duzo_core::scan::{MountBoundaries, scan_in_background_prioritized};

/// Lets the held threads go even when an assertion fails first.
struct Release(Arc<AtomicBool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
fn a_prioritized_scan_finishes_while_the_shared_pool_is_busy() {
    let base = std::env::temp_dir().join(format!("duzo-priority-pool-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("a")).unwrap();
    std::fs::create_dir_all(base.join("b")).unwrap();
    std::fs::write(base.join("a/one.txt"), vec![b'x'; 1000]).unwrap();
    std::fs::write(base.join("b/two.txt"), vec![b'x'; 2000]).unwrap();
    // A prioritized scan refuses a root that resolves elsewhere; the temp dir
    // is behind a symlink on macOS (`/var` → `/private/var`).
    let base = base.canonicalize().unwrap();

    // Hold every thread of the shared pool, the way a full-disk scan does.
    // The barrier proves all of them are held before the scan starts.
    let threads = rayon::current_num_threads();
    let held = Arc::new(Barrier::new(threads + 1));
    let release = Release(Arc::new(AtomicBool::new(false)));
    for _ in 0..threads {
        let held = Arc::clone(&held);
        let released = Arc::clone(&release.0);
        rayon::spawn(move || {
            held.wait();
            while !released.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
    }
    held.wait();

    let (_, _, handle) = scan_in_background_prioritized(
        base.clone(),
        base.parent().unwrap().to_path_buf(),
        ScanOptions::default(),
        MountBoundaries::default(),
    );
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(handle.join());
    });
    let result = finished.recv_timeout(Duration::from_secs(5));
    drop(release);

    let root = result
        .expect("the prioritized scan waited for the busy pool")
        .unwrap()
        .unwrap();
    assert_eq!(root.apparent_size, 3000);
    let _ = std::fs::remove_dir_all(&base);
}
