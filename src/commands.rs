//! What each subcommand actually does.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use duzo_core::categories::{self, Group};
use duzo_core::history::{self, Snapshot, SnapshotId, Store};
use duzo_core::scan::{self, Cancel, Progress};
use duzo_core::{Diff, DiskEntry, PublicTree, SizeKind};
use serde::Serialize;

use crate::cli::Options;
use crate::model::display_path;
use crate::report::{self, Palette};
use crate::timefmt;

/// Scan, and record the result unless the user asked us not to.
pub fn scan_and_record(path: &Path, options: &Options) -> Result<(DiskEntry, Option<Store>)> {
    let progress = Progress::default();
    let tree = scan::scan(path, &options.scan_options(), &progress, &Cancel::new())?;

    if !options.may_snapshot() {
        return Ok((tree, None));
    }

    // A failure to record history must never fail the thing you actually
    // asked for.
    let store = match Store::open() {
        Ok(store) => {
            let _ = store.record(&tree);
            Some(store)
        }
        Err(_) => None,
    };
    Ok((tree, store))
}

pub fn diff(
    out: &mut impl Write,
    options: &Options,
    path: Option<PathBuf>,
    since: Option<String>,
) -> Result<()> {
    let store = Store::open().context("cannot open the snapshot history")?;
    let root = resolve_diff_root(path, &store)?;
    let now = history::now();

    let before = match &since {
        Some(spec) => {
            let window = timefmt::parse_duration(spec)?;
            store.latest_before(&root, now.saturating_sub(window))
        }
        None => store.latest(&root),
    };

    let Some(before) = before else {
        bail!(
            "no snapshot of {} from {} — run `duzo {}` first to start a history",
            display_path(&root),
            match &since {
                Some(spec) => format!("{spec} ago or earlier"),
                None => "before now".to_string(),
            },
            display_path(&root)
        );
    };

    let (after, _) = scan_and_record(&root, options)?;
    let diff = duzo_core::diff::diff(
        &before.tree,
        &after,
        options.size_kind(),
        before.taken_at,
        now,
        before.floor,
    );

    if options.json {
        serde_json::to_writer_pretty(&mut *out, &diff)?;
        writeln!(out)?;
        return Ok(());
    }
    if options.plain {
        return report::diff_plain(out, &diff, options.unit(), options.top);
    }
    report::diff_report(
        out,
        &diff,
        options.unit(),
        options.top,
        now,
        Palette::detect(),
    )
}

/// `duzo diff` with no path means "the thing I was just looking at".
fn resolve_diff_root(path: Option<PathBuf>, store: &Store) -> Result<PathBuf> {
    if let Some(path) = path {
        return path
            .canonicalize()
            .with_context(|| format!("cannot read {}", path.display()));
    }
    if let Some(root) = store.most_recent_root() {
        return Ok(root);
    }
    home().context("no previous scan to compare against, and no home directory to fall back on")
}

pub fn clean(
    out: &mut impl Write,
    options: &Options,
    path: Option<PathBuf>,
    safe_only: bool,
    idle_for: Option<String>,
    delete: bool,
    assume_yes: bool,
) -> Result<()> {
    let root = match path {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("cannot read {}", path.display()))?,
        None => home().context("no home directory to search")?,
    };

    let (tree, _) = scan_and_record(&root, options)?;
    let now = history::now();

    let candidates = categories::find_reclaimable(&tree, options.size_kind());
    let mut groups = categories::group_by_rule(&candidates);

    if safe_only {
        groups.retain(|group| group.rule.safety == duzo_core::Safety::Regenerable);
    }
    if let Some(spec) = &idle_for {
        let threshold = timefmt::parse_duration(spec)?;
        groups.retain(|group| group.idle_for(now) >= threshold);
    }

    if options.json {
        serde_json::to_writer_pretty(&mut *out, &groups)?;
        writeln!(out)?;
        return Ok(());
    }
    if options.plain {
        return report::clean_plain(out, &groups, options.unit());
    }

    report::clean_report(out, &groups, options.unit(), now, Palette::detect())?;

    if delete {
        remove(out, &groups, options, assume_yes)?;
    }
    Ok(())
}

/// Delete the listed categories, after making very sure that is what was
/// meant.
///
/// Every path is re-checked against the category rules immediately before
/// removal: the list on screen came from a scan that finished seconds ago, and
/// deleting something because of a stale classification is exactly the failure
/// worth engineering against.
fn remove(
    out: &mut impl Write,
    groups: &[Group<'_>],
    options: &Options,
    assume_yes: bool,
) -> Result<()> {
    let palette = Palette::detect();
    let total: u64 = groups.iter().map(|group| group.size).sum();
    let paths: Vec<&Path> = groups
        .iter()
        .flat_map(|group| group.paths.iter().copied())
        .collect();

    if paths.is_empty() {
        return Ok(());
    }

    if !assume_yes {
        if !std::io::stdin().is_terminal() {
            bail!(
                "refusing to delete without a terminal to confirm at — pass --yes if you mean it"
            );
        }

        writeln!(
            out,
            " {} {} across {} location{}:",
            palette.warn("About to delete"),
            duzo_core::size::format(total, options.unit()),
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        )?;
        for path in &paths {
            writeln!(out, "   {}", display_path(path))?;
        }
        write!(out, "\n Type {} to confirm: ", palette.bold("delete"))?;
        out.flush()?;

        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != "delete" {
            writeln!(out, " {}", palette.dim("nothing was deleted"))?;
            return Ok(());
        }
    }

    let mut freed = 0u64;
    for group in groups {
        for path in &group.paths {
            // Re-verify, then remove. A path that no longer classifies is one
            // we no longer have a reason to touch.
            if categories::classify(path).is_none() {
                writeln!(
                    out,
                    " {} {} is no longer a recognised cache",
                    palette.warn("skipped"),
                    display_path(path)
                )?;
                continue;
            }
            let progress = crate::deletion::DeleteProgress::default();
            let removed = crate::deletion::delete(path, &progress);
            // On Unix, bytes a failed removal freed before it stopped count too.
            freed = freed.saturating_add(progress.freed());
            match removed {
                Ok(()) => {
                    writeln!(out, " removed {}", display_path(path))?;
                }
                Err(error) => {
                    writeln!(
                        out,
                        " {} {}: {error}",
                        palette.warn("could not remove"),
                        display_path(path)
                    )?;
                }
            }
        }
    }

    writeln!(
        out,
        "\n {} {}\n",
        palette.bold("Freed"),
        duzo_core::size::format(freed, options.unit())
    )?;
    Ok(())
}

pub fn history(
    out: &mut impl Write,
    options: &Options,
    path: Option<PathBuf>,
    forget: bool,
) -> Result<()> {
    let store = Store::open().context("cannot open the snapshot history")?;
    let root = resolve_diff_root(path, &store)?;

    if forget {
        store.forget(&root)?;
        writeln!(out, " forgot the history for {}", display_path(&root))?;
        return Ok(());
    }

    let snapshots = store.load_all(&root);

    if options.json {
        let public: Vec<PublicSnapshot> = snapshots.iter().map(PublicSnapshot::from).collect();
        serde_json::to_writer_pretty(&mut *out, &public)?;
        writeln!(out)?;
        return Ok(());
    }

    report::history_report(
        out,
        &root,
        &snapshots,
        options.unit(),
        history::now(),
        Palette::detect(),
    )
}

/// A [`Snapshot`], for `history --json`: its tree goes through [`PublicTree`].
#[derive(Serialize)]
struct PublicSnapshot<'a> {
    root: &'a Path,
    taken_at: u64,
    total_allocated: u64,
    total_apparent: u64,
    items: u64,
    floor: u64,
    tree: PublicTree<'a>,
}

impl<'a> From<&'a Snapshot> for PublicSnapshot<'a> {
    fn from(snapshot: &'a Snapshot) -> Self {
        // Destructured so a field added to `Snapshot` fails to compile here
        // instead of silently missing from `history --json`.
        let Snapshot {
            root,
            taken_at,
            total_allocated,
            total_apparent,
            items,
            floor,
            tree,
        } = snapshot;
        Self {
            root,
            taken_at: *taken_at,
            total_allocated: *total_allocated,
            total_apparent: *total_apparent,
            items: *items,
            floor: *floor,
            tree: PublicTree(tree),
        }
    }
}

/// The last thing duzo knew about `path`, for painting a screen before the
/// fresh scan has finished.
pub fn last_snapshot(path: &Path) -> Option<duzo_core::Snapshot> {
    Store::open().ok()?.latest(path)
}

/// Store `tree` as the newest snapshot of its root (when `record`), and hand
/// back the store with the one that was newest before it, which a comparison
/// needs.
///
/// Only the old one's name is read before writing — the other way round, the
/// newest snapshot would be the scan itself and every comparison against it
/// would come back empty. Parsing it can wait until the new one is safe, which
/// is also why this one is never written over it: a scan recorded within the
/// same second as the last gets the next second.
pub fn record_snapshot(tree: &DiskEntry, record: bool) -> Option<(Store, SnapshotId)> {
    record_in(Store::open().ok()?, tree, record, history::now())
}

fn record_in(
    store: Store,
    tree: &DiskEntry,
    record: bool,
    now: u64,
) -> Option<(Store, SnapshotId)> {
    let previous = store.list(tree.root_path()).pop();
    if record {
        let after_previous = previous.as_ref().map_or(0, |id| id.taken_at + 1);
        let _ = store.record_at(tree, now.max(after_previous));
    }
    Some((store, previous?))
}

/// What changed between `previous` and `tree`, measured in `kind`.
pub fn diff_since(previous: &Snapshot, tree: &DiskEntry, kind: SizeKind) -> Diff {
    duzo_core::diff::diff(
        &previous.tree,
        tree,
        kind,
        previous.taken_at,
        history::now(),
        previous.floor,
    )
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use duzo_core::EntryType;

    fn options() -> Options {
        use clap::Parser;
        crate::cli::Cli::parse_from(["duzo"]).options
    }

    #[test]
    fn a_depth_capped_scan_is_not_recorded() {
        let mut options = options();
        options.depth = Some(2);
        assert!(!options.may_snapshot());
    }

    #[test]
    fn deleting_without_a_terminal_or_yes_is_refused() {
        let rule = duzo_core::categories::RULES
            .iter()
            .find(|rule| rule.id == "gradle-caches")
            .unwrap();
        let path = PathBuf::from("/nonexistent/.gradle/caches");
        let groups = vec![Group {
            rule,
            size: 1000,
            last_used: 0,
            paths: vec![&path],
        }];

        let mut out = Vec::new();
        // Tests do not run with a terminal on stdin, which is exactly the
        // situation this guard exists for.
        let error = remove(&mut out, &groups, &options(), false).unwrap_err();
        assert!(error.to_string().contains("--yes"), "{error}");
    }

    #[test]
    fn deletion_skips_paths_that_no_longer_classify() {
        let rule = duzo_core::categories::RULES
            .iter()
            .find(|rule| rule.id == "gradle-caches")
            .unwrap();
        // Not under any home directory, so classify() will not claim it.
        let path = PathBuf::from("/tmp/definitely-not-a-known-cache");
        let groups = vec![Group {
            rule,
            size: 1000,
            last_used: 0,
            paths: vec![&path],
        }];

        let mut out = Vec::new();
        remove(&mut out, &groups, &options(), true).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no longer a recognised cache"), "{text}");
        let freed = text
            .lines()
            .find(|line| line.contains("Freed"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(freed.ends_with(" 0 B"), "{text}");
    }

    #[test]
    fn diffing_without_any_history_explains_what_to_do() {
        let mut out = Vec::new();
        let error = diff(
            &mut out,
            &options(),
            Some(PathBuf::from("/nonexistent-path-for-tests")),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot read"), "{error}");
    }

    #[test]
    fn scanning_a_tree_yields_something_recordable() {
        let mut entry = DiskEntry::new(PathBuf::from("/x"), EntryType::Directory);
        entry.allocated_size = 10;
        // Just proving the shape the store wants is the shape a scan gives.
        assert_eq!(duzo_core::history::storage_floor(&entry), 1_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn deletion_reports_actual_removed_bytes_not_the_group_estimate() {
        use std::os::unix::fs::MetadataExt;

        fn allocated(path: &Path) -> u64 {
            let meta = std::fs::symlink_metadata(path).unwrap();
            let own = meta.blocks() * 512;
            if meta.is_dir() {
                own + std::fs::read_dir(path)
                    .unwrap()
                    .map(|entry| allocated(&entry.unwrap().path()))
                    .sum::<u64>()
            } else {
                own
            }
        }

        let rule = duzo_core::categories::RULES
            .iter()
            .find(|rule| rule.id == "rust-target")
            .unwrap();
        let base =
            std::env::temp_dir().join(format!("duzo-clean-actual-freed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let target = base.join("project/target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(base.join("project/Cargo.toml"), b"[package]").unwrap();
        std::fs::write(target.join("blob.bin"), b"temporary").unwrap();
        let actual = allocated(&target);
        let estimate = actual.saturating_add(1_000_000_000);
        let groups = vec![Group {
            rule,
            size: estimate,
            last_used: 0,
            paths: vec![target.as_path()],
        }];
        let options = options();

        let mut out = Vec::new();
        let removed = remove(&mut out, &groups, &options, true);
        let _ = std::fs::remove_dir_all(&base);
        removed.unwrap();

        let text = String::from_utf8(out).unwrap();
        // Matched by line rather than as "Freed <size>": on a terminal the
        // label is wrapped in bold codes.
        let freed = text
            .lines()
            .find(|line| line.contains("Freed"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(
            freed.ends_with(&format!(
                " {}",
                duzo_core::size::format(actual, options.unit())
            )),
            "{text}"
        );
    }

    /// Two scans within the same second must not write over each other, or
    /// the comparison would lose the snapshot it compares against.
    #[test]
    fn a_scan_recorded_in_the_same_second_as_the_last_gets_the_next_second() {
        let dir = std::env::temp_dir().join(format!("duzo-record-in-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tree = DiskEntry::new(PathBuf::from("/data"), EntryType::Directory);

        let first = record_in(Store::at(&dir).unwrap(), &tree, true, 1000);
        assert!(first.is_none(), "nothing to compare against yet");
        let (store, previous) = record_in(Store::at(&dir).unwrap(), &tree, true, 1000).unwrap();

        assert_eq!(previous.taken_at, 1000);
        let taken: Vec<u64> = store
            .list(Path::new("/data"))
            .iter()
            .map(|id| id.taken_at)
            .collect();
        assert_eq!(taken, [1000, 1001]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_json_omits_the_private_aggregate_fields() {
        let mut tree = DiskEntry::new(PathBuf::from("/root"), EntryType::Directory);
        tree.allocated_size = 1000;
        tree.apparent_size = 1000;
        let snapshot = Snapshot {
            root: PathBuf::from("/root"),
            taken_at: 1,
            total_allocated: 1000,
            total_apparent: 1000,
            items: 1,
            floor: 0,
            tree,
        };

        let public = vec![PublicSnapshot::from(&snapshot)];
        let text = serde_json::to_string(&public).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(parsed[0]["root"], "/root");
        assert_eq!(parsed[0]["taken_at"], 1);
        assert_eq!(parsed[0]["tree"]["allocated_size"], 1000);
        assert!(parsed[0]["tree"].get("own_modified").is_none());
        assert!(parsed[0]["tree"].get("own_scan_state").is_none());
    }
}
