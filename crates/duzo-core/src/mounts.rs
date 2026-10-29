//! Mounted filesystems, in human terms.
//!
//! The public shape here is deliberately friendly — a volume has a *name*, a
//! size and a mount point. Device nodes, filesystem types and inode counts are
//! still carried, but they are details a caller opts into rather than the
//! headline.

// Only the Linux mount table needs a lookup map for volume labels.
#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Inodes {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

impl Inodes {
    pub fn used_fraction(&self) -> f64 {
        crate::size::fraction(self.used, self.total)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Filesystem {
    /// What a person would call it: "Macintosh HD", "Root", "backup".
    pub name: String,
    pub mount_point: PathBuf,
    /// The device node, e.g. `/dev/disk3s1s1`.
    pub device: String,
    /// APFS, ext4, xfs...
    pub fs_type: String,
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub read_only: bool,
    pub removable: bool,
    /// SSD / HDD / Removable / Unknown, as far as the OS will say.
    pub kind: String,
    /// Kernel bookkeeping rather than storage: tmpfs, proc, cgroup...
    pub pseudo: bool,
    pub inodes: Option<Inodes>,
}

impl Filesystem {
    pub fn used_fraction(&self) -> f64 {
        crate::size::fraction(self.used, self.total)
    }
}

/// Filesystem types that hold no durable data and only add noise to a list of
/// "what is full".
const PSEUDO_TYPES: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fuse.gvfsd-fuse",
    "fuse.portal",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "overlay",
    "proc",
    "pstore",
    "ramfs",
    "rpc_pipefs",
    "securityfs",
    "selinuxfs",
    "squashfs",
    "sysfs",
    "tmpfs",
    "tracefs",
];

/// Filesystems whose contents do not live on this disk, or that answer a
/// `stat` over a network. Walking one is both useless for "what is filling my
/// disk" and ruinously slow: a single directory listing on a blob-storage
/// mount can take five seconds.
///
/// `fuseblk` is deliberately absent — that is how local NTFS and exFAT drives
/// are mounted, and those really are this machine's storage.
const REMOTE_TYPES: &[&str] = &[
    "9p",
    "afpfs",
    "afs",
    "beegfs",
    "ceph",
    "cifs",
    "curlftpfs",
    "davfs",
    "ftpfs",
    "fuse",
    "fuse.rclone",
    "fuse.s3fs",
    "fuse.sshfs",
    "gfs2",
    "glusterfs",
    "lustre",
    "ncpfs",
    "nfs",
    "nfs4",
    "smb2",
    "smb3",
    "smbfs",
    "sshfs",
    "webdav",
];

/// True for filesystems that are somewhere else pretending to be here.
pub fn is_remote(fs_type: &str) -> bool {
    REMOTE_TYPES.contains(&fs_type)
        // Most fuse drivers register as `fuse.<driver>`; blobfuse2, sshfs and
        // rclone all arrive this way.
        || fs_type.starts_with("fuse.")
}

/// Every mount point that is not really on this disk.
pub fn remote_mount_points() -> Vec<PathBuf> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let mounts = kernel_mounts();
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let mounts = list(true)
        .into_iter()
        .map(|fs| (fs.fs_type, fs.mount_point));
    mounts
        .into_iter()
        .filter(|(fs_type, _)| is_remote(fs_type))
        .map(|(_, mount_point)| mount_point)
        .collect()
}

/// Every mount's filesystem type and mount point, as the kernel lists them.
/// sysinfo leaves out volumes that are not local (SMB, NFS and AFP shares),
/// which are exactly the ones a scan has to stop at. `MNT_NOWAIT` answers from
/// cached state, so a share whose server is gone does not hang the call.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn kernel_mounts() -> Vec<(String, PathBuf)> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: a null buffer asks only for the number of mounts.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // SAFETY: `statfs` is plain data, for which all-zero bytes are valid.
    let mut raw = vec![unsafe { std::mem::zeroed::<libc::statfs>() }; count];
    let Ok(size) = libc::c_int::try_from(count * std::mem::size_of::<libc::statfs>()) else {
        return Vec::new();
    };
    // SAFETY: `raw` holds `count` entries, and `size` is their size in bytes.
    let filled = unsafe { libc::getfsstat(raw.as_mut_ptr(), size, libc::MNT_NOWAIT) };
    let Ok(filled) = usize::try_from(filled) else {
        return Vec::new();
    };
    raw.truncate(filled);
    raw.iter()
        .map(|fs| {
            // SAFETY: the kernel fills both as NUL-terminated strings.
            let (fs_type, mount_point) = unsafe {
                (
                    CStr::from_ptr(fs.f_fstypename.as_ptr()),
                    CStr::from_ptr(fs.f_mntonname.as_ptr()),
                )
            };
            (
                fs_type.to_string_lossy().into_owned(),
                PathBuf::from(OsStr::from_bytes(mount_point.to_bytes())),
            )
        })
        .collect()
}

/// The mount points a scan stops at: network mounts (only with
/// `skip_remote`) and mounts that expose operating-system internals rather
/// than user data.
#[cfg(target_os = "linux")]
pub(crate) fn scan_boundaries(skip_remote: bool) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let remote = if skip_remote {
        remote_mount_points()
    } else {
        Vec::new()
    };
    (remote, pseudo_mount_points())
}

/// One listing serves both, except that on macOS it holds no network shares,
/// so those come from the kernel's own list.
#[cfg(not(target_os = "linux"))]
pub(crate) fn scan_boundaries(skip_remote: bool) -> (Vec<PathBuf>, Vec<PathBuf>) {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let remote = if skip_remote {
        remote_mount_points()
    } else {
        Vec::new()
    };
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let mut remote = Vec::new();
    let mut pseudo = Vec::new();
    for fs in list(true) {
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        if skip_remote && is_remote(&fs.fs_type) {
            remote.push(fs.mount_point.clone());
        }
        if fs.pseudo {
            pseudo.push(fs.mount_point);
        }
    }
    (remote, pseudo)
}

#[cfg(target_os = "linux")]
fn pseudo_mount_points() -> Vec<PathBuf> {
    pseudo_mount_points_from_rows(read_proc_mounts())
}

#[cfg(target_os = "linux")]
fn pseudo_mount_points_from_rows(rows: Vec<MountRow>) -> Vec<PathBuf> {
    // `/proc/mounts` lists mounts in the order the kernel mounted them, so a
    // later row at the same mount point is stacked on top of an earlier one
    // and hides it — the classic case is an autofs placeholder that a real
    // filesystem gets mounted over once something touches it. A path is only
    // pseudo when its *topmost* row is; keep the last row seen per mount
    // point rather than flagging the point pseudo because any row there ever
    // was.
    let mut topmost: std::collections::HashMap<PathBuf, bool> = std::collections::HashMap::new();
    for MountRow {
        device: _,
        mount_point,
        fs_type,
        options: _,
    } in rows
    {
        let pseudo = is_pseudo(&fs_type, &mount_point);
        topmost.insert(mount_point, pseudo);
    }
    topmost
        .into_iter()
        .filter_map(|(mount_point, pseudo)| pseudo.then_some(mount_point))
        .collect()
}

/// Mount points that exist for the OS's benefit, not the user's.
const PSEUDO_PREFIXES: &[&str] = &["/proc", "/sys", "/dev", "/run", "/snap", "/var/snap"];

fn is_pseudo(fs_type: &str, mount_point: &Path) -> bool {
    if PSEUDO_TYPES.contains(&fs_type) {
        return true;
    }
    let mount = mount_point.to_string_lossy();
    // macOS synthesises a pile of mounts under /System/Volumes; only the Data
    // volume holds user files. It is real; `list` folds it into the `/` row.
    if mount.starts_with("/System/Volumes/") && mount != "/System/Volumes/Data" {
        return true;
    }
    if mount == "/run/media" || mount.starts_with("/run/media/") {
        return false;
    }
    PSEUDO_PREFIXES
        .iter()
        .any(|prefix| mount == *prefix || mount.starts_with(&format!("{prefix}/")))
}

/// Every mounted filesystem, real ones first and largest first within that.
///
/// This is the list a person picks from, so macOS's system/Data split is
/// folded into one "Macintosh HD" row here; `for_path` keeps the halves apart.
pub fn list(include_pseudo: bool) -> Vec<Filesystem> {
    let mut filesystems = collect();
    #[cfg(target_os = "macos")]
    fold_data_volume(&mut filesystems);
    if !include_pseudo {
        filesystems.retain(|fs| !fs.pseudo && fs.total > 0);
    }
    filesystems.sort_by(|a, b| {
        a.pseudo
            .cmp(&b.pseudo)
            .then_with(|| b.total.cmp(&a.total))
            .then_with(|| a.mount_point.cmp(&b.mount_point))
    });
    filesystems
}

/// The actual filesystem `path` lives on, with that mount's own metadata.
///
/// On macOS the kernel says which mount holds the path (firmlinks make
/// `/Users` live on `/System/Volumes/Data`, not on `/`); a mount sysinfo
/// leaves out is described from `statfs`. Elsewhere, or when the kernel cannot
/// be asked, the longest mount-point prefix answers.
pub fn for_path(path: &Path) -> Option<Filesystem> {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let filesystems = collect();
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    if let Some(raw) = raw_statfs(&absolute) {
        let mount_point = c_path(&raw.f_mntonname);
        return Some(
            filesystems
                .into_iter()
                .find(|fs| fs.mount_point == mount_point)
                .unwrap_or_else(|| unlisted_filesystem(&raw)),
        );
    }
    filesystems
        .into_iter()
        .filter(|fs| absolute.starts_with(&fs.mount_point))
        .max_by_key(|fs| fs.mount_point.as_os_str().len())
}

/// Pretty names, device kind and removability, keyed by mount point.
///
/// Linux builds the mount list from `/proc/mounts`, which knows nothing about
/// volume labels or whether a disk is removable, so sysinfo fills those in.
/// Elsewhere sysinfo *is* the mount list and this is not needed.
#[cfg(target_os = "linux")]
fn disk_hints() -> HashMap<PathBuf, (String, String, bool)> {
    use sysinfo::{DiskKind, Disks};

    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .map(|disk| {
            let kind = match disk.kind() {
                DiskKind::SSD => "SSD".to_string(),
                DiskKind::HDD => "HDD".to_string(),
                DiskKind::Unknown(_) => "Unknown".to_string(),
            };
            (
                disk.mount_point().to_path_buf(),
                (
                    disk.name().to_string_lossy().to_string(),
                    kind,
                    disk.is_removable(),
                ),
            )
        })
        .collect()
}

fn friendly_name(hint: Option<&str>, device: &str, mount_point: &Path) -> String {
    // A hint that is not a device path is a real volume label. Fuse mounts
    // report the same string for both ("blobfuse2"), which is a driver name
    // rather than something a person would call the volume.
    if let Some(hint) = hint {
        let hint = hint.trim();
        if !hint.is_empty() && !hint.starts_with('/') && hint != device {
            return hint.to_string();
        }
    }
    if mount_point == Path::new("/") {
        return "Root".to_string();
    }
    match mount_point.file_name() {
        Some(name) => name.to_string_lossy().to_string(),
        None => {
            if device.is_empty() {
                mount_point.to_string_lossy().to_string()
            } else {
                device.to_string()
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn collect() -> Vec<Filesystem> {
    let hints = disk_hints();
    let mut seen_devices: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut filesystems = Vec::new();

    for row in read_proc_mounts() {
        // Bind mounts, and the several mounts a container stacks onto one
        // device, would otherwise repeat the same numbers row after row.
        if row.device.starts_with("/dev/") && !seen_devices.insert(row.device.clone()) {
            continue;
        }

        let stat = os_stat(&row.mount_point);
        let hint = hints.get(&row.mount_point);
        let total = stat.as_ref().map(|s| s.total).unwrap_or(0);
        let available = stat.as_ref().map(|s| s.available).unwrap_or(0);
        let used = stat.as_ref().map(|s| s.used).unwrap_or(0);

        filesystems.push(Filesystem {
            name: friendly_name(hint.map(|h| h.0.as_str()), &row.device, &row.mount_point),
            device: row.device.clone(),
            pseudo: is_pseudo(&row.fs_type, &row.mount_point),
            fs_type: row.fs_type,
            total,
            used,
            available,
            read_only: stat.as_ref().map(|s| s.read_only).unwrap_or(false)
                || row.options.split(',').any(|opt| opt == "ro"),
            removable: hint.map(|h| h.2).unwrap_or(false),
            kind: hint
                .map(|h| h.1.clone())
                .unwrap_or_else(|| "Unknown".to_string()),
            inodes: stat.as_ref().and_then(|s| s.inodes.clone()),
            mount_point: row.mount_point,
        });
    }

    filesystems
}

#[cfg(not(target_os = "linux"))]
fn collect() -> Vec<Filesystem> {
    use sysinfo::{DiskKind, Disks};

    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .map(|disk| {
            let mount_point = disk.mount_point().to_path_buf();
            let fs_type = disk.file_system().to_string_lossy().to_string();
            let stat = os_stat(&mount_point);
            let raw_name = disk.name().to_string_lossy().to_string();

            // `statfs` knows the device node; sysinfo reports the volume label.
            let device = stat
                .as_ref()
                .and_then(|s| s.device.clone())
                .unwrap_or_else(|| raw_name.clone());

            let total = stat
                .as_ref()
                .map(|s| s.total)
                .unwrap_or_else(|| disk.total_space());
            let available = stat
                .as_ref()
                .map(|s| s.available)
                .unwrap_or_else(|| disk.available_space());
            let used = stat
                .as_ref()
                .map(|s| s.used)
                .unwrap_or_else(|| disk.total_space().saturating_sub(disk.available_space()));

            Filesystem {
                name: friendly_name(Some(&raw_name), &device, &mount_point),
                device,
                pseudo: is_pseudo(&fs_type, &mount_point),
                fs_type,
                total,
                used,
                available,
                read_only: stat
                    .as_ref()
                    .map(|s| s.read_only)
                    .unwrap_or_else(|| disk.is_read_only()),
                removable: disk.is_removable(),
                kind: match disk.kind() {
                    DiskKind::SSD => "SSD".to_string(),
                    DiskKind::HDD => "HDD".to_string(),
                    DiskKind::Unknown(_) => "Unknown".to_string(),
                },
                inodes: stat.as_ref().and_then(|s| s.inodes.clone()),
                mount_point,
            }
        })
        .collect()
}

/// `MNT_REMOVABLE` from `<sys/mount.h>`; the libc crate does not export it.
#[cfg(any(target_os = "macos", target_os = "ios"))]
const MNT_REMOVABLE: u32 = 0x0000_0200;

/// A row for a mount sysinfo leaves out (it lists only local, browsable
/// volumes, so not devfs, the hidden APFS volumes or network shares), built
/// from the `statfs` that found it.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn unlisted_filesystem(raw: &libc::statfs) -> Filesystem {
    let mount_point = c_path(&raw.f_mntonname);
    let fs_type = c_text(&raw.f_fstypename);
    let stat = fs_stat_from(raw);
    let device = stat.device.unwrap_or_else(|| fs_type.clone());
    Filesystem {
        name: friendly_name(None, &device, &mount_point),
        device,
        pseudo: is_pseudo(&fs_type, &mount_point),
        fs_type,
        total: stat.total,
        used: stat.used,
        available: stat.available,
        read_only: stat.read_only,
        removable: raw.f_flags & MNT_REMOVABLE != 0,
        kind: "Unknown".to_string(),
        inodes: stat.inodes,
        mount_point,
    }
}

/// macOS splits one "Macintosh HD" into a sealed, read-only system snapshot
/// at `/` and the Data volume where everything a user writes lives, with
/// firmlinks making the two look like a single tree under `/`. Present them
/// that way: one row, mounted at `/`, counting inodes from both halves and
/// writable because the half you can write to is.
#[cfg(target_os = "macos")]
fn fold_data_volume(filesystems: &mut Vec<Filesystem>) {
    let Some(data_index) = filesystems
        .iter()
        .position(|fs| fs.mount_point == Path::new("/System/Volumes/Data"))
    else {
        return;
    };
    let data = filesystems.remove(data_index);
    let Some(root) = filesystems
        .iter_mut()
        .find(|fs| fs.mount_point == Path::new("/"))
    else {
        filesystems.insert(data_index, data);
        return;
    };
    root.read_only = data.read_only;
    root.inodes = match (root.inodes.take(), data.inodes) {
        (Some(system), Some(data)) => {
            let used = system.used + data.used;
            Some(Inodes {
                total: used + data.free,
                used,
                free: data.free,
            })
        }
        (system, data) => data.or(system),
    };
}

#[cfg(target_os = "linux")]
struct MountRow {
    device: String,
    mount_point: PathBuf,
    fs_type: String,
    options: String,
}

#[cfg(target_os = "linux")]
fn read_proc_mounts() -> Vec<MountRow> {
    let Ok(contents) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };

    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = unescape_mount_field(fields.next()?);
            let mount_point = unescape_mount_field(fields.next()?);
            let fs_type = unescape_mount_field(fields.next()?);
            let options = fields.next().unwrap_or("").to_string();
            Some(MountRow {
                device,
                mount_point: PathBuf::from(mount_point),
                fs_type,
                options,
            })
        })
        .collect()
}

/// `/proc/mounts` octal-escapes space, tab, newline and backslash.
#[cfg(target_os = "linux")]
fn unescape_mount_field(field: &str) -> String {
    if !field.contains('\\') {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(byte as char);
                for _ in 0..3 {
                    chars.next();
                }
            }
            _ => out.push('\\'),
        }
    }
    out
}

struct FsStat {
    total: u64,
    used: u64,
    available: u64,
    read_only: bool,
    /// Only the BSD `statfs` path learns the device node here; Linux reads it
    /// from `/proc/mounts` instead.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    device: Option<String>,
    inodes: Option<Inodes>,
}

#[cfg(target_os = "linux")]
fn os_stat(mount_point: &Path) -> Option<FsStat> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(mount_point.as_os_str().as_bytes()).ok()?;
    let mut raw: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid NUL-terminated string and `raw` is a valid,
    // correctly sized statvfs the kernel fills in.
    if unsafe { libc::statvfs(path.as_ptr(), &mut raw) } != 0 {
        return None;
    }

    // f_frsize is the fragment size the block counts are expressed in; some
    // filesystems report 0 there, in which case f_bsize is the right unit.
    let unit = if raw.f_frsize > 0 {
        raw.f_frsize as u64
    } else {
        raw.f_bsize as u64
    };
    let total = raw.f_blocks as u64 * unit;
    let free = raw.f_bfree as u64 * unit;
    let available = raw.f_bavail as u64 * unit;

    let inodes_total = raw.f_files as u64;
    let inodes = (inodes_total > 0).then(|| Inodes {
        total: inodes_total,
        free: raw.f_ffree as u64,
        used: inodes_total.saturating_sub(raw.f_ffree as u64),
    });

    Some(FsStat {
        total,
        // `df` calls the reserved-for-root slice "used" too, which is why
        // used + available rarely equals total.
        used: total.saturating_sub(free),
        available,
        read_only: raw.f_flag as u64 & libc::ST_RDONLY != 0,
        device: None,
        inodes,
    })
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn os_stat(mount_point: &Path) -> Option<FsStat> {
    raw_statfs(mount_point).map(|raw| fs_stat_from(&raw))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn raw_statfs(path: &Path) -> Option<libc::statfs> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut raw: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above — valid path, zeroed destination of the right size.
    if unsafe { libc::statfs(path.as_ptr(), &mut raw) } != 0 {
        return None;
    }
    Some(raw)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn fs_stat_from(raw: &libc::statfs) -> FsStat {
    let unit = raw.f_bsize as u64;
    let total = raw.f_blocks * unit;
    let free = raw.f_bfree * unit;
    let available = raw.f_bavail * unit;

    let inodes_total = raw.f_files;
    let inodes = (inodes_total > 0).then(|| Inodes {
        total: inodes_total,
        free: raw.f_ffree,
        used: inodes_total.saturating_sub(raw.f_ffree),
    });

    let device = c_text(&raw.f_mntfromname);
    FsStat {
        total,
        used: total.saturating_sub(free),
        available,
        read_only: raw.f_flags & libc::MNT_RDONLY as u32 != 0,
        device: (!device.is_empty()).then_some(device),
        inodes,
    }
}

/// The bytes of a NUL-terminated name field inside a `statfs`, bounded by
/// the array even if the terminator is missing.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn c_bytes(field: &[libc::c_char]) -> &[u8] {
    // SAFETY: `c_char` and `u8` have the same size and alignment, and the
    // slice covers exactly the array's memory.
    let bytes = unsafe { std::slice::from_raw_parts(field.as_ptr().cast::<u8>(), field.len()) };
    bytes
        .iter()
        .position(|byte| *byte == 0)
        .map_or(bytes, |end| &bytes[..end])
}

/// A name field for display (device, filesystem type).
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn c_text(field: &[libc::c_char]) -> String {
    String::from_utf8_lossy(c_bytes(field)).into_owned()
}

/// A path field, byte for byte: a mount point need not be valid UTF-8, and
/// it must still match the row sysinfo reported for it.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn c_path(field: &[libc::c_char]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(c_bytes(field)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
fn os_stat(_mount_point: &Path) -> Option<FsStat> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_filesystems_are_recognised() {
        assert!(is_remote("nfs4"));
        assert!(is_remote("cifs"));
        assert!(is_remote("fuse"));
        assert!(is_remote("fuse.sshfs"));
        // A locally mounted NTFS drive is this machine's storage.
        assert!(!is_remote("fuseblk"));
        assert!(!is_remote("ext4"));
        assert!(!is_remote("apfs"));
        assert!(!is_remote("xfs"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_data_volume_folds_into_the_root_row() {
        let volume = |mount_point: &str, read_only: bool, inodes: Option<Inodes>| Filesystem {
            name: "Macintosh HD".into(),
            mount_point: PathBuf::from(mount_point),
            device: String::new(),
            fs_type: "apfs".into(),
            total: 100,
            used: 70,
            available: 30,
            read_only,
            removable: false,
            kind: "SSD".into(),
            pseudo: false,
            inodes,
        };
        let inodes = |used, free| {
            Some(Inodes {
                total: used + free,
                used,
                free,
            })
        };
        let mut filesystems = vec![
            volume("/System/Volumes/Data", false, inodes(34_000, 170_000)),
            volume("/", true, inodes(450, 9_000)),
            volume("/Volumes/backup", false, inodes(5, 5)),
        ];

        fold_data_volume(&mut filesystems);

        assert_eq!(filesystems.len(), 2);
        let root = &filesystems[0];
        assert_eq!(root.mount_point, Path::new("/"));
        assert!(!root.read_only, "the half a user writes to is writable");
        let merged = root.inodes.as_ref().unwrap();
        assert_eq!(
            (merged.used, merged.free, merged.total),
            (34_450, 170_000, 204_450)
        );
        assert_eq!(filesystems[1].mount_point, Path::new("/Volumes/backup"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_data_volume_without_a_root_row_is_left_alone() {
        let mut filesystems = vec![Filesystem {
            name: "Macintosh HD".into(),
            mount_point: PathBuf::from("/System/Volumes/Data"),
            device: String::new(),
            fs_type: "apfs".into(),
            total: 1,
            used: 0,
            available: 1,
            read_only: false,
            removable: false,
            kind: "SSD".into(),
            pseudo: false,
            inodes: None,
        }];

        fold_data_volume(&mut filesystems);

        assert_eq!(filesystems.len(), 1);
    }

    #[test]
    fn pseudo_filesystems_are_recognised() {
        assert!(is_pseudo("tmpfs", Path::new("/tmp")));
        assert!(is_pseudo("ext4", Path::new("/proc/whatever")));
        assert!(is_pseudo("apfs", Path::new("/System/Volumes/Preboot")));
        assert!(!is_pseudo("apfs", Path::new("/System/Volumes/Data")));
        assert!(!is_pseudo("ext4", Path::new("/")));
        assert!(!is_pseudo("ext4", Path::new("/home")));
        assert!(!is_pseudo("ext4", Path::new("/run/media/alice/backup")));
        assert!(is_pseudo("tmpfs", Path::new("/run/media")));
        assert!(is_pseudo("ext4", Path::new("/run/systemd")));
        // A directory that merely starts with the same letters is not /dev.
        assert!(!is_pseudo("ext4", Path::new("/development")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pseudo_mount_points_come_from_the_live_mount_table() {
        assert!(pseudo_mount_points().contains(&PathBuf::from("/proc")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_filesystem_mounted_over_an_autofs_placeholder_is_not_skipped() {
        let rows = vec![
            MountRow {
                device: "/dev/root".into(),
                mount_point: PathBuf::from("/"),
                fs_type: "ext4".into(),
                options: "rw".into(),
            },
            // systemd-style automount: an autofs placeholder mounted first...
            MountRow {
                device: "systemd-1".into(),
                mount_point: PathBuf::from("/boot"),
                fs_type: "autofs".into(),
                options: "rw".into(),
            },
            // ...and the real filesystem mounted on top of it once touched.
            MountRow {
                device: "/dev/sda1".into(),
                mount_point: PathBuf::from("/boot"),
                fs_type: "vfat".into(),
                options: "rw".into(),
            },
        ];

        assert_eq!(pseudo_mount_points_from_rows(rows), Vec::<PathBuf>::new());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pseudo_filesystem_mounted_over_a_real_one_is_still_skipped() {
        let rows = vec![
            MountRow {
                device: "/dev/root".into(),
                mount_point: PathBuf::from("/"),
                fs_type: "ext4".into(),
                options: "rw".into(),
            },
            MountRow {
                device: "/dev/sda1".into(),
                mount_point: PathBuf::from("/tmp"),
                fs_type: "ext4".into(),
                options: "rw".into(),
            },
            // /tmp remounted as tmpfs on top of the real filesystem.
            MountRow {
                device: "tmpfs".into(),
                mount_point: PathBuf::from("/tmp"),
                fs_type: "tmpfs".into(),
                options: "rw".into(),
            },
        ];

        assert_eq!(
            pseudo_mount_points_from_rows(rows),
            vec![PathBuf::from("/tmp")]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pseudo_mount_collection_keeps_later_rows_from_the_same_device() {
        let rows = vec![
            MountRow {
                device: "/dev/root".into(),
                mount_point: PathBuf::from("/"),
                fs_type: "ext4".into(),
                options: "rw".into(),
            },
            MountRow {
                device: "/dev/root".into(),
                mount_point: PathBuf::from("/snap/core/1"),
                fs_type: "ext4".into(),
                options: "rw,bind".into(),
            },
        ];

        assert_eq!(
            pseudo_mount_points_from_rows(rows),
            vec![PathBuf::from("/snap/core/1")]
        );
    }

    /// sysinfo lists neither devfs nor network shares; the kernel lists both.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_kernel_lists_mounts_sysinfo_leaves_out() {
        let mounts = kernel_mounts();

        assert!(mounts.contains(&("devfs".to_string(), PathBuf::from("/dev"))));
        assert!(mounts.iter().any(|(_, mount)| mount == Path::new("/")));
    }

    #[test]
    fn names_prefer_a_volume_label_over_a_device() {
        assert_eq!(
            friendly_name(Some("Macintosh HD"), "/dev/disk3s1s1", Path::new("/")),
            "Macintosh HD"
        );
        assert_eq!(
            friendly_name(Some("/dev/sda1"), "/dev/sda1", Path::new("/")),
            "Root"
        );
        assert_eq!(
            friendly_name(None, "/dev/sdb1", Path::new("/mnt/backup")),
            "backup"
        );
        // A fuse driver name is not a label.
        assert_eq!(
            friendly_name(
                Some("blobfuse2"),
                "blobfuse2",
                Path::new("/mnt/remote/session")
            ),
            "session"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_fields_are_unescaped() {
        assert_eq!(unescape_mount_field("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mount_field("/mnt/plain"), "/mnt/plain");
        assert_eq!(
            unescape_mount_field("/mnt/back\\134slash"),
            "/mnt/back\\slash"
        );
    }

    #[test]
    fn the_root_filesystem_is_discoverable() {
        let root = for_path(Path::new("/")).expect("every system has a root filesystem");
        assert!(root.total > 0);
        assert!(root.used_fraction() >= 0.0 && root.used_fraction() <= 1.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_disk_list_shows_the_startup_disk_as_one_writable_row() {
        let listed = list(false);

        assert!(
            !listed
                .iter()
                .any(|fs| fs.mount_point == Path::new("/System/Volumes/Data"))
        );
        let root = listed
            .iter()
            .find(|fs| fs.mount_point == Path::new("/"))
            .unwrap();
        assert!(!root.read_only);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_firmlinked_path_reports_the_data_volume_not_the_sealed_root() {
        let users = for_path(Path::new("/Users")).unwrap();
        let system = for_path(Path::new("/System")).unwrap();

        assert_eq!(users.mount_point, Path::new("/System/Volumes/Data"));
        assert!(!users.read_only);
        assert_eq!(system.mount_point, Path::new("/"));
        assert!(system.read_only, "the sealed system volume is read-only");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_mount_sysinfo_leaves_out_is_still_reported_for_its_paths() {
        let devfs = for_path(Path::new("/dev")).unwrap();

        assert_eq!(devfs.mount_point, Path::new("/dev"));
        assert_eq!(devfs.fs_type, "devfs");
        assert!(devfs.pseudo);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_non_utf8_mount_point_keeps_its_exact_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let name: &[u8] = b"/Volumes/caf\xe9";
        let mut raw: libc::statfs = unsafe { std::mem::zeroed() };
        for (slot, byte) in raw.f_mntonname.iter_mut().zip(name) {
            *slot = *byte as libc::c_char;
        }
        for (slot, byte) in raw.f_fstypename.iter_mut().zip(b"msdos") {
            *slot = *byte as libc::c_char;
        }

        let fs = unlisted_filesystem(&raw);

        assert_eq!(fs.mount_point.as_os_str().as_bytes(), name);
        assert_eq!(fs.fs_type, "msdos");
    }

    #[test]
    fn listing_hides_pseudo_filesystems_by_default() {
        let visible = list(false);
        let everything = list(true);
        assert!(visible.len() <= everything.len());
        assert!(visible.iter().all(|fs| !fs.pseudo));
    }
}
