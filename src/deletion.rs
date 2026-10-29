//! Deleting things, carefully.
//!
//! This is the one part of duzo that cannot be undone, so every path is
//! checked against the scan it came from immediately before it is removed —
//! not when the list was drawn, which may have been minutes ago.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use duzo_core::scan::Cancel;

// Only the Unix descriptor walk can check identities.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy)]
struct ExpectedIdentities {
    root: Option<(u64, u64)>,
    target: Option<(u64, u64)>,
}

/// Something the user has asked to remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    /// Size shown in the confirmation, in the currently selected metric.
    pub size: u64,
    /// Scanner allocation used to decide whether in-memory totals remain valid.
    pub allocated_size: u64,
    /// Filesystem identity captured by the scan that produced this target.
    pub filesystem_identity: Option<(u64, u64)>,
    pub is_dir: bool,
}

/// Why duzo will not touch a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Outside the tree on screen. Deleting something you are not looking at
    /// is never what was meant.
    OutsideScan,
    /// The directory the scan started from.
    ScanRoot,
    /// A mount point: removing it would not free space here, and might free a
    /// great deal somewhere else.
    MountPoint,
    /// Vanished between the scan and now.
    Missing,
    /// A path with no parent — `/` and friends.
    Filesystem,
    /// The user stopped the deletion before reaching this one.
    Stopped,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Refusal::OutsideScan => "outside the current scan",
            Refusal::ScanRoot => "this is the folder being scanned",
            Refusal::MountPoint => "this is a mount point",
            Refusal::Missing => "no longer exists",
            Refusal::Filesystem => "this is a filesystem root",
            Refusal::Stopped => "stopped before reaching it",
        };
        f.write_str(reason)
    }
}

/// Decide whether `path` may be removed as part of a scan rooted at `root`.
pub fn check(path: &Path, root: &Path) -> Result<(), Refusal> {
    if path.parent().is_none() {
        return Err(Refusal::Filesystem);
    }
    if path == root {
        return Err(Refusal::ScanRoot);
    }
    if !path.starts_with(root) {
        return Err(Refusal::OutsideScan);
    }
    // symlink_metadata, so a symlink is judged as itself rather than as
    // whatever it points at.
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Err(Refusal::Missing);
    };
    if meta.is_dir() && is_mount_point(path) {
        return Err(Refusal::MountPoint);
    }
    Ok(())
}

/// Live counters for a deletion running on another thread.
///
/// Removing 68 GB of build caches takes long enough that a UI without these
/// looks like it has hung.
#[derive(Debug, Default)]
pub struct DeleteProgress {
    items_done: AtomicUsize,
    items_total: AtomicUsize,
    files_removed: AtomicU64,
    freed: AtomicU64,
    links_kept: AtomicU64,
    finished: AtomicBool,
    current: Mutex<PathBuf>,
}

impl DeleteProgress {
    pub fn new(total: usize) -> Self {
        let progress = Self::default();
        progress.items_total.store(total, Ordering::Relaxed);
        progress
    }

    /// Which of the requested items is being worked on, 1-based.
    pub fn items_done(&self) -> usize {
        self.items_done.load(Ordering::Relaxed)
    }

    pub fn items_total(&self) -> usize {
        self.items_total.load(Ordering::Relaxed)
    }

    /// Individual files and directories unlinked so far. The number that moves
    /// while one enormous directory is being cleared.
    pub fn files_removed(&self) -> u64 {
        self.files_removed.load(Ordering::Relaxed)
    }

    pub fn freed(&self) -> u64 {
        self.freed.load(Ordering::Relaxed)
    }

    /// Files unlinked while another link still held their data.
    fn links_kept(&self) -> u64 {
        self.links_kept.load(Ordering::Relaxed)
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    pub fn current(&self) -> PathBuf {
        self.current
            .lock()
            .map(|path| path.clone())
            .unwrap_or_default()
    }

    fn start_item(&self, path: &Path) {
        if let Ok(mut current) = self.current.lock() {
            current.clear();
            current.push(path);
        }
    }

    fn finish_item(&self) {
        self.items_done.fetch_add(1, Ordering::Relaxed);
    }

    /// On Unix, bytes are credited as each file is unlinked rather than when a
    /// whole item finishes. Other platforms use the standard library's opaque
    /// protected traversal and publish the final count when it returns.
    fn file_removed(&self, bytes: u64) {
        self.files_removed.fetch_add(1, Ordering::Relaxed);
        self.freed.fetch_add(bytes, Ordering::Relaxed);
    }

    #[cfg(unix)]
    fn link_kept(&self) {
        self.links_kept.fetch_add(1, Ordering::Relaxed);
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }
}

/// Remove a path. Symlinks are unlinked, never followed.
///
/// Unlike [`delete_all`], nothing here compares against a scan: the identities
/// it guards with are read on entry, so they only catch a swap between that
/// read and the removal. [`check`] still refuses a mount point. `progress`
/// counts what was freed, on Unix including by a removal that then fails.
pub fn delete(path: &Path, progress: &DeleteProgress) -> std::io::Result<()> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to delete a filesystem root",
        )
    })?;
    let root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    let path = root.join(name);
    check(&path, &root).map_err(|reason| {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, reason.to_string())
    })?;
    let expected_root_identity = identity_of(&root)?;
    let expected_target_identity = identity_of(&path)?;
    delete_watched(
        &path,
        &root,
        ExpectedIdentities {
            root: expected_root_identity,
            target: expected_target_identity,
        },
        progress,
        &Cancel::new(),
    )
}

/// `(device, inode)` of `path` itself, not of what a symlink there points at.
/// `None` where the platform has no inode to compare.
fn identity_of(path: &Path) -> std::io::Result<Option<(u64, u64)>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Some((meta.dev(), meta.ino())))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// Remove a path, counting entries as they go and stopping if asked.
///
/// Unix walks descriptor-relative and reports each unlink. Other platforms use
/// the standard library's protected recursive deletion and report afterward.
fn delete_watched(
    path: &Path,
    root: &Path,
    expected: ExpectedIdentities,
    progress: &DeleteProgress,
    cancel: &Cancel,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        raise_open_file_limit();
        unix_delete::delete(path, root, expected, progress, cancel)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, expected);
        delete_watched_portable(path, progress, cancel)
    }
}

/// The descriptor walk keeps a few descriptors open per directory level, so
/// macOS's default soft limit of 256 stops it about 80 levels down. Raise the
/// soft limit to the hard one, once per process.
#[cfg(unix)]
fn raise_open_file_limit() {
    static RAISED: std::sync::Once = std::sync::Once::new();
    RAISED.call_once(|| {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` is valid writable storage for one `rlimit`.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
            return;
        }
        // macOS refuses a soft limit above `OPEN_MAX` even when the hard limit
        // is unlimited.
        #[cfg(target_vendor = "apple")]
        let wanted = limit.rlim_max.min(10_240);
        #[cfg(not(target_vendor = "apple"))]
        let wanted = limit.rlim_max;
        if wanted > limit.rlim_cur {
            limit.rlim_cur = wanted;
            // SAFETY: `limit` is a valid `rlimit`; failing leaves the old limit.
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
        }
    });
}

#[cfg(unix)]
mod unix_delete {
    use super::*;
    use rustix::fd::{AsFd, OwnedFd};
    use rustix::fs::{
        AtFlags, Dir, FileType, Mode, OFlags, Stat, fchmod, fstat, openat, statat, unlinkat,
    };
    use rustix::path::Arg;
    use std::ffi::{CString, OsStr, OsString};
    use std::io;
    use std::path::Component;

    const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    /// `(device, inode)` identity of a stat result. `st_dev` is `i32` on
    /// macOS but already `u64` on Linux and NetBSD, so the cast is a no-op
    /// there rather than a real conversion.
    #[allow(clippy::unnecessary_cast)]
    fn identity(stat: &Stat) -> (u64, u64) {
        (stat.st_dev as u64, stat.st_ino)
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    fn traversal_directory_flags() -> OFlags {
        OFlags::PATH
            .union(OFlags::DIRECTORY)
            .union(OFlags::NOFOLLOW)
            .union(OFlags::CLOEXEC)
    }

    #[cfg(any(target_os = "freebsd", target_os = "netbsd", target_vendor = "apple"))]
    fn traversal_directory_flags() -> OFlags {
        OFlags::from_bits_retain(libc::O_SEARCH as u32)
            .union(OFlags::DIRECTORY)
            .union(OFlags::NOFOLLOW)
            .union(OFlags::CLOEXEC)
    }

    #[cfg(not(any(
        target_os = "android",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "netbsd",
        target_vendor = "apple"
    )))]
    fn traversal_directory_flags() -> OFlags {
        DIRECTORY_FLAGS
    }

    struct Boundary {
        device: u64,
        #[cfg(target_os = "linux")]
        mount: LinuxMount,
        #[cfg(target_vendor = "apple")]
        mount: MountIdentity,
        /// `/proc/self/mountinfo`, read at most once per `Boundary` (which
        /// lives for one deletion) rather than once per directory, for
        /// kernels before 3.15, whose fdinfo has no `mnt_id`.
        #[cfg(target_os = "linux")]
        mountinfo: std::cell::RefCell<Option<std::collections::HashSet<PathBuf>>>,
    }

    /// The mount an open directory lives on (`f_mntonname`). On macOS the
    /// sealed system volume and the Data volume share a device, so the name
    /// is what tells them apart.
    #[cfg(target_vendor = "apple")]
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct MountIdentity(PathBuf);

    #[cfg(target_os = "linux")]
    #[derive(PartialEq, Eq)]
    enum LinuxMount {
        Id(u64),
        MountPoint(PathBuf),
    }

    /// Entries removed between two checks that the target and its ancestors
    /// are still where the scan saw them. A check stats every ancestor, so
    /// making it before each unlink slowed a large deletion by half; this
    /// bounds what a move can still let go to entries inside the selection.
    /// Every directory is checked again right before it is removed.
    pub(super) const ANCHOR_CHECK_INTERVAL: u64 = 64;

    struct Context<'a> {
        progress: &'a DeleteProgress,
        cancel: &'a Cancel,
        boundary: Boundary,
        anchors: Vec<Anchor>,
        /// `files_removed` at which the next periodic anchor check is due.
        next_anchor_check: u64,
        #[cfg(test)]
        test_hooks: TestHooks,
    }

    struct Anchor {
        parent: OwnedFd,
        name: CString,
        identity: (u64, u64),
    }

    #[derive(Default)]
    struct Guards {
        expected_root_identity: Option<(u64, u64)>,
        expected_target_identity: Option<(u64, u64)>,
        #[cfg(test)]
        test_hooks: TestHooks,
    }

    #[cfg(test)]
    #[derive(Default)]
    struct TestHooks {
        move_target: Option<MoveTarget>,
        pause: Option<Pause>,
        failure_after_entries: Option<u64>,
        /// Treated as a mount crossing wherever the walk meets it.
        blocked_identity: Option<(u64, u64)>,
        /// Created in the first directory a pass finishes listing, standing
        /// in for an entry the listing skipped.
        unlisted_entry: Option<CString>,
    }

    #[cfg(test)]
    thread_local! {
        /// Makes each deletion on this thread fail once this many entries
        /// are gone.
        pub(super) static FAILURE_AFTER_ENTRIES: std::cell::Cell<Option<u64>> =
            const { std::cell::Cell::new(None) };
    }

    #[cfg(test)]
    struct MoveTarget {
        after_entries: u64,
        from: PathBuf,
        to: PathBuf,
    }

    #[cfg(test)]
    struct Pause {
        after_entries: u64,
        reached: std::sync::mpsc::Sender<()>,
        resume: Arc<std::sync::Barrier>,
    }

    pub(super) fn delete(
        path: &Path,
        root: &Path,
        expected: ExpectedIdentities,
        progress: &DeleteProgress,
        cancel: &Cancel,
    ) -> io::Result<()> {
        delete_impl(
            path,
            root,
            Guards {
                expected_root_identity: expected.root,
                expected_target_identity: expected.target,
                #[cfg(test)]
                test_hooks: TestHooks {
                    failure_after_entries: FAILURE_AFTER_ENTRIES.get(),
                    ..TestHooks::default()
                },
            },
            progress,
            cancel,
        )
    }

    fn delete_impl(
        path: &Path,
        root: &Path,
        guards: Guards,
        progress: &DeleteProgress,
        cancel: &Cancel,
    ) -> io::Result<()> {
        let (parent, name, anchors) = open_parent(path, root, guards.expected_root_identity)?;
        match open_directory(&parent, &name) {
            Ok(directory) => {
                let stat = fstat(&directory)?;
                ensure_identity(&stat, guards.expected_target_identity)?;
                // The boundary is drawn at the target itself, not the scan
                // root: everything below here must stay on the target's own
                // mount, but reaching the target was already allowed to
                // cross mounts in `open_parent`.
                let boundary = Boundary::new(&directory, &stat)?;

                let mut ctx = Context {
                    progress,
                    cancel,
                    boundary,
                    anchors,
                    next_anchor_check: 0,
                    #[cfg(test)]
                    test_hooks: guards.test_hooks,
                };
                preflight(&directory, &ctx)?;
                remove_directory(&parent, &name, directory, stat, &mut ctx)
            }
            Err(error) if is_not_directory(error.raw_os_error()) => remove_leaf(
                &parent,
                &name,
                &anchors,
                guards.expected_target_identity,
                progress,
            ),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub(super) fn delete_with_blocked_path(
        path: &Path,
        root: &Path,
        blocked: &Path,
    ) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(blocked)?;
        let target_meta = std::fs::symlink_metadata(path)?;
        delete_impl(
            path,
            root,
            Guards {
                expected_target_identity: Some((target_meta.dev(), target_meta.ino())),
                test_hooks: TestHooks {
                    blocked_identity: Some((meta.dev(), meta.ino())),
                    ..TestHooks::default()
                },
                ..Guards::default()
            },
            &DeleteProgress::default(),
            &Cancel::new(),
        )
    }

    #[cfg(test)]
    pub(super) fn delete_with_move_after(
        path: &Path,
        root: &Path,
        path_to_move: &Path,
        destination: PathBuf,
        after_entries: u64,
    ) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let target_meta = std::fs::symlink_metadata(path)?;
        delete_impl(
            path,
            root,
            Guards {
                expected_target_identity: Some((target_meta.dev(), target_meta.ino())),
                test_hooks: TestHooks {
                    move_target: Some(MoveTarget {
                        after_entries,
                        from: path_to_move.to_path_buf(),
                        to: destination,
                    }),
                    ..TestHooks::default()
                },
                ..Guards::default()
            },
            &DeleteProgress::default(),
            &Cancel::new(),
        )
    }

    #[cfg(test)]
    pub(super) fn delete_with_an_unlisted_entry(
        path: &Path,
        root: &Path,
        name: &str,
    ) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let target_meta = std::fs::symlink_metadata(path)?;
        delete_impl(
            path,
            root,
            Guards {
                expected_target_identity: Some((target_meta.dev(), target_meta.ino())),
                test_hooks: TestHooks {
                    unlisted_entry: Some(CString::new(name)?),
                    ..TestHooks::default()
                },
                ..Guards::default()
            },
            &DeleteProgress::default(),
            &Cancel::new(),
        )
    }

    #[cfg(test)]
    pub(super) fn delete_with_pause(
        path: PathBuf,
        root: PathBuf,
        progress: Arc<DeleteProgress>,
        reached: std::sync::mpsc::Sender<()>,
        resume: Arc<std::sync::Barrier>,
    ) -> JoinHandle<io::Result<()>> {
        std::thread::spawn(move || {
            use std::os::unix::fs::MetadataExt;
            let target_meta = std::fs::symlink_metadata(&path)?;
            delete_impl(
                &path,
                &root,
                Guards {
                    expected_target_identity: Some((target_meta.dev(), target_meta.ino())),
                    test_hooks: TestHooks {
                        pause: Some(Pause {
                            after_entries: 1,
                            reached,
                            resume,
                        }),
                        ..TestHooks::default()
                    },
                    ..Guards::default()
                },
                &progress,
                &Cancel::new(),
            )
        })
    }

    fn open_parent(
        path: &Path,
        root: &Path,
        expected_root_identity: Option<(u64, u64)>,
    ) -> io::Result<(OwnedFd, OsString, Vec<Anchor>)> {
        let relative = path.strip_prefix(root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path is outside the scan root",
            )
        })?;
        let name = relative.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to delete a filesystem root",
            )
        })?;
        let mut directory = openat(
            rustix::fs::CWD,
            if root.is_absolute() { "/" } else { "." },
            traversal_directory_flags(),
            Mode::empty(),
        )?;
        let mut anchors = Vec::new();

        descend(&mut directory, root, &mut anchors)?;

        let root_stat = fstat(&directory)?;
        if let Some(expected) = expected_root_identity
            && identity(&root_stat) != expected
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "scan root changed since it was scanned",
            ));
        }

        // Walking from the scan root down to the target's parent may cross
        // mount boundaries freely: a target nested under its own mount (say
        // `/home` under a `/` scan) is reached the same way a user's shell
        // would reach it. The boundary that matters is drawn once the target
        // itself is open, below, and never loosened from there on.
        descend(
            &mut directory,
            relative.parent().unwrap_or_else(|| Path::new("")),
            &mut anchors,
        )?;
        Ok((directory, name.to_os_string(), anchors))
    }

    /// Walk `directory` down `path` without following symlinks, anchoring each
    /// step so a later swap of any of them is noticed.
    fn descend(directory: &mut OwnedFd, path: &Path, anchors: &mut Vec<Anchor>) -> io::Result<()> {
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(component) => {
                    let child = openat(
                        &*directory,
                        component,
                        traversal_directory_flags(),
                        Mode::empty(),
                    )?;
                    let stat = fstat(&child)?;
                    anchors.push(new_anchor(directory, component, identity(&stat))?);
                    *directory = child;
                }
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "deletion path must not contain parent components",
                    ));
                }
            }
        }
        Ok(())
    }

    fn open_directory<P: rustix::path::Arg>(parent: &OwnedFd, name: P) -> io::Result<OwnedFd> {
        openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(Into::into)
    }

    fn preflight(directory: &OwnedFd, ctx: &Context<'_>) -> io::Result<()> {
        for child in Dir::read_from(directory)? {
            if ctx.cancel.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
            }
            let child = child?;
            let name = child.file_name();
            // Only a directory can hold a mount; skip what the listing
            // already knows is not one.
            if is_dot(name) || !matches!(child.file_type(), FileType::Directory | FileType::Unknown)
            {
                continue;
            }
            match open_directory(directory, name) {
                Ok(child_dir) => {
                    let stat = fstat(&child_dir)?;
                    if ctx.crossed(&child_dir, &stat)? {
                        return Err(mount_error());
                    }
                    preflight(&child_dir, ctx)?;
                }
                Err(error) if is_not_directory(error.raw_os_error()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    impl Context<'_> {
        fn push_anchor<P: Arg>(
            &mut self,
            parent: &OwnedFd,
            name: P,
            identity: (u64, u64),
        ) -> io::Result<()> {
            self.anchors.push(new_anchor(parent, name, identity)?);
            Ok(())
        }

        fn ensure_anchors(&self) -> io::Result<()> {
            ensure_anchors(&self.anchors)
        }

        /// [`ensure_anchors`](Self::ensure_anchors) once every
        /// [`ANCHOR_CHECK_INTERVAL`] removals, starting with the first.
        fn ensure_anchors_periodically(&mut self) -> io::Result<()> {
            let removed = self.progress.files_removed();
            if removed < self.next_anchor_check {
                return Ok(());
            }
            self.next_anchor_check = removed + ANCHOR_CHECK_INTERVAL;
            self.ensure_anchors()
        }

        fn crossed(&self, fd: impl AsFd, stat: &Stat) -> io::Result<bool> {
            #[cfg(test)]
            if self.test_hooks.blocked_identity == Some(identity(stat)) {
                return Ok(true);
            }
            self.boundary.crossed(fd, stat)
        }

        fn record_removed(&mut self, bytes: u64) {
            self.progress.file_removed(bytes);
            #[cfg(test)]
            if self
                .test_hooks
                .pause
                .as_ref()
                .is_some_and(|pause| self.progress.files_removed() >= pause.after_entries)
            {
                let pause = self.test_hooks.pause.take().unwrap();
                let _ = pause.reached.send(());
                pause.resume.wait();
            }
        }

        #[cfg(test)]
        fn move_target_if_requested(&mut self) -> io::Result<()> {
            if self
                .test_hooks
                .move_target
                .as_ref()
                .is_some_and(|hook| self.progress.files_removed() >= hook.after_entries)
            {
                let hook = self.test_hooks.move_target.take().unwrap();
                std::fs::rename(hook.from, hook.to)?;
            }
            Ok(())
        }

        #[cfg(test)]
        fn add_unlisted_entry_if_requested(&mut self, directory: &OwnedFd) -> io::Result<()> {
            if let Some(name) = self.test_hooks.unlisted_entry.take() {
                openat(
                    directory,
                    name.as_c_str(),
                    OFlags::CREATE | OFlags::WRONLY | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )?;
            }
            Ok(())
        }
    }

    fn new_anchor<P: Arg>(parent: &OwnedFd, name: P, identity: (u64, u64)) -> io::Result<Anchor> {
        let parent = rustix::io::dup(parent)?;
        let name = name.into_with_c_str(|name| Ok(name.to_owned()))?;
        Ok(Anchor {
            parent,
            name,
            identity,
        })
    }

    fn ensure_anchors(anchors: &[Anchor]) -> io::Result<()> {
        for anchor in anchors {
            let stat = statat(
                &anchor.parent,
                anchor.name.as_c_str(),
                AtFlags::SYMLINK_NOFOLLOW,
            );
            if stat
                .ok()
                .is_none_or(|stat| identity(&stat) != anchor.identity)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "deletion target moved outside its original parent",
                ));
            }
        }
        Ok(())
    }

    fn remove_entry<P: rustix::path::Arg + Copy>(
        parent: &OwnedFd,
        name: P,
        ctx: &mut Context<'_>,
    ) -> io::Result<()> {
        #[cfg(test)]
        ctx.move_target_if_requested()?;
        ctx.ensure_anchors_periodically()?;
        #[cfg(test)]
        if ctx
            .test_hooks
            .failure_after_entries
            .is_some_and(|limit| ctx.progress.files_removed() >= limit)
        {
            return Err(io::Error::other("injected deletion failure"));
        }
        // `unlinkat` answers EISDIR (Linux) or EPERM (BSD, macOS) when the
        // entry turned into a directory between the failed `openat` and the
        // unlink. One retry covers that race; a second EPERM is a genuinely
        // immutable or protected file and must surface, not spin.
        let mut retried = false;
        loop {
            match open_directory(parent, name) {
                Ok(directory) => {
                    let stat = fstat(&directory)?;
                    if ctx.crossed(&directory, &stat)? {
                        return Err(mount_error());
                    }
                    return remove_directory(parent, name, directory, stat, ctx);
                }
                Err(error) if is_not_directory(error.raw_os_error()) => {
                    let stat = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(stat) => stat,
                        Err(rustix::io::Errno::NOENT) => return Ok(()),
                        Err(error) => return Err(error.into()),
                    };
                    match unlinkat(parent, name, AtFlags::empty()) {
                        Ok(()) => {
                            let bytes = freed_by_unlink(&stat, ctx.progress);
                            ctx.record_removed(bytes);
                            return Ok(());
                        }
                        Err(rustix::io::Errno::ISDIR | rustix::io::Errno::PERM) if !retried => {
                            retried = true;
                            continue;
                        }
                        Err(rustix::io::Errno::NOENT) => return Ok(()),
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    fn remove_directory<P: rustix::path::Arg + Copy>(
        parent: &OwnedFd,
        name: P,
        directory: OwnedFd,
        stat: Stat,
        ctx: &mut Context<'_>,
    ) -> io::Result<()> {
        let original_mode = Mode::from_raw_mode(stat.st_mode as _);
        let writable_mode = original_mode | Mode::WUSR;
        // Only a directory the user owns is theirs to chmod; root needs none.
        // SAFETY: `geteuid` has no preconditions.
        let changed_mode =
            writable_mode != original_mode && stat.st_uid == unsafe { libc::geteuid() };
        ctx.push_anchor(parent, name, identity(&stat))?;

        // A directory without owner write cannot be emptied, so add the bit
        // before the one walk rather than retrying after a failure: a retry
        // per read-only ancestor re-walks the subtree at every level, which
        // turns one file that cannot go into work doubling with depth.
        let mut mode_was_changed = false;
        let mut result = Ok(());
        if changed_mode {
            match ctx.ensure_anchors() {
                Err(anchor_error) => result = Err(anchor_error),
                Ok(()) => mode_was_changed = fchmod(&directory, writable_mode).is_ok(),
            }
        }
        if result.is_ok() {
            result = remove_directory_once(parent, name, &directory, &stat, ctx);
        }

        if result.is_err() && mode_was_changed {
            let _ = fchmod(&directory, original_mode);
        }
        ctx.anchors.pop();
        result
    }

    fn remove_directory_once<P: rustix::path::Arg + Copy>(
        parent: &OwnedFd,
        name: P,
        directory: &OwnedFd,
        stat: &Stat,
        ctx: &mut Context<'_>,
    ) -> io::Result<()> {
        // POSIX leaves it unspecified whether a listing still returns every
        // entry while entries are being unlinked under it, so a directory
        // that is not empty after a pass is listed again, for as long as a
        // pass still removes something.
        loop {
            let removed_before = ctx.progress.files_removed();
            for child in Dir::read_from(directory)? {
                if ctx.cancel.is_cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
                }
                let child = child?;
                if !is_dot(child.file_name()) {
                    remove_entry(directory, child.file_name(), ctx)?;
                }
            }
            #[cfg(test)]
            ctx.add_unlisted_entry_if_requested(directory)?;
            ctx.ensure_anchors()?;
            match unlinkat(parent, name, AtFlags::REMOVEDIR) {
                Err(rustix::io::Errno::NOTEMPTY | rustix::io::Errno::EXIST)
                    if ctx.progress.files_removed() > removed_before => {}
                result => {
                    result?;
                    break;
                }
            }
        }
        ctx.record_removed(allocated_stat(stat));
        Ok(())
    }

    fn remove_leaf(
        parent: &OwnedFd,
        name: &OsStr,
        anchors: &[Anchor],
        expected_identity: Option<(u64, u64)>,
        progress: &DeleteProgress,
    ) -> io::Result<()> {
        ensure_anchors(anchors)?;
        let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        // A leaf target has no subtree below it to protect, so there is no
        // boundary left to enforce once its own identity checks out: it was
        // already permitted to cross mounts on the way here.
        ensure_identity(&stat, expected_identity)?;
        unlinkat(parent, name, AtFlags::empty())?;
        progress.file_removed(freed_by_unlink(&stat, progress));
        Ok(())
    }

    fn ensure_identity(stat: &Stat, expected: Option<(u64, u64)>) -> io::Result<()> {
        if expected.is_some_and(|expected| identity(stat) == expected) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "deletion target changed since it was scanned",
            ))
        }
    }

    /// Bytes an unlink of `stat` freed: none while another link remains.
    fn freed_by_unlink(stat: &Stat, progress: &DeleteProgress) -> u64 {
        if stat.st_nlink <= 1 {
            allocated_stat(stat)
        } else {
            progress.link_kept();
            0
        }
    }

    fn allocated_stat(stat: &Stat) -> u64 {
        (stat.st_blocks as u64).saturating_mul(512)
    }

    /// Whether opening an entry with `O_DIRECTORY | O_NOFOLLOW` failed because
    /// it is not a directory. For a symlink POSIX answers ELOOP, FreeBSD
    /// EMLINK and NetBSD EFTYPE.
    fn is_not_directory(raw: Option<i32>) -> bool {
        use rustix::io::Errno;
        let not_a_directory = [
            Errno::NOTDIR,
            Errno::LOOP,
            #[cfg(target_os = "freebsd")]
            Errno::MLINK,
            #[cfg(target_os = "netbsd")]
            Errno::FTYPE,
        ];
        raw.is_some_and(|raw| {
            not_a_directory
                .iter()
                .any(|errno| errno.raw_os_error() == raw)
        })
    }

    fn is_dot(name: &std::ffi::CStr) -> bool {
        matches!(name.to_bytes(), b"." | b"..")
    }

    fn mount_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to cross a mount point",
        )
    }

    impl Boundary {
        fn new(fd: impl AsFd, stat: &Stat) -> io::Result<Self> {
            let _ = &fd;
            #[cfg(target_os = "linux")]
            let mountinfo = std::cell::RefCell::new(None);
            Ok(Self {
                device: identity(stat).0,
                #[cfg(target_os = "linux")]
                mount: linux_mount(fd, &mountinfo)?,
                #[cfg(target_vendor = "apple")]
                mount: mount_identity(fd)?,
                #[cfg(target_os = "linux")]
                mountinfo,
            })
        }

        fn crossed(&self, fd: impl AsFd, stat: &Stat) -> io::Result<bool> {
            if identity(stat).0 != self.device {
                return Ok(true);
            }
            #[cfg(target_os = "linux")]
            {
                Ok(linux_mount(fd, &self.mountinfo)? != self.mount)
            }
            #[cfg(target_vendor = "apple")]
            {
                Ok(mount_identity(fd)? != self.mount)
            }
            #[cfg(all(not(target_os = "linux"), not(target_vendor = "apple")))]
            {
                let _ = fd;
                Ok(false)
            }
        }
    }

    #[cfg(target_vendor = "apple")]
    fn mount_identity(fd: impl AsFd) -> io::Result<MountIdentity> {
        use std::os::fd::AsRawFd;
        super::apple_mount_point_of_fd(fd.as_fd().as_raw_fd()).map(MountIdentity)
    }

    #[cfg(target_os = "linux")]
    fn linux_mount(
        fd: impl AsFd,
        mountinfo: &std::cell::RefCell<Option<std::collections::HashSet<PathBuf>>>,
    ) -> io::Result<LinuxMount> {
        use rustix::fs::{StatxFlags, statx};
        if let Ok(stat) = statx(&fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
            && stat.stx_mask & StatxFlags::MNT_ID.bits() != 0
        {
            return Ok(LinuxMount::Id(stat.stx_mnt_id));
        }

        if let Some(id) = linux_mount_from_fdinfo(&fd)? {
            return Ok(LinuxMount::Id(id));
        }
        linux_mount_from_mountinfo(fd, mountinfo)
    }

    /// Kernels without `STATX_MNT_ID` (before 5.8) still name the same mount
    /// ID in `/proc/self/fdinfo` (since 3.15), and reading it needs no path,
    /// so trees deeper than `PATH_MAX` still resolve. `None` only when the
    /// kernel has no such line or no fdinfo at all; any other read error is
    /// returned, since a path-based answer would not compare with an ID.
    #[cfg(target_os = "linux")]
    fn linux_mount_from_fdinfo(fd: impl AsFd) -> io::Result<Option<u64>> {
        use std::os::fd::AsRawFd;
        match std::fs::read(format!("/proc/self/fdinfo/{}", fd.as_fd().as_raw_fd())) {
            Ok(fdinfo) => Ok(super::mount_id_from_fdinfo(&fdinfo)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Last resort, for kernels older than 3.15: resolve the descriptor's
    /// path and match it against `/proc/self/mountinfo`, read once per
    /// `mountinfo` cache (one per `Boundary`, i.e. one per deletion).
    #[cfg(target_os = "linux")]
    fn linux_mount_from_mountinfo(
        fd: impl AsFd,
        mountinfo: &std::cell::RefCell<Option<std::collections::HashSet<PathBuf>>>,
    ) -> io::Result<LinuxMount> {
        use std::os::fd::AsRawFd;
        let fd_path = PathBuf::from(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()));
        let path = fd_path.canonicalize()?;
        if mountinfo.borrow().is_none() {
            *mountinfo.borrow_mut() = Some(super::linux_mount_points()?);
        }
        let mounts = mountinfo.borrow();
        let mount =
            super::containing_mount_point(mounts.as_ref().unwrap(), &path).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "could not identify the descriptor's mount point",
                )
            })?;
        Ok(LinuxMount::MountPoint(mount.to_path_buf()))
    }

    #[cfg(all(test, target_os = "linux"))]
    mod linux_tests {
        use super::*;

        fn directory(path: &str) -> OwnedFd {
            openat(rustix::fs::CWD, path, DIRECTORY_FLAGS, Mode::empty()).unwrap()
        }

        #[test]
        fn fdinfo_names_the_same_mount_as_statx() {
            use rustix::fs::{StatxFlags, statx};
            let root = directory("/");
            let proc = directory("/proc");
            let from_fdinfo = linux_mount_from_fdinfo(&root).unwrap().unwrap();

            assert_ne!(
                from_fdinfo,
                linux_mount_from_fdinfo(&proc).unwrap().unwrap()
            );
            if let Ok(stat) = statx(&root, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
                && stat.stx_mask & StatxFlags::MNT_ID.bits() != 0
            {
                assert_eq!(from_fdinfo, stat.stx_mnt_id);
            }
        }

        #[test]
        fn live_mount_ids_distinguish_proc_from_the_root_mount() {
            let root = directory("/");
            let proc = directory("/proc");
            let proc_stat = fstat(&proc).unwrap();
            let mountinfo = std::cell::RefCell::new(None);
            let boundary = Boundary {
                device: identity(&proc_stat).0,
                mount: linux_mount(&root, &mountinfo).unwrap(),
                mountinfo,
            };

            assert!(
                linux_mount(&root, &boundary.mountinfo).unwrap()
                    != linux_mount(&proc, &boundary.mountinfo).unwrap()
            );
            assert!(boundary.crossed(&proc, &proc_stat).unwrap());
        }

        #[test]
        fn mountinfo_fallback_distinguishes_proc_from_the_root_mount() {
            let root = directory("/");
            let proc = directory("/proc");
            let mountinfo = std::cell::RefCell::new(None);

            assert!(
                linux_mount_from_mountinfo(&root, &mountinfo).unwrap()
                    != linux_mount_from_mountinfo(&proc, &mountinfo).unwrap()
            );
        }

        #[test]
        fn the_mountinfo_cache_is_populated_once_and_reused() {
            let root = directory("/");
            let proc = directory("/proc");
            let mountinfo = std::cell::RefCell::new(None);

            assert!(mountinfo.borrow().is_none());
            linux_mount_from_mountinfo(&root, &mountinfo).unwrap();
            assert!(mountinfo.borrow().is_some());

            // A table already read is used as it is rather than read again:
            // holding only `/`, it places even `/proc` on `/`.
            let mountinfo =
                std::cell::RefCell::new(Some(std::collections::HashSet::from([PathBuf::from(
                    "/",
                )])));
            assert!(
                linux_mount_from_mountinfo(&proc, &mountinfo).unwrap()
                    == LinuxMount::MountPoint(PathBuf::from("/"))
            );
        }
    }

    #[cfg(all(test, target_vendor = "apple"))]
    mod apple_tests {
        use super::*;

        #[test]
        fn a_walk_from_the_system_volume_stops_at_the_data_volume() {
            let open =
                |path: &str| openat(rustix::fs::CWD, path, DIRECTORY_FLAGS, Mode::empty()).unwrap();
            let root = open("/");
            let boundary = Boundary::new(&root, &fstat(&root).unwrap()).unwrap();

            // `/Users` is a firmlink into the Data volume: same device as `/`,
            // different mount. `/System` is on the sealed volume itself.
            let users = open("/Users");
            let system = open("/System");
            assert!(boundary.crossed(&users, &fstat(&users).unwrap()).unwrap());
            assert!(!boundary.crossed(&system, &fstat(&system).unwrap()).unwrap());
        }

        #[test]
        fn the_data_volume_and_devfs_are_refused_as_mount_points() {
            assert_eq!(
                check(
                    Path::new("/System/Volumes/Data"),
                    Path::new("/System/Volumes")
                ),
                Err(Refusal::MountPoint)
            );
            assert_eq!(
                check(Path::new("/dev"), Path::new("/")),
                Err(Refusal::MountPoint)
            );
            // A firmlinked directory is not a mount point of its own.
            assert!(!is_mount_point(Path::new("/Users")));
        }
    }
}

#[cfg(not(unix))]
fn force_remove_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            if let Ok(meta) = std::fs::symlink_metadata(path) {
                let original = meta.permissions();
                if original.readonly() {
                    let mut writable = original.clone();
                    // Windows only: clears the read-only attribute.
                    #[allow(clippy::permissions_set_readonly_false)]
                    writable.set_readonly(false);
                    std::fs::set_permissions(path, writable)?;
                    let result = std::fs::remove_file(path);
                    if result.is_err() {
                        let _ = std::fs::set_permissions(path, original);
                    }
                    return result;
                }
            }
            std::fs::remove_file(path)
        }
        res => res,
    }
}

#[cfg(not(unix))]
fn delete_watched_portable(
    path: &Path,
    progress: &DeleteProgress,
    cancel: &Cancel,
) -> std::io::Result<()> {
    if cancel.is_cancelled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "stopped",
        ));
    }
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let mut entries = Vec::new();
        collect_portable_entries(path, &mut entries, cancel)?;
        if cancel.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "stopped",
            ));
        }
        std::fs::remove_dir_all(path)?;
        for bytes in entries {
            progress.file_removed(bytes);
        }
    } else {
        force_remove_file(path)?;
        progress.file_removed(allocated_of(&meta));
    }
    Ok(())
}

#[cfg(not(unix))]
fn collect_portable_entries(
    path: &Path,
    entries: &mut Vec<u64>,
    cancel: &Cancel,
) -> std::io::Result<()> {
    if cancel.is_cancelled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "stopped",
        ));
    }
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        for child in std::fs::read_dir(path)? {
            collect_portable_entries(&child?.path(), entries, cancel)?;
        }
    }
    entries.push(allocated_of(&meta));
    Ok(())
}

/// What the entry actually occupies, matching how duzo counts sizes
/// everywhere else.
#[cfg(all(unix, test))]
fn allocated_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn allocated_of(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn listed_mount_point(mount_points: &std::collections::HashSet<PathBuf>, path: &Path) -> bool {
    path.canonicalize()
        .map_or(true, |path| mount_points.contains(&path))
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn containing_mount_point<'a>(
    mount_points: &'a std::collections::HashSet<PathBuf>,
    path: &Path,
) -> Option<&'a Path> {
    mount_points
        .iter()
        .filter(|mount| path.starts_with(mount))
        .max_by_key(|mount| mount.components().count())
        .map(PathBuf::as_path)
}

/// The `mnt_id:` line of a `/proc/<pid>/fdinfo/<fd>` file.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn mount_id_from_fdinfo(fdinfo: &[u8]) -> Option<u64> {
    std::str::from_utf8(fdinfo)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|id| id.trim().parse().ok())
}

#[cfg(target_os = "linux")]
fn linux_mount_points() -> std::io::Result<std::collections::HashSet<PathBuf>> {
    let mountinfo = std::fs::read("/proc/self/mountinfo")?;
    Ok(mount_points_from(&mountinfo))
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn mount_points_from(mountinfo: &[u8]) -> std::collections::HashSet<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    mountinfo
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let field = line
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|field| !field.is_empty())
                .nth(4)?;
            let mut decoded = Vec::with_capacity(field.len());
            let mut at = 0;
            while at < field.len() {
                if field[at] == b'\\'
                    && at + 3 < field.len()
                    && field[at + 1..=at + 3]
                        .iter()
                        .all(|byte| matches!(byte, b'0'..=b'7'))
                {
                    decoded.push(
                        (field[at + 1] - b'0') * 64
                            + (field[at + 2] - b'0') * 8
                            + (field[at + 3] - b'0'),
                    );
                    at += 4;
                } else {
                    decoded.push(field[at]);
                    at += 1;
                }
            }
            Some(PathBuf::from(OsString::from_vec(decoded)))
        })
        .collect()
}

/// Whether `path` is on a different device from its parent; `None` when
/// either cannot be read.
#[cfg(unix)]
fn on_another_device_than_its_parent(path: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;

    let parent = path.parent()?;
    let here = std::fs::symlink_metadata(path).ok()?;
    let above = std::fs::symlink_metadata(parent).ok()?;
    Some(here.dev() != above.dev())
}

#[cfg(target_os = "linux")]
fn is_mount_point(path: &Path) -> bool {
    linux_mount_points().map_or(true, |mounts| is_linux_mount_point(&mounts, path))
}

/// A directory listed as a mount in `mount_points` (a bind mount can share
/// its parent's device), or on another device than its parent, as an
/// unmounted btrfs subvolume is.
#[cfg(target_os = "linux")]
fn is_linux_mount_point(mount_points: &std::collections::HashSet<PathBuf>, path: &Path) -> bool {
    // If it cannot be established, assume the more cautious answer.
    on_another_device_than_its_parent(path).unwrap_or(true)
        || listed_mount_point(mount_points, path)
}

/// A directory whose device differs from its parent's is a mount point. On
/// macOS the Data volume shares the system volume's device, so a directory
/// that is its own mount's root counts too.
#[cfg(all(unix, not(target_os = "linux")))]
fn is_mount_point(path: &Path) -> bool {
    match on_another_device_than_its_parent(path) {
        Some(true) => true,
        #[cfg(target_vendor = "apple")]
        Some(false) => apple_is_mount_root(path),
        #[cfg(not(target_vendor = "apple"))]
        Some(false) => false,
        // If it cannot be established, assume the more cautious answer.
        None => true,
    }
}

/// True when `path` is the root directory of a mount, by the kernel's own
/// answer (`ATTR_DIR_MOUNTSTATUS`). Asking by path needs no read access, so
/// an unreadable folder is reported for what it is rather than as a mount;
/// a lookup that fails still counts as one.
#[cfg(target_vendor = "apple")]
fn apple_is_mount_root(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    #[repr(C)]
    struct Reply {
        length: u32,
        mount_status: u32,
    }

    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: 0,
        dirattr: libc::ATTR_DIR_MOUNTSTATUS,
        fileattr: 0,
        forkattr: 0,
    };
    let mut reply = Reply {
        length: 0,
        mount_status: 0,
    };
    // SAFETY: valid nul-terminated path; `request` and `reply` are live and
    // `reply` is exactly the size passed, matching the one requested field.
    let status = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            (&mut request as *mut libc::attrlist).cast(),
            (&mut reply as *mut Reply).cast(),
            std::mem::size_of::<Reply>(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    status != 0 || reply.mount_status & libc::DIR_MNTSTATUS_MNTPOINT != 0
}

/// `f_mntonname` of the filesystem holding `fd`.
#[cfg(target_vendor = "apple")]
fn apple_mount_point_of_fd(fd: std::os::fd::RawFd) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut raw: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is a live descriptor, and `raw` points to valid writable storage.
    if unsafe { libc::fstatfs(fd, &mut raw) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `f_mntonname` is the nul-terminated mount path returned by `fstatfs`.
    let name = unsafe { std::ffi::CStr::from_ptr(raw.f_mntonname.as_ptr()) };
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes())))
}

#[cfg(not(unix))]
fn is_mount_point(_path: &Path) -> bool {
    false
}

/// What happened to one target.
#[derive(Clone, Debug)]
pub enum Outcome {
    Deleted {
        path: PathBuf,
        freed: u64,
        tree_needs_rescan: bool,
        /// Whether it unlinked a file another link still held.
        links_survived: bool,
    },
    Refused {
        path: PathBuf,
        reason: Refusal,
    },
    Failed {
        path: PathBuf,
        error: String,
        removed_bytes: u64,
        removed_entries: u64,
        tree_may_have_changed: bool,
        /// Whether it unlinked a file another link still held.
        links_survived: bool,
    },
}

impl Outcome {
    pub fn path(&self) -> &Path {
        match self {
            Outcome::Deleted { path, .. }
            | Outcome::Refused { path, .. }
            | Outcome::Failed { path, .. } => path,
        }
    }

    pub fn succeeded(&self) -> bool {
        matches!(self, Outcome::Deleted { .. })
    }

    pub fn changed_tree(&self) -> bool {
        matches!(
            self,
            Outcome::Deleted { .. }
                | Outcome::Failed {
                    tree_may_have_changed: true,
                    ..
                }
        )
    }

    /// Whether this deletion unlinked a file that another link still held at
    /// the time. That link can be anywhere in the tree, including inside what
    /// was deleted.
    pub fn links_survived(&self) -> bool {
        matches!(
            self,
            Outcome::Deleted {
                links_survived: true,
                ..
            } | Outcome::Failed {
                links_survived: true,
                ..
            }
        )
    }

    pub fn needs_rescan(&self) -> bool {
        matches!(
            self,
            Outcome::Deleted {
                tree_needs_rescan: true,
                ..
            } | Outcome::Failed {
                tree_may_have_changed: true,
                ..
            }
        )
    }

    /// Short explanation for the status bar or the details panel.
    pub fn detail(&self) -> String {
        match self {
            Outcome::Deleted { .. } => "deleted".into(),
            Outcome::Refused { reason, .. } => reason.to_string(),
            Outcome::Failed {
                error,
                removed_entries: 0,
                ..
            } => error.clone(),
            Outcome::Failed {
                error,
                removed_entries,
                ..
            } => match removed_entries {
                1 => format!("{error} after removing 1 entry"),
                _ => format!("{error} after removing {removed_entries} entries"),
            },
        }
    }
}

/// Delete every target that passes its check.
///
/// Each path is re-checked immediately before it is removed, not when the list
/// was drawn.
pub fn delete_all(
    targets: &[Target],
    root: &Path,
    expected_root_identity: Option<(u64, u64)>,
    progress: &DeleteProgress,
    cancel: &Cancel,
) -> Vec<Outcome> {
    #[cfg(unix)]
    let unusable_root = if expected_root_identity.is_none() {
        Some("scan root identity unavailable")
    } else if !root.is_absolute() {
        Some("scan root must be absolute")
    } else {
        None
    };
    #[cfg(unix)]
    if let Some(error) = unusable_root {
        let outcomes = targets
            .iter()
            .map(|target| Outcome::Failed {
                path: target.path.clone(),
                error: error.to_string(),
                removed_bytes: 0,
                removed_entries: 0,
                tree_may_have_changed: false,
                links_survived: false,
            })
            .collect();
        progress.finish();
        return outcomes;
    }

    let mut outcomes = Vec::with_capacity(targets.len());
    for target in targets {
        if cancel.is_cancelled() {
            outcomes.push(Outcome::Refused {
                path: target.path.clone(),
                reason: Refusal::Stopped,
            });
            continue;
        }

        progress.start_item(&target.path);
        let files_before = progress.files_removed();
        let links_before = progress.links_kept();
        let bytes_before = progress.freed();
        let outcome = match check(&target.path, root) {
            Err(reason) => Outcome::Refused {
                path: target.path.clone(),
                reason,
            },
            Ok(()) => match delete_watched(
                &target.path,
                root,
                ExpectedIdentities {
                    root: expected_root_identity,
                    target: target.filesystem_identity,
                },
                progress,
                cancel,
            ) {
                Ok(()) => {
                    progress.finish_item();
                    let freed = progress.freed().saturating_sub(bytes_before);
                    Outcome::Deleted {
                        path: target.path.clone(),
                        freed,
                        tree_needs_rescan: freed != target.allocated_size,
                        links_survived: progress.links_kept() > links_before,
                    }
                }
                // Nothing under it was touched yet: it was never reached, not
                // stopped part-way. (Elsewhere than Unix a stop only lands
                // before `remove_dir_all` starts.)
                Err(error)
                    if error.kind() == std::io::ErrorKind::Interrupted
                        && progress.files_removed() == files_before =>
                {
                    Outcome::Refused {
                        path: target.path.clone(),
                        reason: Refusal::Stopped,
                    }
                }
                Err(error) => Outcome::Failed {
                    path: target.path.clone(),
                    error: if error.kind() == std::io::ErrorKind::Interrupted {
                        "stopped part-way through".to_string()
                    } else {
                        error.to_string()
                    },
                    removed_bytes: progress.freed().saturating_sub(bytes_before),
                    removed_entries: progress.files_removed().saturating_sub(files_before),
                    tree_may_have_changed: progress.files_removed() > files_before
                        || cfg!(not(unix)) && target.is_dir,
                    links_survived: progress.links_kept() > links_before,
                },
            },
        };
        outcomes.push(outcome);
    }

    progress.finish();
    outcomes
}

/// Run a deletion on its own thread so the interface stays alive.
pub fn delete_in_background(
    targets: Vec<Target>,
    root: PathBuf,
    expected_root_identity: Option<(u64, u64)>,
) -> (Arc<DeleteProgress>, Cancel, JoinHandle<Vec<Outcome>>) {
    let progress = Arc::new(DeleteProgress::new(targets.len()));
    let cancel = Cancel::new();

    let handle = {
        let progress = Arc::clone(&progress);
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            delete_all(&targets, &root, expected_root_identity, &progress, &cancel)
        })
    };

    (progress, cancel, handle)
}

/// Total actually freed.
pub fn freed(outcomes: &[Outcome]) -> u64 {
    outcomes
        .iter()
        .map(|outcome| match outcome {
            Outcome::Deleted { freed, .. } => *freed,
            Outcome::Failed { removed_bytes, .. } => *removed_bytes,
            Outcome::Refused { .. } => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use unix_delete::ANCHOR_CHECK_INTERVAL;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("duzo-delete-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn file(&self, name: &str, bytes: usize) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, vec![b'x'; bytes]).unwrap();
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run(targets: &[Target], root: &Path) -> Vec<Outcome> {
        let targets: Vec<Target> = targets
            .iter()
            .cloned()
            .map(|mut target| {
                target.filesystem_identity = filesystem_identity(&target.path);
                target
            })
            .collect();
        delete_all(
            &targets,
            root,
            filesystem_identity(root),
            &DeleteProgress::new(targets.len()),
            &Cancel::new(),
        )
    }

    #[cfg(unix)]
    fn filesystem_identity(path: &Path) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(path).ok()?;
        Some((meta.dev(), meta.ino()))
    }

    #[cfg(not(unix))]
    fn filesystem_identity(_path: &Path) -> Option<(u64, u64)> {
        None
    }

    #[test]
    fn a_path_inside_the_scan_may_be_removed() {
        let tree = TempTree::new("ok");
        let victim = tree.dir("victim");
        assert_eq!(check(&victim, &tree.0), Ok(()));
    }

    #[test]
    fn a_cwd_relative_file_may_be_removed() {
        let name = format!("duzo-relative-delete-{}", std::process::id());
        fs::write(&name, b"temporary").unwrap();

        let result = delete(Path::new(&name), &DeleteProgress::default());
        let removed = !Path::new(&name).exists();
        let _ = fs::remove_file(&name);

        assert!(result.is_ok(), "{result:?}");
        assert!(removed);
    }

    #[cfg(unix)]
    #[test]
    fn execute_only_scan_root_ancestors_still_allow_deletion() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempTree::new("execute-only-ancestor");
        let locked = temp.dir("locked");
        let scan = temp.dir("locked/scan");
        let victim = temp.file("locked/scan/victim.bin", 100);
        let original_permissions = fs::metadata(&locked).unwrap().permissions();
        let mut execute_only = original_permissions.clone();
        execute_only.set_mode(0o111);
        fs::set_permissions(&locked, execute_only).unwrap();

        let outcomes = delete_all(
            &[Target {
                path: victim.clone(),
                size: 100,
                allocated_size: 100,
                filesystem_identity: filesystem_identity(&victim),
                is_dir: false,
            }],
            &scan,
            filesystem_identity(&scan),
            &DeleteProgress::new(1),
            &Cancel::new(),
        );

        fs::set_permissions(&locked, original_permissions).unwrap();
        assert!(matches!(outcomes[0], Outcome::Deleted { .. }));
        assert!(!victim.exists());
    }

    #[test]
    fn the_scan_root_is_never_a_target() {
        let tree = TempTree::new("root");
        assert_eq!(check(&tree.0, &tree.0), Err(Refusal::ScanRoot));
    }

    #[test]
    fn paths_outside_the_scan_are_refused() {
        let tree = TempTree::new("outside");
        let elsewhere = std::env::temp_dir().join("somewhere-else-entirely");
        assert_eq!(check(&elsewhere, &tree.0), Err(Refusal::OutsideScan));
    }

    #[test]
    fn a_filesystem_root_is_refused_outright() {
        assert_eq!(
            check(Path::new("/"), Path::new("/")),
            Err(Refusal::Filesystem)
        );
    }

    #[test]
    fn something_deleted_behind_our_back_is_refused_not_retried() {
        let tree = TempTree::new("gone");
        let ghost = tree.0.join("never-existed");
        assert_eq!(check(&ghost, &tree.0), Err(Refusal::Missing));
    }

    #[test]
    fn deleting_a_directory_takes_its_contents() {
        let tree = TempTree::new("recursive");
        let victim = tree.dir("victim");
        tree.file("victim/inner.txt", 100);

        delete(&victim, &DeleteProgress::default()).unwrap();
        assert!(!victim.exists());
    }

    #[test]
    fn a_symlink_is_unlinked_rather_than_followed() {
        let tree = TempTree::new("symlink");
        let precious = tree.dir("precious");
        tree.file("precious/keep.txt", 10);
        let link = tree.0.join("link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&precious, &link).unwrap();

        delete(&link, &DeleteProgress::default()).unwrap();

        assert!(!link.exists(), "the link should be gone");
        assert!(precious.join("keep.txt").exists(), "its target must not be");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_nested_inside_a_removed_directory_is_unlinked_not_followed() {
        let tree = TempTree::new("nested-symlink");
        let outside = tree.dir("outside");
        let precious = tree.file("outside/precious.bin", 100);
        let victim = tree.dir("victim");
        let blob = tree.file("victim/a.bin", 100);
        let link = victim.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let expected_freed = allocated_of(&fs::symlink_metadata(&victim).unwrap())
            + allocated_of(&fs::symlink_metadata(&blob).unwrap())
            + allocated_of(&fs::symlink_metadata(&link).unwrap());

        let outcomes = run(
            &[Target {
                path: victim.clone(),
                size: 100,
                allocated_size: expected_freed,
                filesystem_identity: None,
                is_dir: true,
            }],
            &tree.0,
        );

        assert!(matches!(outcomes[0], Outcome::Deleted { freed, .. } if freed == expected_freed));
        assert!(
            !victim.exists(),
            "the directory holding the link should be gone"
        );
        assert!(precious.exists(), "the link's target must be untouched");
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn an_immutable_file_fails_the_deletion_instead_of_spinning() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;
        use std::time::Duration;

        // Undo the flag and the read-only modes so the temp tree can go.
        struct Unlock(CString, Vec<PathBuf>);
        impl Drop for Unlock {
            fn drop(&mut self) {
                // SAFETY: the path is a valid nul-terminated string.
                unsafe { libc::chflags(self.0.as_ptr(), 0) };
                for dir in &self.1 {
                    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
                }
            }
        }

        // Read-only directories all the way down, as in Go's module cache:
        // each one needs a chmod before its entries can go, and none of that
        // may turn one stuck file into work that doubles per level.
        let tree = TempTree::new("immutable");
        let victim = tree.dir("victim");
        let mut dirs = vec![victim.clone()];
        for level in 0..20 {
            let next = dirs.last().unwrap().join(format!("d{level}"));
            fs::create_dir(&next).unwrap();
            dirs.push(next);
        }
        let locked = dirs.last().unwrap().join("locked.bin");
        fs::write(&locked, b"locked").unwrap();
        let c_path = CString::new(locked.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is a valid nul-terminated string.
        assert_eq!(
            unsafe { libc::chflags(c_path.as_ptr(), libc::UF_IMMUTABLE) },
            0
        );
        let _unlock = Unlock(c_path, dirs.clone());
        for dir in dirs.iter().rev() {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
        }

        let (done, finished) = mpsc::channel();
        let target = victim.clone();
        std::thread::spawn(move || {
            let _ = done.send(delete(&target, &DeleteProgress::default()));
        });
        let result = finished
            .recv_timeout(Duration::from_secs(10))
            .expect("deletion must finish rather than retry its way down");

        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(locked.exists());
        for dir in &dirs {
            let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o555, "{} keeps its mode", dir.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_tree_deeper_than_path_max_is_deleted() {
        use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};

        // Built descriptor-relative: the deepest absolute path is longer than
        // macOS's 1024-byte PATH_MAX, so nothing path-based may be asked about it.
        let tree = TempTree::new("deep");
        let victim = tree.dir("victim");
        let name = "d".repeat(250);
        let mut directory = openat(CWD, &victim, OFlags::DIRECTORY, Mode::empty()).unwrap();
        for _ in 0..6 {
            mkdirat(&directory, name.as_str(), Mode::RWXU).unwrap();
            directory =
                openat(&directory, name.as_str(), OFlags::DIRECTORY, Mode::empty()).unwrap();
        }
        drop(directory);

        delete(&victim, &DeleteProgress::default()).unwrap();

        assert!(!victim.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_stable_symlink_alias_in_the_parent_path_is_accepted() {
        let tree = TempTree::new("parent-alias");
        let real = tree.dir("real");
        let victim = tree.file("real/victim.bin", 100);
        let alias = tree.0.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        delete(&alias.join("victim.bin"), &DeleteProgress::default()).unwrap();

        assert!(!victim.exists());
        assert!(alias.exists());
    }

    #[test]
    fn deleting_reports_what_went_and_what_did_not() {
        let temp = TempTree::new("outcomes");
        let victim = temp.dir("victim");
        temp.file("victim/blob", 1000);
        let missing = temp.0.join("already-gone");
        let expected_freed = allocated_of(&fs::symlink_metadata(&victim).unwrap())
            + allocated_of(&fs::symlink_metadata(victim.join("blob")).unwrap());

        let targets = vec![
            Target {
                path: victim.clone(),
                size: 3000,
                allocated_size: expected_freed,
                filesystem_identity: None,
                is_dir: true,
            },
            Target {
                path: missing.clone(),
                size: 99,
                allocated_size: 99,
                filesystem_identity: None,
                is_dir: true,
            },
        ];

        let outcomes = run(&targets, &temp.0);

        assert!(matches!(
            outcomes[0],
            Outcome::Deleted { freed, .. } if freed == expected_freed
        ));
        assert!(matches!(
            outcomes[1],
            Outcome::Refused {
                reason: Refusal::Missing,
                ..
            }
        ));
        assert_eq!(freed(&outcomes), expected_freed);
        assert!(!victim.exists());
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn an_unreadable_folder_is_not_mistaken_for_a_mount_point() {
        use std::os::unix::fs::PermissionsExt;

        let tree = TempTree::new("unreadable");
        let locked = tree.dir("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let refusal = check(&locked, &tree.0);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(refusal, Ok(()));
    }

    #[test]
    fn progress_moves_while_a_big_directory_is_being_cleared() {
        let temp = TempTree::new("progress");
        let victim = temp.dir("victim");
        let mut expected = 0;
        for index in 0..25 {
            let file = temp.file(&format!("victim/file-{index}"), 10);
            expected += allocated_of(&fs::symlink_metadata(file).unwrap());
        }
        // Measured last: on some filesystems a directory grows with its entries.
        expected += allocated_of(&fs::symlink_metadata(&victim).unwrap());

        let targets = vec![Target {
            path: victim.clone(),
            size: 250,
            allocated_size: expected,
            filesystem_identity: filesystem_identity(&victim),
            is_dir: true,
        }];
        let progress = DeleteProgress::new(1);
        delete_all(
            &targets,
            &temp.0,
            filesystem_identity(&temp.0),
            &progress,
            &Cancel::new(),
        );

        // 25 files plus the directory itself.
        assert_eq!(progress.files_removed(), 26);
        assert_eq!(progress.items_done(), 1);
        assert_eq!(progress.freed(), expected);
        assert!(progress.is_finished());
    }

    #[test]
    fn a_cancelled_deletion_stops_and_says_which_ones_it_never_reached() {
        let temp = TempTree::new("cancelled");
        let first = temp.dir("first");
        let second = temp.dir("second");

        let targets = vec![
            Target {
                path: first,
                size: 10,
                allocated_size: 10,
                filesystem_identity: None,
                is_dir: true,
            },
            Target {
                path: second.clone(),
                size: 10,
                allocated_size: 10,
                filesystem_identity: None,
                is_dir: true,
            },
        ];

        let cancel = Cancel::new();
        cancel.cancel();
        let outcomes = delete_all(
            &targets,
            &temp.0,
            filesystem_identity(&temp.0),
            &DeleteProgress::new(2),
            &cancel,
        );

        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            Outcome::Refused {
                reason: Refusal::Stopped,
                ..
            }
        )));
        assert!(second.exists(), "a stopped deletion leaves the rest alone");
        assert_eq!(freed(&outcomes), 0);
    }

    #[test]
    fn a_background_deletion_can_be_joined_for_its_outcomes() {
        let temp = TempTree::new("background");
        let victim = temp.dir("victim");
        temp.file("victim/blob", 500);

        let (progress, _cancel, handle) = delete_in_background(
            vec![Target {
                path: victim.clone(),
                size: 500,
                allocated_size: 500,
                filesystem_identity: filesystem_identity(&victim),
                is_dir: true,
            }],
            temp.0.clone(),
            filesystem_identity(&temp.0),
        );
        let outcomes = handle.join().unwrap();

        assert!(progress.is_finished());
        assert_eq!(freed(&outcomes), progress.freed());
        assert!(!victim.exists());
    }

    #[test]
    fn a_refused_target_is_never_touched() {
        let temp = TempTree::new("refused");
        let outsider = TempTree::new("refused-other");
        let precious = outsider.dir("precious");

        let targets = vec![Target {
            path: precious.clone(),
            size: 10,
            allocated_size: 10,
            filesystem_identity: None,
            is_dir: true,
        }];

        let outcomes = run(&targets, &temp.0);

        assert!(matches!(
            outcomes[0],
            Outcome::Refused {
                reason: Refusal::OutsideScan,
                ..
            }
        ));
        assert!(precious.exists(), "a refused path must survive");
        assert_eq!(freed(&outcomes), 0);
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_are_not_counted_multiple_times() {
        let temp = TempTree::new("hardlink");
        let victim = temp.dir("victim");
        let file1 = temp.file("victim/original.bin", 10_000);
        let file2 = victim.join("link.bin");
        std::fs::hard_link(&file1, &file2).unwrap();

        let file_alloc = allocated_of(&std::fs::symlink_metadata(&file1).unwrap());
        let victim_alloc = allocated_of(&std::fs::symlink_metadata(&victim).unwrap());

        let progress = DeleteProgress::new(1);
        let outcomes = delete_all(
            &[Target {
                path: victim.clone(),
                size: 20_000,
                allocated_size: file_alloc + victim_alloc,
                filesystem_identity: filesystem_identity(&victim),
                is_dir: true,
            }],
            &temp.0,
            filesystem_identity(&temp.0),
            &progress,
            &Cancel::new(),
        );

        assert!(outcomes[0].succeeded());
        // 2 files + 1 dir = 3 entries
        assert_eq!(progress.files_removed(), 3);
        // Space should be counted for the file once (+ dir overhead), exactly file_alloc + victim_alloc
        assert_eq!(progress.freed(), file_alloc + victim_alloc);
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_across_separate_targets_are_counted_once() {
        let temp = TempTree::new("hardlink-targets");
        let first = temp.file("first.bin", 10_000);
        let second = temp.0.join("second.bin");
        std::fs::hard_link(&first, &second).unwrap();
        let expected = allocated_of(&std::fs::symlink_metadata(&first).unwrap());
        let progress = DeleteProgress::new(2);

        let outcomes = delete_all(
            &[
                Target {
                    path: first.clone(),
                    size: 10_000,
                    allocated_size: expected,
                    filesystem_identity: filesystem_identity(&first),
                    is_dir: false,
                },
                Target {
                    path: second.clone(),
                    size: 10_000,
                    allocated_size: 0,
                    filesystem_identity: filesystem_identity(&second),
                    is_dir: false,
                },
            ],
            &temp.0,
            filesystem_identity(&temp.0),
            &progress,
            &Cancel::new(),
        );

        assert!(outcomes.iter().all(Outcome::succeeded));
        assert_eq!(progress.freed(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn blocks_are_not_reported_freed_while_a_hard_link_survives() {
        let temp = TempTree::new("hardlink-survivor");
        let victim = temp.dir("victim");
        let inside = temp.file("victim/file.bin", 10_000);
        let survivor = temp.0.join("survivor.bin");
        std::fs::hard_link(&inside, &survivor).unwrap();
        let file_bytes = allocated_of(&std::fs::symlink_metadata(&inside).unwrap());
        let directory_bytes = allocated_of(&std::fs::symlink_metadata(&victim).unwrap());

        let outcomes = run(
            &[Target {
                path: victim,
                size: directory_bytes,
                allocated_size: file_bytes + directory_bytes,
                filesystem_identity: None,
                is_dir: true,
            }],
            &temp.0,
        );

        assert_eq!(freed(&outcomes), directory_bytes);
        assert!(
            outcomes[0].needs_rescan(),
            "the scan credited the file to the target, so freeing less must be flagged"
        );
        assert!(survivor.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_completed_deletion_reports_a_link_left_elsewhere() {
        let temp = TempTree::new("hardlink-credited-elsewhere");
        let victim = temp.dir("victim");
        let inside = temp.file("victim/file.bin", 10_000);
        std::fs::hard_link(&inside, temp.0.join("survivor.bin")).unwrap();
        let directory_bytes = allocated_of(&std::fs::symlink_metadata(&victim).unwrap());

        // The scan credited the file to the link outside the target.
        let outcomes = run(
            &[Target {
                path: victim,
                size: directory_bytes,
                allocated_size: directory_bytes,
                filesystem_identity: None,
                is_dir: true,
            }],
            &temp.0,
        );

        assert!(outcomes[0].succeeded());
        assert!(
            !outcomes[0].needs_rescan(),
            "the tree already credits the surviving link, so nothing needs a rescan"
        );
        assert!(outcomes[0].links_survived());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_deletion_reports_a_link_it_left_elsewhere() {
        let temp = TempTree::new("hardlink-failed");
        let victim = temp.dir("victim");
        // Both files keep a link outside, so whichever goes first leaves one.
        for name in ["first.bin", "second.bin"] {
            let inside = temp.file(&format!("victim/{name}"), 4096);
            std::fs::hard_link(&inside, temp.0.join(name)).unwrap();
        }

        unix_delete::FAILURE_AFTER_ENTRIES.set(Some(1));
        let outcomes = run(
            &[Target {
                path: victim,
                size: 8192,
                allocated_size: 8192,
                filesystem_identity: None,
                is_dir: true,
            }],
            &temp.0,
        );
        unix_delete::FAILURE_AFTER_ENTRIES.set(None);

        assert!(!outcomes[0].succeeded());
        assert!(outcomes[0].links_survived());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_cannot_escape_the_scan_root() {
        let scan = TempTree::new("parent-symlink-scan");
        let outside = TempTree::new("parent-symlink-outside");
        let precious = outside.file("precious.bin", 100);
        std::os::unix::fs::symlink(&outside.0, scan.0.join("escape")).unwrap();
        let target = scan.0.join("escape/precious.bin");

        let outcomes = delete_all(
            &[Target {
                path: target,
                size: 100,
                allocated_size: 100,
                filesystem_identity: filesystem_identity(&precious),
                is_dir: false,
            }],
            &scan.0,
            filesystem_identity(&scan.0),
            &DeleteProgress::new(1),
            &Cancel::new(),
        );

        assert!(matches!(outcomes[0], Outcome::Failed { .. }));
        assert!(precious.exists());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_scan_root_with_a_symlink_cannot_redirect_deletion() {
        let container = TempTree::new("replaced-root");
        let scan_root = container.dir("scan");
        container.file("scan/victim.bin", 100);
        let expected_root_identity = filesystem_identity(&scan_root);
        let moved_root = container.0.join("moved-scan");
        std::fs::rename(&scan_root, &moved_root).unwrap();

        let outside = TempTree::new("replaced-root-outside");
        let precious = outside.file("victim.bin", 100);
        std::os::unix::fs::symlink(&outside.0, &scan_root).unwrap();

        // The target identity matches what the link now points at, so only
        // NOFOLLOW on the root or the root-identity check can refuse this.
        let outcomes = delete_all(
            &[Target {
                path: scan_root.join("victim.bin"),
                size: 100,
                allocated_size: 100,
                filesystem_identity: filesystem_identity(&precious),
                is_dir: false,
            }],
            &scan_root,
            expected_root_identity,
            &DeleteProgress::new(1),
            &Cancel::new(),
        );

        assert!(matches!(outcomes[0], Outcome::Failed { .. }));
        assert!(precious.exists());
        assert!(moved_root.join("victim.bin").exists());
    }

    #[cfg(unix)]
    #[test]
    fn batch_deletion_without_a_scan_root_identity_is_refused() {
        let temp = TempTree::new("missing-root-identity");
        let victim = temp.file("victim.bin", 100);

        let outcomes = delete_all(
            &[Target {
                path: victim.clone(),
                size: 100,
                allocated_size: 100,
                filesystem_identity: filesystem_identity(&victim),
                is_dir: false,
            }],
            &temp.0,
            None,
            &DeleteProgress::new(1),
            &Cancel::new(),
        );

        assert!(matches!(outcomes[0], Outcome::Failed { .. }));
        assert!(victim.exists());
    }

    #[cfg(unix)]
    #[test]
    fn batch_deletion_without_a_target_identity_is_refused() {
        let temp = TempTree::new("missing-target-identity");
        let victim = temp.file("victim.bin", 100);

        let outcomes = delete_all(
            &[Target {
                path: victim.clone(),
                size: 100,
                allocated_size: 100,
                filesystem_identity: None,
                is_dir: false,
            }],
            &temp.0,
            filesystem_identity(&temp.0),
            &DeleteProgress::new(1),
            &Cancel::new(),
        );

        assert!(matches!(outcomes[0], Outcome::Failed { .. }));
        assert!(victim.exists());
    }

    #[cfg(unix)]
    #[test]
    fn batch_deletion_with_a_relative_scan_root_is_refused() {
        let root = PathBuf::from(format!(
            "target/duzo-relative-batch-root-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let victim = root.join("victim.bin");
        fs::write(&victim, b"temporary").unwrap();

        let outcomes = delete_all(
            &[Target {
                path: victim.clone(),
                size: 9,
                allocated_size: 9,
                filesystem_identity: filesystem_identity(&victim),
                is_dir: false,
            }],
            &root,
            filesystem_identity(&root),
            &DeleteProgress::new(1),
            &Cancel::new(),
        );

        let kept = victim.exists();
        let _ = fs::remove_dir_all(&root);

        assert!(matches!(outcomes[0], Outcome::Failed { .. }));
        assert!(kept);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_scan_root_is_refused_even_when_the_target_inode_matches() {
        let container = TempTree::new("replaced-root-hardlink");
        let scan_root = container.dir("scan");
        let victim = container.file("scan/victim.bin", 100);
        let expected_root_identity = filesystem_identity(&scan_root);
        let expected_target_identity = filesystem_identity(&victim);
        let moved_root = container.0.join("moved-scan");
        fs::rename(&scan_root, &moved_root).unwrap();

        fs::create_dir(&scan_root).unwrap();
        let replacement_link = scan_root.join("victim.bin");
        fs::hard_link(moved_root.join("victim.bin"), &replacement_link).unwrap();
        let outcomes = delete_all(
            &[Target {
                path: replacement_link.clone(),
                size: 100,
                allocated_size: 100,
                filesystem_identity: expected_target_identity,
                is_dir: false,
            }],
            &scan_root,
            expected_root_identity,
            &DeleteProgress::new(1),
            &Cancel::new(),
        );

        assert!(matches!(outcomes[0], Outcome::Failed { .. }));
        assert!(replacement_link.exists());
        assert!(moved_root.join("victim.bin").exists());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_target_at_the_same_path_is_refused() {
        let temp = TempTree::new("replaced-target");
        let victim = temp.file("victim.bin", 100);
        let expected_target_identity = filesystem_identity(&victim);
        let original = temp.0.join("original.bin");
        fs::rename(&victim, &original).unwrap();
        fs::write(&victim, b"replacement").unwrap();

        let outcomes = delete_all(
            &[Target {
                path: victim.clone(),
                size: 100,
                allocated_size: 100,
                filesystem_identity: expected_target_identity,
                is_dir: false,
            }],
            &temp.0,
            filesystem_identity(&temp.0),
            &DeleteProgress::new(1),
            &Cancel::new(),
        );

        assert!(matches!(outcomes[0], Outcome::Failed { .. }));
        assert!(victim.exists());
        assert!(original.exists());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_target_directory_at_the_same_path_is_refused() {
        let temp = TempTree::new("replaced-target-directory");
        let victim = temp.dir("victim");
        temp.file("victim/original.bin", 100);
        let expected_target_identity = filesystem_identity(&victim);
        let original = temp.0.join("original-directory");
        fs::rename(&victim, &original).unwrap();
        fs::create_dir(&victim).unwrap();
        temp.file("victim/replacement.bin", 100);

        let outcomes = delete_all(
            &[Target {
                path: victim.clone(),
                size: 100,
                allocated_size: 100,
                filesystem_identity: expected_target_identity,
                is_dir: true,
            }],
            &temp.0,
            filesystem_identity(&temp.0),
            &DeleteProgress::new(1),
            &Cancel::new(),
        );

        assert!(matches!(outcomes[0], Outcome::Failed { .. }));
        assert!(victim.join("replacement.bin").exists());
        assert!(original.join("original.bin").exists());
    }

    #[cfg(unix)]
    #[test]
    fn successful_outcomes_report_allocated_bytes_not_the_requested_size() {
        let temp = TempTree::new("sparse-outcome");
        let sparse = temp.0.join("sparse.bin");
        let file = fs::File::create(&sparse).unwrap();
        file.set_len(8 * 1024 * 1024).unwrap();
        let expected = allocated_of(&fs::symlink_metadata(&sparse).unwrap());

        let outcomes = run(
            &[Target {
                path: sparse,
                size: 8 * 1024 * 1024,
                allocated_size: expected,
                filesystem_identity: None,
                is_dir: false,
            }],
            &temp.0,
        );

        assert!(matches!(
            outcomes[0],
            Outcome::Deleted { freed, .. } if freed == expected
        ));
        assert_eq!(freed(&outcomes), expected);
    }

    #[cfg(unix)]
    #[test]
    fn successful_outcomes_request_rescan_when_accounting_drifts() {
        let temp = TempTree::new("drift-outcome");
        let victim = temp.file("victim.bin", 100);
        let actual = allocated_of(&fs::symlink_metadata(&victim).unwrap());

        let outcomes = run(
            &[Target {
                path: victim,
                size: 100,
                allocated_size: actual.saturating_add(512),
                filesystem_identity: None,
                is_dir: false,
            }],
            &temp.0,
        );

        assert!(matches!(
            outcomes[0],
            Outcome::Deleted {
                tree_needs_rescan: true,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_mid_delete_failure_reports_the_partial_filesystem_change() {
        let temp = TempTree::new("partial-outcome");
        let victim = temp.dir("victim");
        let first = temp.file("victim/first.bin", 4096);
        temp.file("victim/second.bin", 4096);
        // Both files allocate the same, so it does not matter which one went.
        let one_file = allocated_of(&fs::symlink_metadata(&first).unwrap());
        let progress = DeleteProgress::new(1);

        unix_delete::FAILURE_AFTER_ENTRIES.set(Some(1));
        let outcomes = delete_all(
            &[Target {
                path: victim.clone(),
                size: 8192,
                allocated_size: 8192,
                filesystem_identity: filesystem_identity(&victim),
                is_dir: true,
            }],
            &temp.0,
            filesystem_identity(&temp.0),
            &progress,
            &Cancel::new(),
        );
        unix_delete::FAILURE_AFTER_ENTRIES.set(None);

        assert!(matches!(
            outcomes[0],
            Outcome::Failed {
                removed_entries: 1,
                removed_bytes,
                tree_may_have_changed: true,
                ..
            } if removed_bytes == one_file
        ));
        assert_eq!(freed(&outcomes), one_file);
        assert_eq!(progress.files_removed(), 1);
        assert!(victim.exists());
        assert_eq!(fs::read_dir(victim).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_nested_mount_is_found_before_any_sibling_is_removed() {
        let temp = TempTree::new("nested-mount-preflight");
        let victim = temp.dir("victim");
        // Many siblings, so a walk without the preflight is likely to remove
        // one before it reaches the mount — unless readdir happens to list the
        // mount first, which this fixed set of names cannot rule out.
        let siblings: Vec<PathBuf> = (0..16)
            .map(|index| temp.file(&format!("victim/sibling-{index}.bin"), 100))
            .collect();
        let mounted = temp.dir("victim/mounted");
        let precious = temp.file("victim/mounted/precious.bin", 100);

        let result = unix_delete::delete_with_blocked_path(&victim, &temp.0, &mounted);

        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(siblings.iter().all(|sibling| sibling.exists()));
        assert!(precious.exists());
        assert!(victim.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_mount_crossed_only_on_the_way_to_the_target_does_not_block_it() {
        // The scan may walk through a nested mount to reach a target living
        // under it (e.g. `/home` under a `/` scan); only crossings *inside*
        // the target's own subtree are refused.
        let temp = TempTree::new("mounted-ancestor");
        let mounted = temp.dir("mounted");
        let precious = temp.file("mounted/precious.bin", 100);

        let result = unix_delete::delete_with_blocked_path(&precious, &temp.0, &mounted);

        assert!(result.is_ok(), "{result:?}");
        assert!(!precious.exists());
    }

    #[cfg(unix)]
    #[test]
    fn moving_the_target_outside_the_scan_stops_further_removal() {
        let container = TempTree::new("moved-target");
        let scan = container.dir("scan");
        let victim = container.dir("scan/victim");
        let survivors = fill_past_one_anchor_check(&container, "scan/victim");
        let moved = container.0.join("moved-victim");

        let result = unix_delete::delete_with_move_after(&victim, &scan, &victim, moved.clone(), 1);

        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(moved.exists());
        assert_eq!(fs::read_dir(moved).unwrap().count(), survivors);
    }

    #[cfg(unix)]
    #[test]
    fn moving_a_nested_directory_outside_the_target_stops_further_removal() {
        let container = TempTree::new("moved-nested-directory");
        let scan = container.dir("scan");
        let victim = container.dir("scan/victim");
        let nested = container.dir("scan/victim/nested");
        let survivors = fill_past_one_anchor_check(&container, "scan/victim/nested");
        let moved = container.0.join("moved-nested");

        let result = unix_delete::delete_with_move_after(&victim, &scan, &nested, moved.clone(), 1);

        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(moved.exists());
        assert_eq!(fs::read_dir(moved).unwrap().count(), survivors);
    }

    #[cfg(unix)]
    #[test]
    fn moving_a_scan_root_ancestor_stops_further_removal() {
        let container = TempTree::new("moved-scan-ancestor");
        let ancestor = container.dir("ancestor");
        let scan = container.dir("ancestor/scan");
        let victim = container.dir("ancestor/scan/victim");
        let survivors = fill_past_one_anchor_check(&container, "ancestor/scan/victim");
        let moved = container.0.join("moved-ancestor");

        let result =
            unix_delete::delete_with_move_after(&victim, &scan, &ancestor, moved.clone(), 1);

        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let moved_victim = moved.join("scan/victim");
        assert!(moved_victim.exists());
        assert_eq!(fs::read_dir(moved_victim).unwrap().count(), survivors);
    }

    /// Fills `dir` with two files more than one anchor check lets go, and
    /// returns those two: a move after the first removal is caught at the
    /// next check, with the rest still in place.
    #[cfg(unix)]
    fn fill_past_one_anchor_check(tree: &TempTree, dir: &str) -> usize {
        let survivors = 2;
        for index in 0..ANCHOR_CHECK_INTERVAL as usize + survivors {
            tree.file(&format!("{dir}/file-{index}.bin"), 100);
        }
        survivors
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_still_holding_entries_after_its_listing_is_listed_again() {
        let container = TempTree::new("unlisted-entry");
        let scan = container.dir("scan");
        let victim = container.dir("scan/victim");
        container.file("scan/victim/listed.bin", 100);

        let result = unix_delete::delete_with_an_unlisted_entry(&victim, &scan, "unlisted.bin");

        assert!(result.is_ok(), "{result:?}");
        assert!(!victim.exists());
    }

    #[cfg(unix)]
    #[test]
    fn recursive_deletion_publishes_progress_before_the_worker_finishes() {
        let temp = TempTree::new("live-progress");
        let victim = temp.dir("victim");
        temp.file("victim/first.bin", 100);
        temp.file("victim/second.bin", 100);
        let progress = Arc::new(DeleteProgress::new(1));
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let resume = Arc::new(std::sync::Barrier::new(2));
        let handle = unix_delete::delete_with_pause(
            victim.clone(),
            temp.0.clone(),
            Arc::clone(&progress),
            reached_tx,
            Arc::clone(&resume),
        );

        reached_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(progress.files_removed(), 1);
        assert!(victim.exists());
        resume.wait();

        handle.join().unwrap().unwrap();
        assert!(!victim.exists());
    }

    #[test]
    fn directory_contents_are_deleted_when_owner_write_is_missing() {
        let temp = TempTree::new("readonly");
        let victim = temp.dir("victim");
        let ro_file = temp.file("victim/readonly.txt", 100);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&victim).unwrap().permissions();
            perms.set_mode(0o500);
            std::fs::set_permissions(&victim, perms).unwrap();
            let _ = &ro_file;
        }
        #[cfg(not(unix))]
        {
            let mut perms = std::fs::metadata(&ro_file).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&ro_file, perms).unwrap();
        }

        let outcomes = run(
            &[Target {
                path: victim.clone(),
                size: 100,
                allocated_size: 100,
                filesystem_identity: None,
                is_dir: true,
            }],
            &temp.0,
        );

        assert!(outcomes[0].succeeded());
        assert!(!victim.exists());
    }

    #[cfg(unix)]
    #[test]
    fn mountinfo_paths_are_decoded_and_detected() {
        let mut mounts = mount_points_from(
            b"36 25 0:32 / / rw,relatime - overlay overlay rw\n\
              77 36 0:42 / /tmp/bind\\040mount rw,relatime - ext4 /dev/sda rw\n",
        );

        assert!(mounts.contains(Path::new("/")));
        assert!(mounts.contains(Path::new("/tmp/bind mount")));
        assert_eq!(
            containing_mount_point(&mounts, Path::new("/tmp/bind mount/child")),
            Some(Path::new("/tmp/bind mount"))
        );
        assert_eq!(
            containing_mount_point(&mounts, Path::new("/ordinary/path")),
            Some(Path::new("/"))
        );

        let temp = TempTree::new("listed-mount");
        let canonical = temp.0.canonicalize().unwrap();
        mounts.insert(canonical);
        assert!(listed_mount_point(&mounts, &temp.0));
    }

    /// An unmounted btrfs subvolume is on its own device but absent from
    /// mountinfo; `/proc` stands in for one with an empty mount list.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_folder_on_another_device_is_a_mount_point_even_when_unlisted() {
        let unlisted = std::collections::HashSet::new();

        assert!(is_linux_mount_point(&unlisted, Path::new("/proc")));
        assert!(!is_linux_mount_point(&unlisted, Path::new("/usr/bin")));
    }

    #[cfg(unix)]
    #[test]
    fn fdinfo_mount_id_is_read_from_its_own_line() {
        assert_eq!(
            mount_id_from_fdinfo(b"pos:\t0\nflags:\t02200000\nmnt_id:\t29\nino:\t2\n"),
            Some(29)
        );
        // Kernels before 3.15 have no `mnt_id` line at all.
        assert_eq!(mount_id_from_fdinfo(b"pos:\t0\nflags:\t02200000\n"), None);
    }
}
