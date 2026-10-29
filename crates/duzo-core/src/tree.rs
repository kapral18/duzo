//! The neutral data the scanner produces. No formatting, no colours — the TUI,
//! `--json` and any other consumer all read this same shape.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::size::SizeKind;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    Directory,
    File,
    Symlink,
    Other,
}

/// How much of the truth an entry's sizes represent.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanState {
    /// Fully walked.
    Complete,
    /// Walked, but something underneath was denied, cancelled or depth-capped.
    Partial,
    /// `read_dir` failed — almost always a permission error.
    Denied,
    /// The user stopped the scan before this subtree finished.
    Cancelled,
    /// Deliberately not descended into: another filesystem with `-x`, or a
    /// network or system mount.
    Skipped,
}

impl ScanState {
    /// Worst-of, so a single denied leaf marks its ancestors partial.
    fn merge(self, child: ScanState) -> ScanState {
        match (self, child) {
            (ScanState::Complete, ScanState::Complete) => ScanState::Complete,
            (ScanState::Cancelled, _) | (_, ScanState::Cancelled) => ScanState::Cancelled,
            (ScanState::Denied, _) => ScanState::Denied,
            _ => ScanState::Partial,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiskEntry {
    /// What this entry is called — and, on the root of a scan, the whole path
    /// it was given, since the root is the one entry with no parent to ask.
    ///
    /// Holding a full path on every entry means storing each directory's name
    /// again for every single file beneath it. On a home directory with two
    /// million entries in it that came to 481 MB of paths where the names
    /// alone are 75 MB. Anything needing a full path builds it on the way
    /// down, which is how the tree is walked anyway.
    ///
    /// Read it through [`DiskEntry::name_os`] rather than directly: that
    /// returns the last component either way, so the root and everything under
    /// it can be treated alike.
    name: Box<OsStr>,
    /// Sum of file lengths, as `ls` reports.
    pub apparent_size: u64,
    /// Blocks actually occupied, as `du` reports.
    pub allocated_size: u64,
    pub entry_type: EntryType,
    /// Number of filesystem entries in this subtree, including this one.
    pub items: u64,
    /// Newest modification time anywhere in this subtree, in seconds since the
    /// Unix epoch. This is what answers "when was this last actually used" —
    /// a 30 GB cache nobody has touched in four months is a very different
    /// proposition from one that is warm.
    modified: u64,
    /// This entry's modification time before child aggregates are folded in.
    own_modified: u64,
    pub children: Vec<DiskEntry>,
    scan_state: ScanState,
    /// This entry's scan result before child states are folded in.
    own_scan_state: ScanState,
    /// Filesystem identity captured by the scanner. It is runtime safety
    /// state, not part of snapshots or public JSON.
    filesystem_identity: Option<(u64, u64)>,
}

impl DiskEntry {
    /// Takes either a bare name, for an entry inside a scan, or a whole path,
    /// for the root of one. Both read back the same through [`Self::name_os`].
    pub fn new(name: impl Into<OsString>, entry_type: EntryType) -> Self {
        Self {
            name: name.into().into_boxed_os_str(),
            apparent_size: 0,
            allocated_size: 0,
            entry_type,
            items: 1,
            modified: 0,
            own_modified: 0,
            children: Vec::new(),
            scan_state: ScanState::Complete,
            own_scan_state: ScanState::Complete,
            filesystem_identity: None,
        }
    }

    /// The last component of what this entry stores.
    ///
    /// A bare name has no separators in it, so taking its file name gives it
    /// straight back; a root's whole path yields the directory it points at.
    /// That is what lets both be stored in the same field.
    pub fn name_os(&self) -> &OsStr {
        // `file_name` is `None` only for `/`, `.` and `..`, none of which
        // `read_dir` produces and none of which survive canonicalising a root.
        Path::new(&self.name).file_name().unwrap_or(&self.name)
    }

    /// The last path component, falling back to the whole path for roots like
    /// `/` which have no file name.
    pub fn name(&self) -> Cow<'_, str> {
        self.name_os().to_string_lossy()
    }

    /// The path this entry stands for — meaningful only on the root of a scan,
    /// which is the one entry that stores a whole path. Everything below it
    /// gets its path from the walk that reached it.
    pub fn root_path(&self) -> &Path {
        Path::new(&self.name)
    }

    /// This entry's own figures, with an empty child list.
    ///
    /// Anything building a reduced copy of a tree wants this rather than
    /// `clone()`: cloning deep-copies every descendant, so a caller that only
    /// means to keep a couple of levels still pays for the whole subtree first
    /// and then throws it away.
    pub fn without_children(&self) -> DiskEntry {
        DiskEntry {
            name: self.name.clone(),
            apparent_size: self.apparent_size,
            allocated_size: self.allocated_size,
            entry_type: self.entry_type,
            items: self.items,
            modified: self.modified,
            own_modified: self.own_modified,
            children: Vec::new(),
            scan_state: self.scan_state,
            own_scan_state: self.own_scan_state,
            filesystem_identity: self.filesystem_identity,
        }
    }

    pub fn modified(&self) -> u64 {
        self.modified
    }

    pub fn set_modified(&mut self, modified: u64) {
        self.own_modified = modified;
        self.modified = modified.max(
            self.children
                .iter()
                .map(|child| child.modified)
                .max()
                .unwrap_or(0),
        );
    }

    /// Whether this entry lists everything it holds: it counts no more than
    /// itself plus what its children count. A snapshot keeps only the larger entries, so
    /// a folder loaded from one can count more than it lists.
    pub fn lists_everything(&self) -> bool {
        let listed: u64 = self.children.iter().map(|child| child.items).sum();
        self.items <= listed.saturating_add(1)
    }

    pub fn scan_state(&self) -> ScanState {
        self.scan_state
    }

    pub fn set_scan_state(&mut self, scan_state: ScanState) {
        self.own_scan_state = scan_state;
        self.recompute_metadata();
    }

    pub fn filesystem_identity(&self) -> Option<(u64, u64)> {
        self.filesystem_identity
    }

    pub(crate) fn set_filesystem_identity(&mut self, identity: Option<(u64, u64)>) {
        self.filesystem_identity = identity;
    }

    pub fn size(&self, kind: SizeKind) -> u64 {
        match kind {
            SizeKind::Allocated => self.allocated_size,
            SizeKind::Apparent => self.apparent_size,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.entry_type == EntryType::Directory
    }

    /// Roll a finished child's totals into this entry.
    pub fn absorb(&mut self, child: &DiskEntry) {
        self.apparent_size += child.apparent_size;
        self.allocated_size += child.allocated_size;
        self.items += child.items;
        self.modified = self.modified.max(child.modified);
        self.scan_state = self.scan_state.merge(child.scan_state);
    }

    pub fn child(&self, name: &str) -> Option<&DiskEntry> {
        self.children.iter().find(|c| c.name() == name)
    }

    fn child_named(&self, name: &OsStr) -> Option<&DiskEntry> {
        self.children.iter().find(|c| c.name_os() == name)
    }

    /// `path` relative to this entry, or `None` when it lives elsewhere.
    fn relative_path<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(self.root_path()).ok()
    }

    /// The child names under this entry that lead to `path`, for the mutating
    /// walks that recurse on a slice. `resolve` runs every frame and iterates
    /// the components directly instead.
    fn relative_components<'a>(&self, path: &'a Path) -> Option<Vec<&'a OsStr>> {
        let relative = self.relative_path(path)?;
        Some(relative.components().map(|c| c.as_os_str()).collect())
    }

    /// Walk down to `path`, which must live under this entry. Returns `None`
    /// if any component is missing (a file that was deleted mid-session, or a
    /// subtree the depth limit pruned).
    pub fn resolve(&self, path: &Path) -> Option<&DiskEntry> {
        let relative = self.relative_path(path)?;
        let mut node = self;
        for component in relative.components() {
            node = node.child_named(component.as_os_str())?;
        }
        Some(node)
    }

    /// Remove `path` from this tree, subtracting its totals from every
    /// ancestor on the way back up.
    ///
    /// Lets the display correct itself the instant something is deleted,
    /// without paying for a full rescan of a tree that may have taken minutes
    /// to build.
    pub fn remove(&mut self, path: &Path) -> Option<DiskEntry> {
        // A tree cannot remove itself, and nothing outside it is its business.
        let names = self.relative_components(path)?;
        if names.is_empty() {
            return None;
        }
        self.remove_named(&names)
    }

    fn remove_named(&mut self, names: &[&OsStr]) -> Option<DiskEntry> {
        let (name, rest) = names.split_first()?;
        let index = self.children.iter().position(|c| c.name_os() == *name)?;

        let removed = if rest.is_empty() {
            self.children.remove(index)
        } else {
            // Nothing is subtracted unless the whole path was there to remove.
            self.children[index].remove_named(rest)?
        };
        self.subtract(&removed);
        Some(removed)
    }

    fn subtract(&mut self, child: &DiskEntry) {
        self.apparent_size = self.apparent_size.saturating_sub(child.apparent_size);
        self.allocated_size = self.allocated_size.saturating_sub(child.allocated_size);
        self.items = self.items.saturating_sub(child.items);
        self.recompute_metadata();
    }

    fn recompute_metadata(&mut self) {
        self.modified = self
            .children
            .iter()
            .map(|child| child.modified)
            .fold(self.own_modified, u64::max);
        self.scan_state = self
            .children
            .iter()
            .fold(self.own_scan_state, |state, child| {
                state.merge(child.scan_state)
            });
    }

    /// Graft `subtree` into this tree at `path`, adjusting the totals of every
    /// ancestor along the way.
    ///
    /// Hard-link dedup is per scan, so a file linked from both inside and
    /// outside `subtree` that the original scan credited elsewhere is counted
    /// again here; ancestor totals can exceed the disk by that much until the
    /// next full rescan. Carrying the original inode set over would not fix it:
    /// links the original scan credited *inside* the subtree would then be
    /// dropped instead.
    ///
    /// Hands back the entry it replaced, or `subtree` itself when `path` is
    /// not in this tree, so the caller decides where a big tree is freed.
    pub fn graft(&mut self, path: &Path, subtree: DiskEntry) -> Result<DiskEntry, DiskEntry> {
        let Some(names) = self.relative_components(path) else {
            return Err(subtree);
        };
        if names.is_empty() {
            return Ok(std::mem::replace(self, subtree));
        }
        self.graft_named(&names, subtree)
    }

    fn graft_named(
        &mut self,
        names: &[&OsStr],
        mut subtree: DiskEntry,
    ) -> Result<DiskEntry, DiskEntry> {
        let Some((name, rest)) = names.split_first() else {
            return Err(subtree);
        };
        let Some(index) = self.children.iter().position(|c| c.name_os() == *name) else {
            return Err(subtree);
        };

        // A folder painted from a pruned snapshot holds more than it lists.
        // What it leaves out still counts toward its newest mtime and its
        // scan state, which only the old aggregate knows, so the new child is
        // folded into that instead of recomputing from the listed children.
        // The replaced child's old share stays in it too, until the full scan
        // replaces the tree.
        let lists_everything = self.lists_everything();

        let child = &mut self.children[index];
        let (old_apparent, old_allocated, old_items) =
            (child.apparent_size, child.allocated_size, child.items);
        let replaced = if rest.is_empty() {
            subtree.name = child.name.clone();
            std::mem::replace(child, subtree)
        } else {
            child.graft_named(rest, subtree)?
        };

        let (apparent, allocated, items) = (child.apparent_size, child.allocated_size, child.items);
        self.apparent_size = self
            .apparent_size
            .saturating_sub(old_apparent)
            .saturating_add(apparent);
        self.allocated_size = self
            .allocated_size
            .saturating_sub(old_allocated)
            .saturating_add(allocated);
        self.items = self.items.saturating_sub(old_items).saturating_add(items);
        if lists_everything {
            self.recompute_metadata();
        } else {
            let (modified, scan_state) = (child.modified, child.scan_state);
            self.modified = self.modified.max(modified);
            self.scan_state = self.scan_state.merge(scan_state);
        }
        Ok(replaced)
    }

    /// Sorts every level largest-first. The TUI re-sorts by other keys itself;
    /// this is the ordering `--json` and `--plain` ship with.
    pub fn sort_by_size(&mut self, kind: SizeKind) {
        self.children.sort_by(|a, b| {
            b.size(kind)
                .cmp(&a.size(kind))
                .then_with(|| a.name().cmp(&b.name()))
        });
        for child in &mut self.children {
            child.sort_by_size(kind);
        }
    }

    /// Every descendant exactly `depth` levels below this entry, each with the
    /// path that reached it — built on the way down, since no entry below the
    /// root carries one.
    ///
    /// Used for the "Largest items" list: children answer *what* is big, but
    /// grandchildren answer *where to look next* — `~/Downloads` is actionable
    /// in a way that `Users` is not.
    pub fn descendants_at_depth(&self, base: &Path, depth: usize) -> Vec<(PathBuf, &DiskEntry)> {
        let mut out = Vec::new();
        self.collect_at_depth(base, depth, &mut out);
        out
    }

    fn collect_at_depth<'a>(
        &'a self,
        base: &Path,
        depth: usize,
        out: &mut Vec<(PathBuf, &'a DiskEntry)>,
    ) {
        if depth == 0 {
            out.push((base.to_path_buf(), self));
            return;
        }
        for child in &self.children {
            child.collect_at_depth(&base.join(child.name_os()), depth - 1, out);
        }
    }

    /// The `n` biggest entries `depth` levels down, largest first. Falls back
    /// to shallower levels when the tree is not that deep.
    pub fn largest_at_depth(
        &self,
        base: &Path,
        depth: usize,
        n: usize,
        kind: SizeKind,
    ) -> Vec<(PathBuf, &DiskEntry)> {
        let mut candidates = self.descendants_at_depth(base, depth);
        for shallower in (1..depth).rev() {
            if !candidates.is_empty() {
                break;
            }
            candidates = self.descendants_at_depth(base, shallower);
        }
        candidates.sort_by_key(|(_, entry)| std::cmp::Reverse(entry.size(kind)));
        candidates.truncate(n);
        candidates
    }
}

pub use wire::PublicTree;

/// Every format duzo publishes — `--json` and the snapshots behind `diff` —
/// gives each entry its full path, and did so before entries stopped carrying
/// one. Serialising rebuilds those paths on the way down. The private aggregate
/// inputs are additive fields; their defaults keep older snapshots readable.
mod wire {
    use super::*;
    use serde::de::Deserializer;
    use serde::ser::{SerializeSeq, SerializeStruct, Serializer};

    /// An entry together with the path that reached it. `public` leaves out
    /// the private aggregate inputs; see [`PublicTree`].
    struct Pathed<'a> {
        path: &'a Path,
        entry: &'a DiskEntry,
        public: bool,
    }

    struct Children<'a> {
        base: &'a Path,
        entries: &'a [DiskEntry],
        public: bool,
    }

    impl Serialize for Children<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut seq = serializer.serialize_seq(Some(self.entries.len()))?;
            for entry in self.entries {
                // Transient: one path at a time, dropped before the next, so
                // writing a two-million entry tree still holds only one copy.
                let path = self.base.join(entry.name_os());
                seq.serialize_element(&Pathed {
                    path: &path,
                    entry,
                    public: self.public,
                })?;
            }
            seq.end()
        }
    }

    impl Serialize for Pathed<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let entry = self.entry;
            let field_count = if self.public { 8 } else { 10 };
            let mut out = serializer.serialize_struct("DiskEntry", field_count)?;
            out.serialize_field("path", self.path)?;
            out.serialize_field("apparent_size", &entry.apparent_size)?;
            out.serialize_field("allocated_size", &entry.allocated_size)?;
            out.serialize_field("entry_type", &entry.entry_type)?;
            out.serialize_field("items", &entry.items)?;
            out.serialize_field("modified", &entry.modified)?;
            if !self.public {
                out.serialize_field("own_modified", &entry.own_modified)?;
            }
            out.serialize_field(
                "children",
                &Children {
                    base: self.path,
                    entries: &entry.children,
                    public: self.public,
                },
            )?;
            out.serialize_field("scan_state", &entry.scan_state)?;
            if !self.public {
                out.serialize_field("own_scan_state", &entry.own_scan_state)?;
            }
            out.end()
        }
    }

    /// Only ever the root of a tree is serialised — a snapshot's tree, or the
    /// root of a `--json` scan — and the root is the entry that knows its own
    /// whole path.
    impl Serialize for DiskEntry {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            Pathed {
                path: self.root_path(),
                entry: self,
                public: false,
            }
            .serialize(serializer)
        }
    }

    /// The same tree, for the formats a person reads (`duzo --json`,
    /// `duzo history --json`). It leaves out `own_modified` and
    /// `own_scan_state`, which only exist so `remove` and `graft` can
    /// recompute aggregates. Snapshots go through [`DiskEntry`]'s own
    /// `Serialize` impl, which keeps them.
    pub struct PublicTree<'a>(pub &'a DiskEntry);

    impl Serialize for PublicTree<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            Pathed {
                path: self.0.root_path(),
                entry: self.0,
                public: true,
            }
            .serialize(serializer)
        }
    }

    #[derive(Deserialize)]
    struct Wire {
        path: PathBuf,
        apparent_size: u64,
        allocated_size: u64,
        entry_type: EntryType,
        items: u64,
        #[serde(default)]
        modified: u64,
        #[serde(default)]
        own_modified: Option<u64>,
        children: Vec<Wire>,
        scan_state: ScanState,
        #[serde(default)]
        own_scan_state: Option<ScanState>,
    }

    impl Wire {
        fn into_entry(self, is_root: bool) -> DiskEntry {
            // The root keeps the whole path it was written with; everything
            // below it needs only the last component back.
            let name: OsString = match self.path.file_name() {
                Some(name) if !is_root => name.to_os_string(),
                _ => self.path.into_os_string(),
            };
            let children: Vec<DiskEntry> = self
                .children
                .into_iter()
                .map(|child| child.into_entry(false))
                .collect();
            // Snapshots written before `own_modified` stored only the subtree
            // maximum, which is never older than any child. Taking it as the
            // directory's own keeps the recorded aggregate exactly; the
            // directory's real mtime is not in the file.
            let own_modified = self.own_modified.unwrap_or(self.modified);
            let own_scan_state = self.own_scan_state.unwrap_or({
                if children.is_empty() {
                    self.scan_state
                } else {
                    ScanState::Complete
                }
            });
            DiskEntry {
                name: name.into_boxed_os_str(),
                apparent_size: self.apparent_size,
                allocated_size: self.allocated_size,
                entry_type: self.entry_type,
                items: self.items,
                modified: self.modified,
                own_modified,
                children,
                scan_state: self.scan_state,
                own_scan_state,
                filesystem_identity: None,
            }
        }
    }

    impl<'de> Deserialize<'de> for DiskEntry {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            Ok(Wire::deserialize(deserializer)?.into_entry(true))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &str, size: u64, children: Vec<DiskEntry>) -> DiskEntry {
        let mut entry = DiskEntry::new(PathBuf::from(path), EntryType::Directory);
        entry.allocated_size = size;
        entry.apparent_size = size;
        entry.children = children;
        entry
    }

    fn tree() -> DiskEntry {
        dir(
            "/root",
            100,
            vec![
                dir("/root/a", 70, vec![dir("/root/a/deep", 65, vec![])]),
                dir("/root/b", 30, vec![dir("/root/b/deep", 10, vec![])]),
            ],
        )
    }

    #[test]
    fn resolves_nested_paths() {
        let root = tree();
        assert_eq!(
            root.resolve(Path::new("/root/a/deep")).unwrap().name(),
            "deep"
        );
        assert_eq!(root.resolve(Path::new("/root")).unwrap().name(), "root");
        assert!(root.resolve(Path::new("/root/missing")).is_none());
        assert!(root.resolve(Path::new("/elsewhere")).is_none());
    }

    #[test]
    fn largest_at_depth_looks_past_direct_children() {
        let root = tree();
        let largest = root.largest_at_depth(Path::new("/root"), 2, 2, SizeKind::Allocated);
        let names: Vec<_> = largest
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect();
        assert_eq!(names, vec!["/root/a/deep", "/root/b/deep"]);
    }

    #[test]
    fn largest_at_depth_falls_back_when_tree_is_shallow() {
        let root = dir("/root", 10, vec![dir("/root/a", 5, vec![])]);
        let largest = root.largest_at_depth(Path::new("/root"), 3, 5, SizeKind::Allocated);
        assert_eq!(largest.len(), 1);
        assert_eq!(largest[0].1.name(), "a");
    }

    #[test]
    fn removing_an_entry_corrects_every_ancestor() {
        let mut root = tree();
        let before = root.allocated_size;

        let removed = root.remove(Path::new("/root/a/deep")).unwrap();

        assert_eq!(removed.allocated_size, 65);
        assert_eq!(root.allocated_size, before - 65);
        assert_eq!(root.child("a").unwrap().allocated_size, 70 - 65);
        assert!(root.resolve(Path::new("/root/a/deep")).is_none());
    }

    #[test]
    fn removing_a_whole_branch_takes_its_children_with_it() {
        let mut root = tree();
        let removed = root.remove(Path::new("/root/a")).unwrap();

        assert_eq!(removed.children.len(), 1);
        assert_eq!(root.allocated_size, 100 - 70);
        assert!(root.child("a").is_none());
    }

    #[test]
    fn a_tree_refuses_to_remove_itself_or_a_stranger() {
        let mut root = tree();
        assert!(root.remove(Path::new("/root")).is_none());
        assert!(root.remove(Path::new("/elsewhere")).is_none());
        assert!(root.remove(Path::new("/root/missing")).is_none());
        assert_eq!(
            root.allocated_size, 100,
            "nothing should have been subtracted"
        );
    }

    #[test]
    fn a_copy_without_children_keeps_every_figure() {
        let root = tree();
        let bare = root.child("a").unwrap().without_children();

        assert_eq!(bare.name_os(), "a");
        assert_eq!(bare.allocated_size, 70);
        assert_eq!(bare.apparent_size, 70);
        assert_eq!(bare.entry_type, EntryType::Directory);
        // The subtree is what is left behind, and only the subtree.
        assert!(bare.children.is_empty());
    }

    /// `/root` -> `a` -> `deep.txt`, built the way a scan builds one: the root
    /// with a whole path, everything under it with a name.
    fn named_tree() -> DiskEntry {
        let mut root = DiskEntry::new("/root", EntryType::Directory);
        let mut a = DiskEntry::new("a", EntryType::Directory);
        let mut leaf = DiskEntry::new("deep.txt", EntryType::File);
        leaf.allocated_size = 40;
        a.allocated_size = 40;
        a.children.push(leaf);
        root.allocated_size = 40;
        root.children.push(a);
        root
    }

    #[test]
    fn the_wire_format_still_gives_every_entry_its_full_path() {
        let json: serde_json::Value = serde_json::to_value(named_tree()).unwrap();

        // `--json` and every snapshot ever written say `path`, and say it in
        // full. Entries stopped carrying one; the output must not notice.
        assert_eq!(json["path"], "/root");
        assert_eq!(json["children"][0]["path"], "/root/a");
        assert_eq!(
            json["children"][0]["children"][0]["path"],
            "/root/a/deep.txt"
        );
        assert_eq!(json["allocated_size"], 40);
        assert_eq!(json["entry_type"], "directory");
        assert_eq!(json["children"][0]["children"][0]["entry_type"], "file");
        assert_eq!(json["own_modified"], 0);
        assert_eq!(json["own_scan_state"], "complete");
    }

    #[test]
    fn public_tree_drops_the_private_aggregate_fields() {
        let json: serde_json::Value = serde_json::to_value(PublicTree(&named_tree())).unwrap();

        // The public view (`duzo --json`) still carries every path and size...
        assert_eq!(json["path"], "/root");
        assert_eq!(json["children"][0]["path"], "/root/a");
        assert_eq!(json["allocated_size"], 40);
        // ...but not the internal aggregate inputs, at any depth.
        assert!(json.get("own_modified").is_none());
        assert!(json.get("own_scan_state").is_none());
        assert!(json["children"][0].get("own_modified").is_none());
        assert!(json["children"][0].get("own_scan_state").is_none());
    }

    #[test]
    fn a_tree_survives_a_round_trip_through_the_wire_format() {
        let json = serde_json::to_string(&named_tree()).unwrap();
        let back: DiskEntry = serde_json::from_str(&json).unwrap();

        assert_eq!(back.root_path(), Path::new("/root"));
        // The root got its whole path back and the rest got their names.
        assert_eq!(back.child("a").unwrap().name_os(), "a");
        assert_eq!(
            back.resolve(Path::new("/root/a/deep.txt")).unwrap().name(),
            "deep.txt"
        );
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    #[test]
    fn a_round_trip_keeps_the_metadata_needed_for_later_removal() {
        // The root's own mtime is older than its child's, so only a wire that
        // keeps the root's own value can give it back after the removal.
        let mut root = DiskEntry::new("/root", EntryType::Directory);
        root.set_modified(5);
        root.set_scan_state(ScanState::Complete);
        let mut child = DiskEntry::new("child", EntryType::Directory);
        child.set_modified(10);
        child.set_scan_state(ScanState::Denied);
        root.absorb(&child);
        root.children.push(child);

        let json = serde_json::to_string(&root).unwrap();
        let mut back: DiskEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.modified(), 10);
        back.remove(Path::new("/root/child")).unwrap();

        assert_eq!(back.modified(), 5);
        assert_eq!(back.scan_state(), ScanState::Complete);
    }

    #[test]
    fn a_round_trip_keeps_a_parents_own_incomplete_scan_state() {
        let mut root = DiskEntry::new("/root", EntryType::Directory);
        root.set_scan_state(ScanState::Denied);
        let child = DiskEntry::new("child", EntryType::Directory);
        root.absorb(&child);
        root.children.push(child);

        let json = serde_json::to_string(&root).unwrap();
        let mut back: DiskEntry = serde_json::from_str(&json).unwrap();
        back.remove(Path::new("/root/child")).unwrap();

        assert_eq!(back.scan_state(), ScanState::Denied);
    }

    #[test]
    fn a_snapshot_written_by_an_older_duzo_still_loads() {
        // Verbatim shape of a stored tree from before entries dropped their
        // paths, down to `modified` being absent.
        let stored = r#"{
            "path": "/root",
            "apparent_size": 40,
            "allocated_size": 40,
            "entry_type": "directory",
            "items": 3,
            "children": [
                {
                    "path": "/root/a",
                    "apparent_size": 40,
                    "allocated_size": 40,
                    "entry_type": "directory",
                    "items": 2,
                    "children": [],
                    "scan_state": "complete"
                }
            ],
            "scan_state": "complete"
        }"#;

        let tree: DiskEntry = serde_json::from_str(stored).unwrap();
        assert_eq!(tree.root_path(), Path::new("/root"));
        assert_eq!(tree.allocated_size, 40);
        assert_eq!(tree.modified(), 0);
        assert_eq!(tree.resolve(Path::new("/root/a")).unwrap().items, 2);
    }

    #[test]
    fn an_old_snapshot_keeps_parent_scan_state_separate_from_children() {
        let stored = r#"{
            "path": "/root",
            "apparent_size": 40,
            "allocated_size": 40,
            "entry_type": "directory",
            "items": 2,
            "modified": 10,
            "children": [
                {
                    "path": "/root/denied",
                    "apparent_size": 0,
                    "allocated_size": 0,
                    "entry_type": "directory",
                    "items": 1,
                    "modified": 0,
                    "children": [],
                    "scan_state": "denied"
                }
            ],
            "scan_state": "partial"
        }"#;

        let mut tree: DiskEntry = serde_json::from_str(stored).unwrap();
        tree.remove(Path::new("/root/denied")).unwrap();

        assert_eq!(tree.modified(), 10);
        assert_eq!(tree.scan_state(), ScanState::Complete);
    }

    #[test]
    fn scan_state_degrades_to_partial() {
        let mut parent = DiskEntry::new(PathBuf::from("/p"), EntryType::Directory);
        let mut denied = DiskEntry::new(PathBuf::from("/p/c"), EntryType::Directory);
        denied.set_scan_state(ScanState::Denied);
        parent.absorb(&denied);
        assert_eq!(parent.scan_state(), ScanState::Partial);
    }

    #[test]
    fn graft_replaces_subtree_and_updates_ancestor_totals() {
        let mut tree = DiskEntry::new(PathBuf::from("/root"), EntryType::Directory);
        let mut sub = DiskEntry::new("sub", EntryType::Directory);
        sub.allocated_size = 50;
        sub.apparent_size = 50;
        sub.items = 1;
        tree.children.push(sub);
        tree.allocated_size = 50;
        tree.apparent_size = 50;
        tree.items = 2;

        let mut deep_sub = DiskEntry::new(PathBuf::from("/root/sub"), EntryType::Directory);
        let mut file1 = DiskEntry::new("file1", EntryType::File);
        file1.allocated_size = 80;
        file1.apparent_size = 80;
        file1.items = 1;
        deep_sub.children.push(file1);
        deep_sub.allocated_size = 80;
        deep_sub.apparent_size = 80;
        deep_sub.items = 2;

        assert!(tree.graft(Path::new("/root/sub"), deep_sub).is_ok());
        assert_eq!(tree.allocated_size, 80);
        assert_eq!(tree.apparent_size, 80);
        assert_eq!(tree.items, 3);
        assert_eq!(
            tree.resolve(Path::new("/root/sub/file1"))
                .unwrap()
                .allocated_size,
            80
        );
    }

    #[test]
    fn graft_recomputes_ancestor_metadata_from_current_children() {
        let mut root = DiskEntry::new("/root", EntryType::Directory);
        root.set_modified(10);
        let mut old = DiskEntry::new("child", EntryType::Directory);
        old.set_modified(100);
        old.set_scan_state(ScanState::Denied);
        root.absorb(&old);
        root.children.push(old);

        let mut replacement = DiskEntry::new("child", EntryType::Directory);
        replacement.set_modified(20);

        assert!(root.graft(Path::new("/root/child"), replacement).is_ok());
        assert_eq!(root.modified(), 20);
        assert_eq!(root.scan_state(), ScanState::Complete);
    }

    /// A folder painted from a pruned snapshot lists less than it holds. A
    /// graft must not let what was pruned stop counting toward its parent.
    #[test]
    fn graft_into_a_folder_listed_in_part_keeps_what_it_does_not_list() {
        let mut root = DiskEntry::new("/root", EntryType::Directory);
        let mut kept = DiskEntry::new("kept", EntryType::Directory);
        kept.set_modified(20);
        let mut pruned = DiskEntry::new("pruned", EntryType::Directory);
        pruned.set_modified(999);
        pruned.set_scan_state(ScanState::Denied);
        root.absorb(&kept);
        root.absorb(&pruned);
        root.children.push(kept);
        root.children.push(pruned);
        root.recompute_metadata();
        // What a snapshot that dropped the small folder would load back.
        root.children.pop();

        let mut fresh = DiskEntry::new("kept", EntryType::Directory);
        fresh.set_modified(30);
        assert!(root.graft(Path::new("/root/kept"), fresh).is_ok());

        assert_eq!(root.modified(), 999);
        assert_eq!(root.scan_state(), ScanState::Partial);
    }

    #[test]
    fn graft_totals_saturate_without_signed_wraparound() {
        let mut root = DiskEntry::new("/root", EntryType::Directory);
        let mut child = DiskEntry::new("child", EntryType::Directory);
        child.children.push(DiskEntry::new("leaf", EntryType::File));
        child.allocated_size = 1;
        child.apparent_size = 1;
        child.items = 2;
        root.children.push(child);
        root.allocated_size = 2;
        root.apparent_size = 2;
        root.items = 3;

        let mut replacement = DiskEntry::new("leaf", EntryType::File);
        replacement.allocated_size = u64::MAX;
        replacement.apparent_size = u64::MAX;
        replacement.items = u64::MAX;

        assert!(
            root.graft(Path::new("/root/child/leaf"), replacement)
                .is_ok()
        );
        assert_eq!(root.allocated_size, u64::MAX);
        assert_eq!(root.apparent_size, u64::MAX);
        assert_eq!(root.items, u64::MAX);
    }
}
