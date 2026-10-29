//! Parallel directory scanning with cancellation and live progress.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::tree::{DiskEntry, EntryType, ScanState};

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Do not cross into other mounted filesystems.
    pub one_file_system: bool,
    /// Keep children only this many levels below the root. Sizes stay exact —
    /// deeper levels are still walked, just not retained.
    pub max_depth: Option<usize>,
    /// Count a hard-linked file once, under the first path that reaches it.
    pub dedup_hardlinks: bool,
    /// Do not walk into network filesystems — NFS, SMB, sshfs, blobfuse and
    /// friends. Their contents are not on this disk, and stat-ing them one
    /// round trip at a time turns a one-second scan into a ten-minute one.
    ///
    /// A network mount named explicitly as the scan root is still scanned:
    /// asking for it is asking for it.
    pub skip_remote: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            one_file_system: false,
            max_depth: None,
            dedup_hardlinks: true,
            skip_remote: true,
        }
    }
}

/// A directory that has finished being counted, published while the rest of
/// the scan is still running.
///
/// Every figure here is final for that directory — a summary is only sent once
/// its whole subtree is counted. A UI showing these is showing incomplete
/// information, never wrong information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub path: PathBuf,
    pub allocated: u64,
    pub apparent: u64,
    pub items: u64,
}

/// Live counters a UI can poll while a scan runs on another thread.
#[derive(Debug, Default)]
pub struct Progress {
    entries: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    finished: AtomicBool,
    current: Mutex<PathBuf>,
    /// Levels below the root that publish a summary as they complete. Zero
    /// means the caller is not watching, and nothing is queued.
    stream_depth: usize,
    completed: Mutex<Vec<Finished>>,
}

impl Progress {
    /// Publish directory summaries this many levels below the root as they
    /// finish, so a UI can show real numbers before the scan ends.
    pub fn streaming(stream_depth: usize) -> Self {
        Self {
            stream_depth,
            ..Default::default()
        }
    }

    /// Take everything published since the last call.
    pub fn drain_completed(&self) -> Vec<Finished> {
        match self.completed.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(_) => Vec::new(),
        }
    }

    fn publish(&self, path: &Path, depth: usize, entry: &DiskEntry) {
        if depth == 0 || depth > self.stream_depth {
            return;
        }
        if let Ok(mut queue) = self.completed.lock() {
            queue.push(Finished {
                path: path.to_path_buf(),
                allocated: entry.allocated_size,
                apparent: entry.apparent_size,
                items: entry.items,
            });
        }
    }

    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Directories that could not be read, almost always permissions.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    pub fn current(&self) -> PathBuf {
        self.current.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }

    fn saw_entry(&self, bytes: u64) {
        self.entries.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn saw_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Best effort — a busy lock means another worker just published a path,
    /// which is just as good for a progress line.
    fn set_current(&self, path: &Path) {
        if let Ok(mut current) = self.current.try_lock() {
            current.clear();
            current.push(path);
        }
    }
}

/// A cancellation flag shared with a running scan. Cloning shares the flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

struct Ctx<'a> {
    options: &'a ScanOptions,
    progress: &'a Progress,
    cancel: &'a Cancel,
    root_device: u64,
    /// Mount points to stop at, resolved once before the walk starts.
    remote_mounts: Vec<PathBuf>,
    system_mounts: Vec<PathBuf>,
    /// (device, inode) pairs already counted, for hard-link dedup.
    seen_links: Mutex<HashSet<(u64, u64)>>,
}

impl Ctx<'_> {
    fn is_remote_mount(&self, path: &Path) -> bool {
        self.remote_mounts.iter().any(|mount| mount == path)
    }

    /// `system_mounts` is sorted and deduplicated by `system_mount_points_from`;
    /// on Linux it can hold hundreds of rows and is consulted per directory.
    fn is_system_mount(&self, path: &Path) -> bool {
        self.system_mounts
            .binary_search_by(|mount| mount.as_path().cmp(path))
            .is_ok()
    }

    /// True the first time this inode is seen, false for every later link.
    fn first_sighting(&self, meta: &Metadata) -> bool {
        match self.seen_links.lock() {
            Ok(mut seen) => seen.insert((device_of(meta), inode_of(meta))),
            // A poisoned lock means a worker panicked; counting the file again
            // is a better failure than aborting the whole scan.
            Err(_) => true,
        }
    }
}

/// Walk `root`, returning its subtree. Blocks until the walk finishes or
/// `cancel` is tripped, in which case the partial tree is returned.
pub fn scan(
    root: &Path,
    options: &ScanOptions,
    progress: &Progress,
    cancel: &Cancel,
) -> Result<DiskEntry> {
    scan_within(root, options, progress, cancel, &MountBoundaries::default())
}

/// The mount points scans stop at, read from the OS once and then shared.
/// Listing mounts costs 50–100 ms on macOS; a run of small scans started one
/// after another (each folder opened while a whole-disk scan runs) should
/// pay it once, not once per folder. Cloning shares the listing.
#[derive(Clone, Debug, Default)]
pub struct MountBoundaries(Arc<std::sync::OnceLock<(Vec<PathBuf>, Vec<PathBuf>)>>);

fn scan_within(
    root: &Path,
    options: &ScanOptions,
    progress: &Progress,
    cancel: &Cancel,
    boundaries: &MountBoundaries,
) -> Result<DiskEntry> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot read {}", root.display()))?;
    let meta =
        fs::symlink_metadata(&root).with_context(|| format!("cannot stat {}", root.display()))?;

    let (remote_mounts, system_mounts) = boundaries
        .0
        .get_or_init(|| crate::mounts::scan_boundaries(options.skip_remote))
        .clone();
    let remote_mounts = remote_mounts
        .into_iter()
        // Scanning a network mount on purpose is allowed; wandering into
        // one by accident is what this guards against.
        .filter(|mount| mount != &root)
        .collect();

    let ctx = Ctx {
        options,
        progress,
        cancel,
        root_device: device_of(&meta),
        remote_mounts,
        system_mounts: system_mount_points_from(&root, system_mounts),
        seen_links: Mutex::new(HashSet::new()),
    };

    let entry = if meta.is_dir() {
        scan_dir(&root, &meta, 0, &ctx)
    } else {
        leaf_entry(root.as_os_str(), &meta, &ctx)
    };

    progress.finish();
    Ok(entry)
}

/// Same as [`scan`], run on its own thread. Returns the counters to poll, the
/// flag to stop it, and the handle to join for the tree.
pub fn scan_in_background(
    root: PathBuf,
    options: ScanOptions,
) -> (Arc<Progress>, Cancel, JoinHandle<Result<DiskEntry>>) {
    scan_in_background_streaming(root, options, 0)
}

/// As [`scan_in_background`], but publishing directory summaries up to
/// `stream_depth` levels down while the scan runs.
pub fn scan_in_background_streaming(
    root: PathBuf,
    options: ScanOptions,
    stream_depth: usize,
) -> (Arc<Progress>, Cancel, JoinHandle<Result<DiskEntry>>) {
    spawn_scan(
        root,
        options,
        stream_depth,
        None,
        MountBoundaries::default(),
        None,
    )
}

/// As [`scan_in_background`], for a rescan of `root` inside a scan rooted at
/// `within`. It fails before walking anything when that scan would not walk
/// `root`, the same rules as [`scan_in_background_prioritized`].
pub fn scan_in_background_within(
    root: PathBuf,
    within: PathBuf,
    options: ScanOptions,
) -> (Arc<Progress>, Cancel, JoinHandle<Result<DiskEntry>>) {
    spawn_scan(
        root,
        options,
        0,
        None,
        MountBoundaries::default(),
        Some(within),
    )
}

/// As [`scan_in_background`], for a scan a person is waiting on right now
/// while a larger one runs. It walks on its own thread pool: a scan of
/// a whole disk keeps every thread of the shared pool busy, and work queued
/// behind it would wait for that scan rather than run beside it.
///
/// `boundaries` is shared by every prioritized scan of one session, all with
/// the same `skip_remote`, so only the first lists the mounts.
///
/// `root` is a folder the caller already has on screen under that exact
/// path, inside a larger scan rooted at `within`. The scan fails before
/// walking anything when `root` now resolves elsewhere (swapped for a
/// symlink), or when the larger scan would not walk it: it sits in a
/// network share or system mount, or under `-x` on another filesystem.
pub fn scan_in_background_prioritized(
    root: PathBuf,
    within: PathBuf,
    options: ScanOptions,
    boundaries: MountBoundaries,
) -> (Arc<Progress>, Cancel, JoinHandle<Result<DiskEntry>>) {
    spawn_scan(root, options, 0, priority_pool(), boundaries, Some(within))
}

/// Built on first use, as wide as the shared pool so the two split the
/// machine evenly while both run; `None` if the OS refuses the threads, in
/// which case prioritized scans share the global pool like any other.
static PRIORITY_POOL: std::sync::LazyLock<Option<rayon::ThreadPool>> =
    std::sync::LazyLock::new(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|index| format!("duzo-priority-{index}"))
            .build()
            .ok()
    });

fn priority_pool() -> Option<&'static rayon::ThreadPool> {
    PRIORITY_POOL.as_ref()
}

fn spawn_scan(
    root: PathBuf,
    options: ScanOptions,
    stream_depth: usize,
    pool: Option<&'static rayon::ThreadPool>,
    boundaries: MountBoundaries,
    session_root: Option<PathBuf>,
) -> (Arc<Progress>, Cancel, JoinHandle<Result<DiskEntry>>) {
    let progress = Arc::new(Progress::streaming(stream_depth));
    let cancel = Cancel::new();

    let handle = {
        let progress = Arc::clone(&progress);
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            let walk = || {
                if let Some(within) = &session_root {
                    refuse_a_root_its_session_skips(&root, within, &options, &boundaries)?;
                }
                scan_within(&root, &options, &progress, &cancel, &boundaries)
            };
            let result = match pool {
                Some(pool) => pool.install(walk),
                None => walk(),
            };
            progress.finish();
            result
        })
    };

    (progress, cancel, handle)
}

/// A prioritized scan is part of a larger one rooted at `within`, so its
/// root must be a folder that scan would walk: not one that moved (swapped
/// for a symlink), and not inside a mount that scan stops at — a network
/// share, a system mount, or under `-x` another filesystem. Scanning it as a
/// root of its own would exempt it from the very rules that skip it.
fn refuse_a_root_its_session_skips(
    root: &Path,
    within: &Path,
    options: &ScanOptions,
    boundaries: &MountBoundaries,
) -> Result<()> {
    // A root that cannot be resolved at all (gone, unreadable) is left for
    // the scan to report in its own words.
    if root.canonicalize().is_ok_and(|canonical| canonical != root) {
        anyhow::bail!("{} now resolves somewhere else", root.display());
    }
    let (remote, system) = boundaries
        .0
        .get_or_init(|| crate::mounts::scan_boundaries(options.skip_remote));
    // Under `-x`, the device every folder must share with the session's root;
    // `None` when crossing devices is allowed (no `-x`) or the root is gone.
    let session_device = options
        .one_file_system
        .then(|| {
            fs::symlink_metadata(within)
                .ok()
                .map(|meta| device_of(&meta))
        })
        .flatten();
    let skipped = |folder: &Path| {
        remote.iter().any(|mount| mount == folder)
            || system.iter().any(|mount| mount == folder)
            || MACOS_SYSTEM_MOUNTS
                .iter()
                .any(|mount| Path::new(mount) == folder)
            || session_device.is_some_and(|device| {
                fs::symlink_metadata(folder).is_ok_and(|meta| device_of(&meta) != device)
            })
    };
    // Every folder from just below the session's root down to `root`: the
    // session would stop at the first that is a mount it skips.
    let mut below_session = root
        .ancestors()
        .take_while(|folder| *folder != within && folder.starts_with(within));
    if below_session.any(skipped) {
        anyhow::bail!("{} is a mount the scan skips", root.display());
    }
    Ok(())
}

/// The root of a scan keeps the whole path it was given, since there is
/// nothing above it to supply one. Everything below keeps only its own name and
/// gets its path from the walk that reaches it.
fn entry_at(path: &Path, depth: usize, entry_type: EntryType) -> DiskEntry {
    if depth == 0 {
        DiskEntry::new(path.as_os_str(), entry_type)
    } else {
        DiskEntry::new(path.file_name().unwrap_or(path.as_os_str()), entry_type)
    }
}

fn scan_dir(path: &Path, meta: &Metadata, depth: usize, ctx: &Ctx) -> DiskEntry {
    let mut entry = entry_at(path, depth, EntryType::Directory);
    entry.set_filesystem_identity(filesystem_identity(meta));
    // A directory's own inode occupies blocks, which is why `du` on an empty
    // tree is not zero. Its `st_size` is bookkeeping rather than content, so
    // it stays out of the apparent total — the same split `du` makes between
    // its default output and `--apparent-size`.
    entry.allocated_size = allocated_of(meta);
    entry.set_modified(modified_of(meta));
    ctx.progress.saw_entry(entry.allocated_size);

    if ctx.cancel.is_cancelled() {
        entry.set_scan_state(ScanState::Cancelled);
        return entry;
    }
    ctx.progress.set_current(path);

    let contents = match read_dir_entries(path, ctx) {
        Ok(contents) => contents,
        Err(state) => {
            ctx.progress.saw_error();
            entry.set_scan_state(state);
            return entry;
        }
    };
    // Stopped while listing: what was stat'd so far is kept, so a stopped
    // scan still shows what it counted, but the directory says it is short.
    if ctx.cancel.is_cancelled() {
        entry.set_scan_state(ScanState::Cancelled);
    }

    let scan_child = |child: Classified| {
        let (child_path, child_meta) = match child {
            Classified::Leaf(leaf) => return leaf,
            Classified::Subdir(child_path, child_meta) => (child_path, child_meta),
        };
        let crosses_filesystem =
            ctx.options.one_file_system && device_of(&child_meta) != ctx.root_device;
        if crosses_filesystem
            || ctx.is_system_mount(&child_path)
            || ctx.is_remote_mount(&child_path)
        {
            let mut skipped = entry_at(&child_path, depth + 1, EntryType::Directory);
            skipped.set_filesystem_identity(filesystem_identity(&child_meta));
            skipped.set_modified(modified_of(&child_meta));
            skipped.set_scan_state(ScanState::Skipped);
            return skipped;
        }
        scan_dir(&child_path, &child_meta, depth + 1, ctx)
    };
    let subdirs = contents
        .iter()
        .filter(|child| matches!(child, Classified::Subdir(..)))
        .count();
    let mut children: Vec<DiskEntry> = if subdirs <= 1 {
        contents.into_iter().map(scan_child).collect()
    } else {
        contents.into_par_iter().map(scan_child).collect()
    };

    for child in &children {
        entry.absorb(child);
    }

    // Drop children past the depth limit as the recursion unwinds rather than
    // at the end, so a capped scan of `/` never holds the whole tree at once.
    let keep_children = ctx.options.max_depth.is_none_or(|max| depth < max);
    if keep_children {
        children.shrink_to_fit();
        entry.children = children;
    }

    ctx.progress.publish(path, depth, &entry);
    entry
}

/// One directory entry, stat'd and — for a leaf — counted. Built inside the
/// parallel map below rather than after it, so a flat directory (a cache, a
/// `node_modules`) pays for `stat` and for counting its files across every
/// core instead of one.
enum Classified {
    Subdir(PathBuf, Metadata),
    Leaf(DiskEntry),
}

/// The entries of `path` in `read_dir` order, minus those that failed to stat
/// and, once the scan is cancelled, every entry not yet stat'd. `scan_dir`
/// marks a listing cut short that way as `Cancelled`.
fn read_dir_entries(path: &Path, ctx: &Ctx) -> Result<Vec<Classified>, ScanState> {
    let read_dir = match fs::read_dir(path) {
        Ok(read_dir) => read_dir,
        Err(_) => return Err(ScanState::Denied),
    };
    let dir_entries: Vec<fs::DirEntry> = read_dir.filter_map(|result| result.ok()).collect();

    Ok(dir_entries
        .par_iter()
        .filter_map(|dir_entry| {
            // A stopped scan has no use for the rest of a huge directory, and
            // its threads may be wanted by the next scan.
            if ctx.cancel.is_cancelled() {
                return None;
            }
            let child_path = join_exact(path, &dir_entry.file_name());
            // `DirEntry::metadata()` on Apple platforms calls `fstatat` on
            // the shared directory fd for every entry, serialising what
            // should be parallel work; `symlink_metadata` on the joined path
            // does not share that bottleneck.
            match fs::symlink_metadata(&child_path) {
                Ok(meta) if meta.is_dir() => Some(Classified::Subdir(child_path, meta)),
                Ok(meta) => Some(Classified::Leaf(leaf_entry(
                    &dir_entry.file_name(),
                    &meta,
                    ctx,
                ))),
                Err(_) => {
                    ctx.progress.saw_error();
                    None
                }
            }
        })
        .collect())
}

fn leaf_entry(name: &OsStr, meta: &Metadata, ctx: &Ctx) -> DiskEntry {
    let file_type = meta.file_type();
    let entry_type = if file_type.is_symlink() {
        EntryType::Symlink
    } else if file_type.is_file() {
        EntryType::File
    } else {
        EntryType::Other
    };

    let mut entry = DiskEntry::new(name, entry_type);
    entry.set_filesystem_identity(filesystem_identity(meta));
    entry.set_modified(modified_of(meta));

    // A hard link seen a second time contributes nothing: the blocks were
    // already counted under the path that reached it first.
    if ctx.options.dedup_hardlinks && link_count(meta) > 1 && !ctx.first_sighting(meta) {
        ctx.progress.saw_entry(0);
        return entry;
    }

    entry.apparent_size = meta.len();
    entry.allocated_size = allocated_of(meta);
    ctx.progress.saw_entry(entry.allocated_size);
    entry
}

/// `parent/name`, in a buffer sized to hold exactly that.
///
/// The obvious `DirEntry::path()` grows its buffer by doubling and leaves the
/// slack in place. These paths are transient now that entries keep only their
/// names, but there is one per file scanned, and the length is known before the
/// first byte is written.
fn join_exact(parent: &Path, name: &OsStr) -> PathBuf {
    let mut path =
        PathBuf::with_capacity(parent.as_os_str().len() + MAIN_SEPARATOR.len_utf8() + name.len());
    path.push(parent);
    path.push(name);
    path
}

/// Seconds since the Unix epoch, or 0 when the filesystem will not say.
fn modified_of(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
fn allocated_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // `blocks` is always in 512-byte units regardless of the filesystem's own
    // block size — that is what `stat(2)` specifies.
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn allocated_of(meta: &Metadata) -> u64 {
    meta.len()
}

#[cfg(unix)]
fn device_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

#[cfg(not(unix))]
fn device_of(_meta: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn inode_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode_of(_meta: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn filesystem_identity(meta: &Metadata) -> Option<(u64, u64)> {
    Some((device_of(meta), inode_of(meta)))
}

#[cfg(not(unix))]
fn filesystem_identity(_meta: &Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn link_count(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn link_count(_meta: &Metadata) -> u64 {
    1
}

/// sysinfo only reports browsable volumes on macOS, so devfs and the
/// synthetic volumes never reach `scan_boundaries`; they are named here.
#[cfg(target_os = "macos")]
const MACOS_SYSTEM_MOUNTS: [&str; 4] = [
    "/dev",
    "/System/Volumes",
    "/Volumes",
    "/System/Volumes/Data/Volumes",
];
#[cfg(not(target_os = "macos"))]
const MACOS_SYSTEM_MOUNTS: [&str; 0] = [];

fn system_mount_points_from(root: &Path, mut mounts: Vec<PathBuf>) -> Vec<PathBuf> {
    mounts.extend(MACOS_SYSTEM_MOUNTS.into_iter().map(PathBuf::from));
    mounts.retain(|mount| mount != root);
    mounts.sort();
    mounts.dedup();
    mounts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    /// A throwaway directory tree that cleans itself up.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("duzo-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self, relative: &str, bytes: usize) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut file = File::create(&path).unwrap();
            file.write_all(&vec![b'x'; bytes]).unwrap();
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scan_tree(root: &Path, options: ScanOptions) -> DiskEntry {
        let progress = Progress::default();
        scan(root, &options, &progress, &Cancel::new()).unwrap()
    }

    #[test]
    fn apparent_size_is_exactly_the_sum_of_file_lengths() {
        let tree = TempTree::new("sums");
        tree.file("a.txt", 1000);
        tree.file("nested/b.txt", 2000);
        tree.file("nested/deeper/c.txt", 3000);

        let root = scan_tree(&tree.0, ScanOptions::default());

        // Matches `du --apparent-size`: directory inodes are bookkeeping, not
        // content, so they do not inflate the total.
        assert_eq!(root.apparent_size, 6000);
        assert_eq!(root.scan_state(), ScanState::Complete);
    }

    #[test]
    fn allocated_size_adds_each_directory_to_what_is_underneath() {
        let tree = TempTree::new("allocated");
        tree.file("nested/b.txt", 2000);

        let root = scan_tree(&tree.0, ScanOptions::default());
        let nested = root.child("nested").unwrap();
        let file = nested.child("b.txt").unwrap();

        // How much a directory inode itself occupies is filesystem-specific:
        // ext4 charges a block, APFS reports none. Either way the totals
        // accumulate upward and never lose what is below.
        assert_eq!(
            root.allocated_size,
            own_blocks(&tree.0) + nested.allocated_size
        );
        assert_eq!(
            nested.allocated_size,
            own_blocks(&tree.0.join("nested")) + file.allocated_size
        );
        // A 2000-byte file always occupies whole blocks.
        assert!(file.allocated_size >= 2000);
        assert_eq!(nested.apparent_size, 2000);
    }

    /// What the filesystem charges for one entry, ignoring its contents.
    fn own_blocks(path: &Path) -> u64 {
        allocated_of(&fs::symlink_metadata(path).unwrap())
    }

    #[test]
    fn max_depth_prunes_children_but_keeps_sizes_exact() {
        let tree = TempTree::new("depth");
        tree.file("nested/deeper/c.txt", 4000);

        let full = scan_tree(&tree.0, ScanOptions::default());
        let capped = scan_tree(
            &tree.0,
            ScanOptions {
                max_depth: Some(1),
                ..Default::default()
            },
        );

        assert_eq!(full.apparent_size, capped.apparent_size);
        assert_eq!(capped.children.len(), 1);
        assert!(capped.children[0].children.is_empty());
        assert!(!full.children[0].children.is_empty());
    }

    #[test]
    fn counts_every_entry_including_directories() {
        let tree = TempTree::new("items");
        tree.file("a.txt", 10);
        tree.file("nested/b.txt", 10);

        let root = scan_tree(&tree.0, ScanOptions::default());

        // root + a.txt + nested + nested/b.txt
        assert_eq!(root.items, 4);
    }

    #[test]
    fn a_large_flat_directory_lists_every_entry_once() {
        // Every entry in a directory is stat'd and counted inside the same
        // parallel map; large enough that a per-entry ordering bug would show
        // up as a missing or duplicated file rather than by chance passing.
        let count = 600;
        let tree = TempTree::new("parallel-stat");
        let mut names: Vec<String> = Vec::with_capacity(count);
        for i in 0..count {
            let name = format!("file-{i:04}.txt");
            tree.file(&name, 10);
            names.push(name);
        }

        let root = scan_tree(&tree.0, ScanOptions::default());

        assert_eq!(root.children.len(), count);
        assert_eq!(root.apparent_size, (count * 10) as u64);
        // root + every flat file.
        assert_eq!(root.items, count as u64 + 1);

        let mut seen: Vec<String> = root
            .children
            .iter()
            .map(|child| child.name().into_owned())
            .collect();
        seen.sort();
        let mut expected = names;
        expected.sort();
        assert_eq!(
            seen, expected,
            "every entry must still be present exactly once"
        );
    }

    #[test]
    fn children_keep_the_order_read_dir_lists_them_in() {
        let tree = TempTree::new("order");
        for i in 0..8 {
            tree.file(&format!("file-{i}.txt"), 10);
            tree.file(&format!("dir-{i}/inner.txt"), 10);
        }

        let root = scan_tree(&tree.0, ScanOptions::default());

        let listed: Vec<String> = fs::read_dir(&tree.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let scanned: Vec<String> = root
            .children
            .iter()
            .map(|child| child.name().into_owned())
            .collect();
        assert_eq!(scanned, listed);
    }

    #[test]
    fn cancelled_scan_returns_a_partial_tree() {
        let tree = TempTree::new("cancel");
        tree.file("a.txt", 10);

        let progress = Progress::default();
        let cancel = Cancel::new();
        cancel.cancel();
        let root = scan(&tree.0, &ScanOptions::default(), &progress, &cancel).unwrap();

        assert_eq!(root.scan_state(), ScanState::Cancelled);
        assert!(root.children.is_empty());
    }

    /// `r` rescans a folder inside the session it belongs to, under the same
    /// rules a prioritized scan follows.
    #[cfg(unix)]
    #[test]
    fn a_rescan_within_a_session_refuses_a_moved_folder() {
        let tree = TempTree::new("moved-rescan");
        tree.file("target/one.txt", 1000);
        let base = tree.0.canonicalize().unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(base.join("target"), &link).unwrap();

        let (progress, _, handle) =
            scan_in_background_within(link, base.clone(), ScanOptions::default());

        assert!(handle.join().unwrap().is_err());
        assert_eq!(progress.entries(), 0, "the link's target was walked");
    }

    #[test]
    fn a_rescan_of_a_vanished_folder_says_it_is_gone() {
        let tree = TempTree::new("vanished-rescan");
        let base = tree.0.canonicalize().unwrap();

        let (_, _, handle) =
            scan_in_background_within(base.join("gone"), base.clone(), ScanOptions::default());

        let error = handle.join().unwrap().unwrap_err().to_string();
        assert!(!error.contains("resolves somewhere else"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_prioritized_scan_of_a_moved_folder_walks_nothing() {
        let tree = TempTree::new("moved-root");
        tree.file("target/one.txt", 1000);
        let base = tree.0.canonicalize().unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(base.join("target"), &link).unwrap();

        let (progress, _, handle) = scan_in_background_prioritized(
            link,
            base.clone(),
            ScanOptions::default(),
            MountBoundaries::default(),
        );

        assert!(handle.join().unwrap().is_err());
        assert_eq!(progress.entries(), 0, "the link's target was walked");
    }

    #[test]
    fn a_prioritized_scan_inside_a_mount_its_session_skips_walks_nothing() {
        let tree = TempTree::new("skipped-root");
        tree.file("mounted/inner/one.txt", 1000);
        let base = tree.0.canonicalize().unwrap();
        let mounted = base.join("mounted");
        // The session's mount listing names `mounted` as a system mount; the
        // folder opened is a folder inside it.
        let boundaries = MountBoundaries::default();
        boundaries
            .0
            .set((Vec::new(), vec![mounted.clone()]))
            .unwrap();

        let (progress, _, handle) = scan_in_background_prioritized(
            mounted.join("inner"),
            base,
            ScanOptions::default(),
            boundaries,
        );

        assert!(handle.join().unwrap().is_err());
        assert_eq!(progress.entries(), 0, "the skipped mount was walked");
    }

    #[test]
    fn stopping_while_a_big_folder_is_listed_keeps_what_was_counted() {
        let tree = TempTree::new("cancel-listing");
        const FILES: usize = 5000;
        for index in 0..FILES {
            File::create(tree.0.join(format!("f{index}"))).unwrap();
        }

        // Racing a stop against a listing can lose: the listing may finish
        // first on a busy machine. Every run must be consistent; retry until
        // one is cut, which is the run this test is about.
        for _ in 0..50 {
            let (progress, cancel, handle) =
                scan_in_background(tree.0.clone(), ScanOptions::default());
            while progress.entries() < 50 && !handle.is_finished() {
                std::hint::spin_loop();
            }
            cancel.cancel();
            let root = handle.join().unwrap().unwrap();

            assert_eq!(root.items, 1 + root.children.len() as u64);
            if root.children.len() < FILES {
                assert_eq!(
                    root.scan_state(),
                    ScanState::Cancelled,
                    "a cut listing says so"
                );
                assert!(!root.children.is_empty(), "what was counted stays");
                return;
            }
        }
        panic!("no run was stopped before the listing finished");
    }

    #[test]
    fn completed_directories_are_published_while_the_scan_runs() {
        let tree = TempTree::new("stream");
        tree.file("alpha/one.txt", 1000);
        tree.file("beta/two.txt", 2000);
        tree.file("beta/deeper/three.txt", 3000);

        let progress = Progress::streaming(2);
        let root = scan(&tree.0, &ScanOptions::default(), &progress, &Cancel::new()).unwrap();

        let published = progress.drain_completed();
        let names: Vec<String> = published
            .iter()
            .map(|done| done.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        // Two levels down, and never the root itself.
        assert!(names.contains(&"alpha".to_string()), "{names:?}");
        assert!(names.contains(&"beta".to_string()), "{names:?}");
        assert!(names.contains(&"deeper".to_string()), "{names:?}");
        assert!(!names.contains(&root.name().to_string()), "{names:?}");

        // Every published figure is final, not a running partial.
        let beta = published
            .iter()
            .find(|done| done.path.ends_with("beta"))
            .unwrap();
        assert_eq!(beta.apparent, 5000);

        // Draining twice does not repeat anything.
        assert!(progress.drain_completed().is_empty());
    }

    #[test]
    fn nothing_is_published_when_nobody_is_watching() {
        let tree = TempTree::new("nostream");
        tree.file("alpha/one.txt", 1000);

        let progress = Progress::default();
        scan(&tree.0, &ScanOptions::default(), &progress, &Cancel::new()).unwrap();

        assert!(progress.drain_completed().is_empty());
    }

    #[test]
    fn streaming_stops_at_the_requested_depth() {
        let tree = TempTree::new("streamdepth");
        tree.file("a/b/c/deep.txt", 100);

        let progress = Progress::streaming(1);
        scan(&tree.0, &ScanOptions::default(), &progress, &Cancel::new()).unwrap();

        let published = progress.drain_completed();
        assert_eq!(published.len(), 1);
        assert!(published[0].path.ends_with("a"));
    }

    #[test]
    fn progress_tracks_entries_and_bytes() {
        let tree = TempTree::new("progress");
        tree.file("a.txt", 1000);
        tree.file("b.txt", 1000);

        let progress = Progress::default();
        let root = scan(&tree.0, &ScanOptions::default(), &progress, &Cancel::new()).unwrap();

        assert_eq!(progress.entries(), root.items);
        assert_eq!(progress.bytes(), root.allocated_size);
        assert!(progress.is_finished());
    }

    #[test]
    fn scanning_a_single_file_yields_one_entry() {
        let tree = TempTree::new("file");
        tree.file("solo.txt", 512);

        let root = scan_tree(&tree.0.join("solo.txt"), ScanOptions::default());

        assert_eq!(root.entry_type, EntryType::File);
        assert_eq!(root.apparent_size, 512);
        assert_eq!(root.items, 1);
    }

    #[test]
    fn joined_paths_are_correct_and_carry_no_slack() {
        let joined = join_exact(Path::new("/home/someone/code"), OsStr::new("duzo"));

        assert_eq!(joined, PathBuf::from("/home/someone/code/duzo"));
        // The whole point: no room left over. Every entry in a scan keeps its
        // path for the session, so a byte of slack here is a byte per file.
        assert!(
            joined.capacity() <= joined.as_os_str().len() + 1,
            "capacity {} for a path of {} bytes",
            joined.capacity(),
            joined.as_os_str().len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn every_scanned_entry_keeps_its_filesystem_identity() {
        use std::os::unix::fs::MetadataExt;

        let tree = TempTree::new("entry-identities");
        tree.file("child.bin", 100);
        // The scan keeps the canonical path; the temp dir may sit behind a
        // symlink (`/var` on macOS).
        let file = tree.0.canonicalize().unwrap().join("child.bin");
        let root_meta = fs::symlink_metadata(&tree.0).unwrap();
        let file_meta = fs::symlink_metadata(&file).unwrap();

        let scanned = scan_tree(&tree.0, ScanOptions::default());

        assert_eq!(
            scanned.filesystem_identity(),
            Some((root_meta.dev(), root_meta.ino()))
        );
        assert_eq!(
            scanned.resolve(&file).unwrap().filesystem_identity(),
            Some((file_meta.dev(), file_meta.ino()))
        );
    }

    #[test]
    fn only_the_root_of_a_scan_keeps_a_whole_path() {
        let tree = TempTree::new("names");
        tree.file("some/reasonably/deep/nesting/file.txt", 10);

        let root = scan_tree(&tree.0, ScanOptions::default());
        assert_eq!(root.root_path(), tree.0.canonicalize().unwrap());

        fn check(entry: &DiskEntry) {
            for child in &entry.children {
                // The whole point: a child holds its own name and nothing of
                // where it sits, so a directory's name is stored once rather
                // than once per file beneath it.
                assert!(
                    !child.name_os().as_encoded_bytes().contains(&b'/'),
                    "{:?} carries a path, not a name",
                    child.name_os()
                );
                assert_eq!(
                    child.children.capacity(),
                    child.children.len(),
                    "{:?} keeps a child list bigger than its child count",
                    child.name_os()
                );
                check(child);
            }
        }
        check(&root);

        // ...and paths still come back out of it intact.
        let deep = tree.0.canonicalize().unwrap().join("some/reasonably/deep");
        assert!(
            root.resolve(&deep).is_some(),
            "{} did not resolve",
            deep.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_are_counted_once() {
        let tree = TempTree::new("links");
        tree.file("original.txt", 4096);
        fs::hard_link(tree.0.join("original.txt"), tree.0.join("link.txt")).unwrap();

        let deduped = scan_tree(&tree.0, ScanOptions::default());
        let raw = scan_tree(
            &tree.0,
            ScanOptions {
                dedup_hardlinks: false,
                ..Default::default()
            },
        );

        assert_eq!(raw.apparent_size - deduped.apparent_size, 4096);
        assert_eq!(deduped.items, raw.items);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_counted_as_links_not_traversed() {
        let tree = TempTree::new("directory-symlink");
        tree.file("target/only-once.bin", 4096);
        std::os::unix::fs::symlink(tree.0.join("target"), tree.0.join("alias")).unwrap();

        let scanned = scan_tree(&tree.0, ScanOptions::default());
        // The scan keeps the canonical path; the temp dir may sit behind a
        // symlink (`/var` on macOS).
        let alias = scanned
            .resolve(&tree.0.canonicalize().unwrap().join("alias"))
            .unwrap();

        assert_eq!(alias.entry_type, EntryType::Symlink);
        assert!(alias.children.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_system_folders_are_boundaries_unless_scanned_directly() {
        let mounts = system_mount_points_from(Path::new("/"), Vec::new());
        assert!(mounts.contains(&PathBuf::from("/System/Volumes")));
        assert!(mounts.contains(&PathBuf::from("/Volumes")));
        assert!(mounts.contains(&PathBuf::from("/dev")));

        let mounts = system_mount_points_from(Path::new("/Volumes"), Vec::new());
        assert!(!mounts.contains(&PathBuf::from("/Volumes")));
    }

    #[test]
    fn discovered_pseudo_mounts_are_included_except_for_an_explicit_root() {
        let mounts = vec![PathBuf::from("/snap/core/1"), PathBuf::from("/proc")];

        let filtered = system_mount_points_from(Path::new("/proc"), mounts);

        assert!(filtered.contains(&PathBuf::from("/snap/core/1")));
        assert!(!filtered.contains(&PathBuf::from("/proc")));
    }

    #[test]
    fn scan_dir_routes_configured_system_mounts_through_the_skip_branch() {
        let tree = TempTree::new("system-mount");
        let mount = tree.0.join("mounted");
        fs::create_dir(&mount).unwrap();
        tree.file("mounted/external.bin", 4096);
        let root = tree.0.canonicalize().unwrap();
        let mount = mount.canonicalize().unwrap();
        let meta = fs::symlink_metadata(&root).unwrap();
        let options = ScanOptions::default();
        let progress = Progress::default();
        let cancel = Cancel::new();
        let ctx = Ctx {
            options: &options,
            progress: &progress,
            cancel: &cancel,
            root_device: device_of(&meta),
            remote_mounts: Vec::new(),
            system_mounts: vec![mount.clone()],
            seen_links: Mutex::new(HashSet::new()),
        };

        let ordinary = root.join("ordinary");
        fs::create_dir(&ordinary).unwrap();
        tree.file("ordinary/local.bin", 1024);

        let scanned = scan_dir(&root, &meta, 0, &ctx);
        let skipped = scanned.resolve(&mount).unwrap();

        assert_eq!(skipped.scan_state(), ScanState::Skipped);
        assert!(skipped.children.is_empty());
        assert!(scanned.resolve(&ordinary.join("local.bin")).is_some());
    }
}
