//! Interactive state: what is on screen, what is selected, and what a key does.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use duzo_core::scan::{self, Cancel, Finished, Progress};
use duzo_core::{Diff, DiskEntry, Filesystem, ScanOptions, ScanState, SizeKind, Unit};
use duzo_render::{LayoutOptions, RadialNode, Segment, palette, radial};

use super::preferences::BrowseView;
use crate::deletion::{self, DeleteProgress, Outcome, Target};
use crate::model::{self, Metric, Row, RowOptions, Sort};

/// The word that has to be typed before anything is removed.
pub const CONFIRM_WORD: &str = "delete";

fn deletion_status(
    deleted: usize,
    skipped: usize,
    freed: u64,
    outcomes: &[Outcome],
    unit: Unit,
) -> String {
    // Name the outcome that left the tree in an unknown state when there is
    // one; a refusal that touched nothing is less urgent than that.
    let first_problem = outcomes
        .iter()
        .find(|outcome| !outcome.succeeded() && outcome.changed_tree())
        .or_else(|| outcomes.iter().find(|outcome| !outcome.succeeded()));
    let problem_hint = first_problem.map(|outcome| {
        format!(
            "{}: {}",
            model::display_path(outcome.path()),
            outcome.detail()
        )
    });
    let incomplete = first_problem.is_some_and(Outcome::changed_tree);

    match (deleted, skipped, problem_hint) {
        (0, _, Some(hint)) if incomplete => match freed {
            0 => format!("deletion incomplete — {hint}"),
            _ => format!(
                "deletion incomplete · freed {} — {hint}",
                duzo_core::size::format(freed, unit)
            ),
        },
        (0, _, hint) => hint
            .map(|hint| format!("nothing deleted — {hint}"))
            .unwrap_or_else(|| "nothing deleted — d for details".into()),
        (n, 0, _) => format!(
            "deleted {n} · freed {}",
            duzo_core::size::format(freed, unit)
        ),
        // A half-removed item is not merely skipped: say so even when others
        // were deleted. The hint names it.
        (n, 1, Some(hint)) if incomplete => format!(
            "deleted {n} · freed {} · deletion incomplete — {hint}",
            duzo_core::size::format(freed, unit)
        ),
        (n, skipped, Some(hint)) if incomplete => format!(
            "deleted {n} · freed {} · {skipped} not deleted · deletion incomplete — {hint} · d for details",
            duzo_core::size::format(freed, unit)
        ),
        (n, 1, hint) => {
            let base = format!(
                "deleted {n} · freed {} · 1 skipped",
                duzo_core::size::format(freed, unit)
            );
            hint.map(|hint| format!("{base} — {hint}")).unwrap_or(base)
        }
        (n, skipped, _) => format!(
            "deleted {n} · freed {} · {skipped} skipped — d for details",
            duzo_core::size::format(freed, unit)
        ),
    }
}

/// How far down a running scan publishes finished directories. Two levels is
/// enough that a scan of `/` shows `~/Library` long before `/` itself is done.
const STREAM_DEPTH: usize = 2;

/// How many finished directories to keep on the progress screen.
const STREAM_KEEP: usize = 12;

/// How much of the previous tree a rescan keeps on screen while it runs. The
/// sunburst draws three rings and the list draws one level, so five is already
/// more than anything on screen can reach.
const REFRESH_KEEP_DEPTH: usize = 5;

/// How deep a focus scan keeps what it finds below the folder it was started
/// for. Sizes stay exact; going further down starts the next focus scan.
const FOCUS_KEEP_DEPTH: usize = REFRESH_KEEP_DEPTH;

/// A pending deletion, waiting to be confirmed or called off.
#[derive(Clone, Debug, Default)]
pub struct Confirm {
    pub targets: Vec<Target>,
    /// What the user has typed toward [`CONFIRM_WORD`].
    pub typed: String,
    /// Set when Enter was pressed before the word was complete.
    pub nagged: bool,
}

impl Confirm {
    pub fn total(&self) -> u64 {
        self.targets.iter().map(|target| target.size).sum()
    }

    pub fn is_armed(&self) -> bool {
        self.typed == CONFIRM_WORD
    }
}

/// How completely the tree lists a folder; see [`App::cwd_listing`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Listing {
    /// Every entry it holds is listed — no rows means it is empty.
    Complete,
    /// Its contents are past `--depth`; no rescan here would keep them.
    BeyondDepth,
    /// Counted but not all listed, or never finished; a scan can fill it.
    Short,
    /// A mount point the scan stopped at; only scanning it directly lists it.
    Skipped,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// No path was given: choose a disk first.
    Picker,
    /// A scan is running.
    Scanning,
    /// The ranked list of what is using the space.
    Overview,
    /// The radial explorer.
    Explorer,
}

impl From<BrowseView> for View {
    fn from(view: BrowseView) -> Self {
        match view {
            BrowseView::Chart => Self::Explorer,
            BrowseView::List => Self::Overview,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub size_kind: SizeKind,
    pub unit: Unit,
    pub top: usize,
    pub scan_options: ScanOptions,
    pub show_all_filesystems: bool,
    /// Whether finished scans join the snapshot history.
    pub record_snapshots: bool,
}

/// Live watching: rescan every `every` seconds and report growth against the
/// state of the world when watching started.
#[derive(Clone, Debug)]
pub struct Watch {
    pub every: u64,
    /// The pruned tree the deltas are measured from. Shared, so a finished
    /// scan's own thread can compare against it.
    baseline: Option<Arc<DiskEntry>>,
    started_at: u64,
    last_scan: Instant,
    pub rounds: u64,
}

impl Watch {
    pub fn new(every: u64) -> Self {
        Self {
            every,
            baseline: None,
            started_at: duzo_core::history::now(),
            last_scan: Instant::now(),
            rounds: 0,
        }
    }

    pub fn is_due(&self) -> bool {
        self.last_scan.elapsed() >= Duration::from_secs(self.every)
    }

    pub fn elapsed(&self) -> u64 {
        duzo_core::history::now().saturating_sub(self.started_at)
    }
}

/// How a scan should treat the state around it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ScanMode {
    /// A new place: reset the cursor and show progress.
    Fresh,
    /// The same place again: keep where the user was standing.
    Refresh,
}

/// A deletion running on another thread, so the interface stays alive while
/// tens of thousands of files are being unlinked.
pub struct Deleting {
    pub progress: Arc<DeleteProgress>,
    cancel: Cancel,
    handle: Option<JoinHandle<Vec<Outcome>>>,
    stopping: bool,
}

impl Deleting {
    pub fn is_stopping(&self) -> bool {
        self.stopping
    }
}

/// The sunburst for one directory: the tree the layout runs over, the table
/// taking a path back to the wedge id standing for it, and the layout itself,
/// shared by every selection.
pub struct RadialCache {
    rings: usize,
    pub root: RadialNode,
    ids: HashMap<PathBuf, usize>,
    visible: OnceLock<Segments>,
    /// Includes slivers, built only when one needs to be highlighted.
    all: OnceLock<Vec<Segment>>,
}

impl RadialCache {
    /// The wedge drawn for `path`, if it is one of the ones drawn at all.
    pub fn id_of(&self, path: &Path) -> Option<usize> {
        self.ids.get(path).copied()
    }

    /// The normal wedges plus the selected sliver, if it was too thin to draw.
    /// `build_radial` and `build_growth_radial` number nodes depth-first, the
    /// order `layout` emits them in, so both layouts are sorted by id and
    /// inserting by id preserves the sibling order used to highlight a run of
    /// tiny wedges.
    pub fn segments(&self, pinned: Option<usize>) -> Segments {
        let visible = self.visible.get_or_init(|| {
            Arc::new(radial::layout(
                &self.root,
                &LayoutOptions {
                    rings: self.rings,
                    ..Default::default()
                },
            ))
        });
        let Some(id) = pinned else {
            return Arc::clone(visible);
        };
        let Err(at) = visible.binary_search_by_key(&id, |segment| segment.id) else {
            return Arc::clone(visible);
        };
        let all = self.all.get_or_init(|| {
            radial::layout(
                &self.root,
                &LayoutOptions {
                    rings: self.rings,
                    min_span: 0.0,
                    ..Default::default()
                },
            )
        });
        let Ok(selected) = all.binary_search_by_key(&id, |segment| segment.id) else {
            return Arc::clone(visible);
        };
        let mut segments = visible.as_ref().clone();
        segments.insert(at, all[selected].clone());
        Arc::new(segments)
    }
}

/// Everything the sunburst is built out of. While none of it has moved, the
/// cached sunburst is still the right one.
#[derive(Clone, PartialEq, Eq)]
struct RadialKey {
    cwd: PathBuf,
    rings: usize,
    size_kind: SizeKind,
    metric: Metric,
    /// Bumped whenever the scanned tree itself changes under the view.
    revision: u64,
}

/// Whether a running scan owns the whole tree or is grafted into a subtree.
///
/// Recorded when the session begins, so the landing code reads it instead of
/// comparing `scanned_path` with `self.root`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SessionKind {
    /// A scan of the whole tree (`begin`, `R`, a full-refresh drift rescan).
    Root,
    /// A scan grafted into part of an existing tree (`scan_subtree`).
    Subtree,
}

/// A scan running on another thread.
struct Session {
    scanned_path: PathBuf,
    kind: SessionKind,
    progress: Arc<Progress>,
    cancel: Cancel,
    /// The thread the finished scan lands on.
    handle: Option<Landing>,
    started: Instant,
    /// Set when `z` stops a subtree scan, or the scan correcting a snapshot
    /// painted for a new place. A stopped root rescan keeps its partial tree;
    /// these leave what is on screen as it was — the partial result is
    /// discarded once the thread joins instead of grafted in, unless a
    /// whole-tree scan finished counting before the stop reached it. Folders a
    /// focus scan filled in during it keep their fresh counts.
    discard_on_finish: bool,
}

/// A scan of the folder on screen, run beside the session's scan whose tree
/// does not list all of that folder yet (a refresh stub, a pruned snapshot,
/// or a stopped scan).
struct Focus {
    path: PathBuf,
    cancel: Cancel,
    handle: JoinHandle<Result<DiskEntry>>,
}

/// The size type the screen shows, readable from a landing thread. A scan is
/// compared in whatever size type is on screen when it finishes, as it was
/// when the UI thread did the comparing — not the one it started under.
#[derive(Clone, Default)]
struct LiveSizeKind(Arc<AtomicBool>);

impl LiveSizeKind {
    fn set(&self, kind: SizeKind) {
        self.0.store(kind == SizeKind::Apparent, Ordering::Relaxed);
    }

    fn get(&self) -> SizeKind {
        if self.0.load(Ordering::Relaxed) {
            SizeKind::Apparent
        } else {
            SizeKind::Allocated
        }
    }
}

/// What a finished scan is compared against, settled when it starts so the
/// comparison can run on a thread of its own.
enum Compare {
    /// A folder rescan grafted into the tree: nothing to compare.
    Nothing,
    /// Watch mode: against the first round, or become it.
    Watch {
        baseline: Option<Arc<DiskEntry>>,
        started_at: u64,
        kind: LiveSizeKind,
    },
    /// Against the newest stored snapshot, recording this scan as the next.
    Snapshot { kind: LiveSizeKind, record: bool },
}

/// A finished scan with its comparison already worked out.
struct Landed {
    tree: DiskEntry,
    diff: Option<Diff>,
    /// Watch mode's first round, trimmed down to become the baseline.
    baseline: Option<DiskEntry>,
}

/// What a landing thread and the UI tell each other.
#[derive(Default)]
struct LandingSignals {
    /// Set by the UI when it lets go of the session — back to the disk list,
    /// another disk picked: neither the comparison nor a snapshot of a scan
    /// walked away from is wanted.
    abandoned: AtomicBool,
    /// Set by the landing once nothing is left to write to disk: the snapshot
    /// recorded, or none to record. Quitting waits for this, not for the
    /// comparison after it.
    saved: AtomicBool,
}

/// A finished scan being compared on a thread of its own.
struct Landing {
    handle: JoinHandle<Result<Landed>>,
    signals: Arc<LandingSignals>,
}

impl Landing {
    fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Whether quitting now would lose a snapshot it has yet to write.
    fn saving(&self) -> bool {
        !self.signals.saved.load(Ordering::Relaxed) && !self.handle.is_finished()
    }

    /// Let go of it: nothing it would still compare or record is wanted, and
    /// it is joined elsewhere.
    fn abandon(self) {
        self.signals.abandoned.store(true, Ordering::Relaxed);
        join_in_background(self.handle);
    }
}

/// Wait for `scan`, then compare its tree, on another thread. Diffing a whole
/// disk against the last snapshot and writing it out as the next one takes
/// seconds; on the UI thread that froze the screen at whatever moment the
/// scan happened to finish.
fn land_in_background(scan: JoinHandle<Result<DiskEntry>>, compare: Compare) -> Landing {
    let signals = Arc::new(LandingSignals::default());
    let shared = Arc::clone(&signals);
    let handle = std::thread::spawn(move || {
        let signals = shared;
        let abandoned = || signals.abandoned.load(Ordering::Relaxed);
        let tree = match scan.join() {
            Ok(result) => result?,
            Err(panic) => std::panic::resume_unwind(panic),
        };
        // A partial count compared would show everything it had not reached
        // yet as shrunk; an abandoned scan is not wanted at all.
        let skip = tree.scan_state() == ScanState::Cancelled || abandoned();
        if skip || !matches!(compare, Compare::Snapshot { record: true, .. }) {
            // Nothing to write to disk, so nothing for a quit to wait for.
            signals.saved.store(true, Ordering::Relaxed);
        }
        if skip {
            return Ok(Landed {
                tree,
                diff: None,
                baseline: None,
            });
        }
        let (diff, baseline) = match compare {
            Compare::Nothing => (None, None),
            Compare::Watch {
                baseline: Some(baseline),
                started_at,
                kind,
            } => (
                Some(duzo_core::diff::diff(
                    &baseline,
                    &tree,
                    kind.get(),
                    started_at,
                    duzo_core::history::now(),
                    duzo_core::history::storage_floor(&baseline),
                )),
                None,
            ),
            Compare::Watch { baseline: None, .. } => {
                (None, Some(duzo_core::history::prune_for_storage(&tree)))
            }
            Compare::Snapshot { kind, record } => {
                let previous = crate::commands::record_snapshot(&tree, record);
                signals.saved.store(true, Ordering::Relaxed);
                // The comparison is the slow half — reading the previous
                // snapshot, then diffing — and not for a scan let go of
                // meanwhile.
                let diff = previous
                    .filter(|_| !abandoned())
                    .and_then(|(store, id)| store.load(&id).ok())
                    .filter(|_| !abandoned())
                    .map(|previous| crate::commands::diff_since(&previous, &tree, kind.get()));
                (diff, None)
            }
        };
        Ok(Landed {
            tree,
            diff,
            baseline,
        })
    });
    Landing { handle, signals }
}

/// Free `value` on a thread of its own. A whole-disk tree, or the comparison
/// that mirrors one, is millions of entries, and freeing them one by one on
/// the UI thread is a freeze of its own. If no thread can be started, `spawn`
/// frees it right here instead.
fn drop_in_background<T: Send + 'static>(value: T) {
    let _ = std::thread::Builder::new().spawn(move || drop(value));
}

/// Wait for `handle` on a thread of its own. A finished thread's result is
/// freed by whoever drops its handle last, and that must not be the UI
/// thread; an unfinished one is left to stop there.
fn join_in_background<T: Send + 'static>(handle: JoinHandle<T>) {
    let _ = std::thread::Builder::new().spawn(move || drop(handle.join()));
}

/// Said when a stop comes after the scan has counted everything: there is no
/// partial count to stop at any more, only the rest of the landing.
pub const FINISHING: &str = "finishing — the count is done…";

/// Said while a stopped rescan's partial count is on its way.
const STOPPED_SHORT: &str = "stopping — showing what was counted so far";

/// What `t` says once the scan correcting a snapshot was stopped.
const STOPPED_OVER_SNAPSHOT: &str = "scan was stopped — R rescans and compares";

/// Said while a stopped scan whose result is discarded winds down.
const STOPPING: &str = "stopping the background scan…";

/// What a cached list for the folder on screen was built from. While none of
/// it has moved, the list is still the right one.
#[derive(Clone, PartialEq, Eq)]
struct ListKey {
    cwd: PathBuf,
    revision: u64,
    /// Growth lists come from the comparison, which only ever changes along
    /// with the revision; this says which of the two lists is showing.
    growth: bool,
    options: RowOptions,
}

/// Folder, tree revision, size kind and count behind "Largest items".
type LargestKey = (PathBuf, u64, SizeKind, usize);

/// Laid-out sunburst wedges, shared with whatever frame is drawing them.
type Segments = Arc<Vec<Segment>>;

/// "Largest items": each path with its size.
type Largest = Arc<Vec<(PathBuf, u64)>>;

/// Folder, tree revision and size kind behind `cwd_shortfall`.
type ShortfallKey = (PathBuf, u64, SizeKind);

/// The values last built for the few folders visited most recently, so going
/// back up to a folder just left finds its rows, "Largest items" and sunburst
/// still there instead of building them again. A key naming the tree revision
/// never matches again once the tree changes.
struct Recent<K, V> {
    slots: RefCell<Vec<(K, V)>>,
    /// How many values are kept.
    keep: usize,
    /// Whether what falls out is big enough to free on a thread of its own;
    /// a few numbers are cheaper to free than a thread is to start.
    big: bool,
}

impl<K: PartialEq + Send + 'static, V: Clone + Send + 'static> Recent<K, V> {
    /// The folder on screen and a few on the way back up from it.
    const FOLDERS: usize = 4;

    fn new() -> Self {
        Self::with(Self::FOLDERS, false)
    }

    /// For values like a big folder's rows, freed off the UI thread when they
    /// fall out.
    fn big() -> Self {
        Self::with(Self::FOLDERS, true)
    }

    /// A single slot, for lists that change with every key typed — a search
    /// — so they never push a folder out of the cache beside it.
    fn one(big: bool) -> Self {
        Self::with(1, big)
    }

    fn with(keep: usize, big: bool) -> Self {
        Self {
            slots: RefCell::new(Vec::new()),
            keep,
            big,
        }
    }

    /// Forget everything: nothing built from an older tree is asked for again.
    fn clear(&self) {
        let slots = std::mem::take(&mut *self.slots.borrow_mut());
        if self.big && !slots.is_empty() {
            drop_in_background(slots);
        }
    }

    fn get_or_build(&self, key: K, build: impl FnOnce(&K) -> V) -> V {
        let hit = {
            let mut slots = self.slots.borrow_mut();
            slots
                .iter()
                .position(|(cached, _)| *cached == key)
                .map(|at| {
                    let slot = slots.remove(at);
                    let value = slot.1.clone();
                    slots.push(slot);
                    value
                })
        };
        if let Some(value) = hit {
            return value;
        }
        // Built with no borrow held, so building may read another cache.
        let value = build(&key);
        let mut slots = self.slots.borrow_mut();
        if slots.len() >= self.keep {
            let evicted = slots.remove(0);
            if self.big {
                drop_in_background(evicted);
            }
        }
        slots.push((key, value.clone()));
        value
    }
}

pub struct App {
    pub settings: Settings,
    pub view: View,
    /// The view scans open on: the one Tab last picked.
    pub(super) browsing_view: BrowseView,
    pub filesystems: Vec<Filesystem>,
    pub picker_index: usize,
    pub tree: Option<DiskEntry>,
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub selection: usize,
    pub marks: HashSet<PathBuf>,
    pub search: Option<String>,
    pub search_active: bool,
    pub sort: Sort,
    pub details: bool,
    pub filesystem: Option<Filesystem>,
    pub status: Option<String>,
    pub quit: bool,
    /// Measuring size, or measuring what changed.
    pub metric: Metric,
    /// The comparison behind growth mode, when there is one.
    pub diff: Option<Diff>,
    /// A deletion or subtree rescan changed the tree after the scan, which
    /// drops the comparison until the next full scan.
    changed_since_scan: bool,
    pub watch: Option<Watch>,
    /// True when the disk picker is where this scan came from, so "up" from
    /// the top of the tree has somewhere to go back to.
    pub from_picker: bool,
    /// A deletion waiting on confirmation.
    pub confirm: Option<Confirm>,
    /// What the last deletion did, shown until the next keystroke.
    pub outcomes: Vec<Outcome>,
    /// When set, duzo will not delete anything at all.
    pub read_only: bool,
    /// When the numbers on screen came from a stored snapshot rather than the
    /// scan that is still running: the moment that snapshot was taken.
    pub provisional: Option<u64>,
    /// Set when `z` stopped that scan: the snapshot stays on screen, and
    /// nothing is coming to correct it until the next whole-tree scan.
    pub snapshot_kept: bool,
    /// Directories the running scan has finished counting, largest first.
    pub streamed: Vec<Finished>,
    /// A deletion in flight.
    pub deleting: Option<Deleting>,
    tick: usize,
    session: Option<Session>,
    /// The folder being scanned ahead of the running session, if any.
    focus: Option<Focus>,
    /// A folder `ensure_focus` gave up on, and the tree revision it gave up
    /// at, so it is not retried every frame.
    focus_refused: Option<(PathBuf, u64)>,
    /// Mount points read by the first focus scan of a session and reused by
    /// the rest, so each folder opened does not list the mounts again.
    focus_boundaries: scan::MountBoundaries,
    /// Folders a focus scan grafted into the tree during the running
    /// session; their counts are fresh, not the snapshot's or the stub's.
    focus_grafted: Vec<PathBuf>,
    /// Folders counted afresh, by a focus scan or an `r` rescan, during
    /// sessions that ended while a snapshot stayed on screen.
    counted_since_snapshot: Vec<PathBuf>,
    mode: ScanMode,
    /// How many times the tree behind the view has been replaced or edited.
    revision: u64,
    /// Sunbursts for the folders visited last.
    radial: Recent<RadialKey, Arc<RadialCache>>,
    /// Rows for the folders visited last. Every frame draws them and most
    /// keys look them up; sorting a big folder for each was a lag.
    rows_cache: Recent<ListKey, Arc<Vec<Row>>>,
    /// Rows of the folder on screen filtered by a search, on their own so
    /// typing does not push folders out of `rows_cache`.
    search_rows: Recent<ListKey, Arc<Vec<Row>>>,
    /// The explorer legend's measure of those rows; see `legend_measure`.
    measure_cache: Recent<(ListKey, Unit), (usize, i64)>,
    /// The same for searched rows.
    search_measure: Recent<(ListKey, Unit), (usize, i64)>,
    /// The overview's "Largest items", kept the same way.
    largest_cache: Recent<LargestKey, Largest>,
    /// What each of those folders holds beyond its rows; see `cwd_shortfall`.
    shortfall_cache: Recent<ShortfallKey, Option<(u64, u64)>>,
    /// `settings.size_kind`, shared with the thread a scan lands on.
    live_size_kind: LiveSizeKind,
    /// The disk list being read again for the picker, off the UI thread.
    filesystems_refresh: Option<JoinHandle<Vec<Filesystem>>>,
}

impl App {
    pub fn new(settings: Settings, details: bool) -> Self {
        let filesystems = duzo_core::mounts::list(settings.show_all_filesystems);
        Self {
            settings,
            view: View::Picker,
            browsing_view: BrowseView::default(),
            filesystems,
            picker_index: 0,
            tree: None,
            root: PathBuf::new(),
            cwd: PathBuf::new(),
            selection: 0,
            marks: HashSet::new(),
            search: None,
            search_active: false,
            sort: Sort::Size,
            details,
            filesystem: None,
            status: None,
            quit: false,
            metric: Metric::Size,
            diff: None,
            changed_since_scan: false,
            watch: None,
            from_picker: false,
            confirm: None,
            outcomes: Vec::new(),
            read_only: false,
            provisional: None,
            snapshot_kept: false,
            streamed: Vec::new(),
            deleting: None,
            tick: 0,
            session: None,
            focus: None,
            focus_refused: None,
            focus_boundaries: scan::MountBoundaries::default(),
            focus_grafted: Vec::new(),
            counted_since_snapshot: Vec::new(),
            mode: ScanMode::Fresh,
            revision: 0,
            radial: Recent::big(),
            rows_cache: Recent::big(),
            search_rows: Recent::one(true),
            measure_cache: Recent::new(),
            search_measure: Recent::one(false),
            largest_cache: Recent::new(),
            shortfall_cache: Recent::new(),
            live_size_kind: LiveSizeKind::default(),
            filesystems_refresh: None,
        }
    }

    /// Show the chart or the list. The list is capped at `--top` rows and the
    /// chart is not, so the cursor is kept on a row that exists.
    pub(super) fn show_view(&mut self, view: BrowseView) {
        self.view = view.into();
        self.selection = self.selection.min(self.rows().len().saturating_sub(1));
    }

    /// Show a view and make it the one later scans open on.
    pub(super) fn set_browsing_view(&mut self, view: BrowseView) {
        self.browsing_view = view;
        self.show_view(view);
    }

    /// Rescan on a timer and report growth since the moment watching started.
    pub fn watching(mut self, watch: Watch) -> Self {
        self.watch = Some(watch);
        self.metric = Metric::Growth;
        self
    }

    /// Start straight in a scan instead of the disk picker. There is no list
    /// to go back to in this case — the user named the place themselves.
    pub fn with_path(mut self, path: &Path) -> Self {
        self.from_picker = false;
        self.start_scan(path);
        self
    }

    pub fn start_scan(&mut self, path: &Path) {
        self.begin(path, ScanMode::Fresh);
    }

    /// Scan the same place again without disturbing the view — what watch mode
    /// does on every tick, and what `R` (or `r` at the root) does on demand.
    pub fn refresh(&mut self) {
        if self.root.as_os_str().is_empty() || self.session.is_some() || self.deleting.is_some() {
            return;
        }
        let root = self.root.clone();
        self.begin(&root, ScanMode::Refresh);
    }

    /// Correct an accounting drift a deletion left behind.
    ///
    /// A `Deleted` target that freed less than the tree believed it held left
    /// bytes the tree credited to it somewhere else — with a hard link that
    /// survived, or after a change made outside duzo since the scan — and
    /// only a full [`refresh`](Self::refresh) can find them. A target that
    /// unlinked a file another link still holds ([`Outcome::links_survived`])
    /// gets the same treatment when its total is off at all, since a file that
    /// grew inside it can cover the shortfall, and so does a failed one, which
    /// has no total to compare. Any other drift only touched what is under the
    /// target's parent, so rescanning the closest parent of every drifted
    /// target is enough — on a `/` scan, one folder instead of the whole disk.
    fn rescan_drifted(&mut self, outcomes: &[Outcome], removed_allocated: &HashMap<PathBuf, u64>) {
        if self.session.is_some() {
            return;
        }
        // Each target on its own: across the batch, one target's excess (a
        // file that grew since the scan) could hide another's shortfall.
        let needs_full_rescan = outcomes.iter().any(|outcome| match outcome {
            Outcome::Deleted {
                path,
                freed,
                links_survived,
                ..
            } => removed_allocated
                .get(path)
                .is_some_and(|expected| freed < expected || *links_survived && freed != expected),
            outcome => outcome.links_survived(),
        });
        if needs_full_rescan {
            self.refresh();
            return;
        }
        let Some(root) = self.parent_for_drifted(outcomes) else {
            return;
        };
        if root == self.root {
            self.refresh();
        } else {
            self.scan_subtree(root);
        }
    }

    /// The lowest directory that contains every target `outcomes` says
    /// drifted — usually one target's own parent; the common ancestor of
    /// several, still short of a full rescan as long as they share one below
    /// the scan root.
    fn parent_for_drifted(&self, outcomes: &[Outcome]) -> Option<PathBuf> {
        let mut ancestor: Option<PathBuf> = None;
        for outcome in outcomes {
            if !outcome.needs_rescan() {
                continue;
            }
            let parent = outcome.path().parent().unwrap_or(&self.root);
            ancestor = Some(match ancestor {
                None => parent.to_path_buf(),
                Some(current) => common_ancestor(&current, parent, &self.root),
            });
        }
        ancestor
    }

    fn begin(&mut self, path: &Path, mode: ScanMode) {
        if self.deleting.is_some() {
            self.status = Some("wait for deletion to finish before scanning".into());
            return;
        }
        if self.confirm.is_some() {
            return;
        }
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if mode == ScanMode::Refresh && path != self.root {
            // The root now resolves somewhere else than what is on screen —
            // the same symlink-swap case `scan_subtree` refuses, just hit by
            // rescanning the top of the tree instead of a folder under it.
            // Grafting what `path` now points at back in under the old root
            // would silently swap in a different subtree, so refuse before
            // any stub replaces what is on screen.
            self.status = Some(format!(
                "{} now points somewhere else — go back and open it again",
                model::display_path(&self.root)
            ));
            // Watch mode reaches here once per frame while the root stays
            // moved; move its clock so the refusal repeats once an interval,
            // not every frame.
            if let Some(watch) = &mut self.watch {
                watch.last_scan = Instant::now();
            }
            return;
        }
        self.mode = mode;
        self.touch();

        if mode == ScanMode::Fresh {
            // Watch mode normally never sees a `Fresh` begin at all — it
            // scans one path for its whole run — except when a watched root
            // disappears mid-scan: `poll_scan`'s error arm drops back to the
            // Picker without clearing `watch`, and picking a filesystem from
            // there reaches here with a different root. Leaving the old
            // root's baseline in place would diff the new tree against a
            // different place on disk; only the interval the user chose
            // should survive a change of root, not what it was measured from.
            if self.watch.is_some() && path != self.root {
                if let Some(watch) = &mut self.watch {
                    watch.baseline = None;
                    watch.started_at = duzo_core::history::now();
                    watch.rounds = 0;
                }
                // The growth view was measured against the old root too.
                self.set_diff(None);
            }
            self.filesystem = duzo_core::mounts::for_path(&path);
            self.root = path.clone();
            self.cwd = path.clone();
            self.selection = 0;
            self.search = None;
            self.search_active = false;
            if let Some(previous) = self.tree.take() {
                drop_in_background(previous);
            }
            self.changed_since_scan = false;
            self.status = None;
            self.streamed.clear();
            self.provisional = None;
            self.focus_grafted.clear();
            self.counted_since_snapshot.clear();
            self.view = View::Scanning;

            // Paint what duzo knew last time straight away, clearly labelled,
            // and let the scan correct it. Waiting on a spinner for numbers we
            // already have is a second nobody needs to spend.
            if self.settings.record_snapshots
                && let Some(snapshot) = crate::commands::last_snapshot(&path)
            {
                self.tree = Some(snapshot.tree);
                self.provisional = Some(snapshot.taken_at);
                self.view = self.browsing_view.into();
            }
        } else if let Some(tree) = &mut self.tree {
            // A rescan builds a whole second tree before it can replace this
            // one, so holding on to it intact means every entry is in memory
            // twice for the length of the scan — a gigabyte, on a home
            // directory with two million things under it. Keep the part the
            // screen can actually reach and let go of the rest; `absorb_scan`
            // puts the full tree back in a moment.
            let mut cut = Vec::new();
            trim_to_visible(
                &mut self.root.clone(),
                tree,
                &self.cwd,
                REFRESH_KEEP_DEPTH,
                &mut cut,
            );
            drop_in_background(cut);
        }

        // Only the initial progress screen consumes directory summaries.
        // Background scans still publish their live counters without them.
        let stream_depth = if self.view == View::Scanning {
            STREAM_DEPTH
        } else {
            0
        };
        let (progress, cancel, handle) = scan::scan_in_background_streaming(
            path.clone(),
            self.settings.scan_options.clone(),
            stream_depth,
        );
        // Defensive: every caller today reaches here with no session running
        // (`refresh` refuses while one does, and the Picker is only entered
        // after the session was dropped). Should one still be live, cancel its
        // token before it is dropped, or the detached thread keeps scanning
        // an abandoned root to a result nothing will ever read.
        if let Some(previous) = self.session.take() {
            previous.cancel.cancel();
            if let Some(landing) = previous.handle {
                landing.abandon();
            }
        }
        // A focus scan belongs to the tree it was grafting into, now replaced.
        self.drop_focus();
        self.focus_boundaries = scan::MountBoundaries::default();
        self.forget_focus_grafts();
        let compare = self.compare_for(SessionKind::Root);
        let landing = land_in_background(handle, compare);
        self.snapshot_kept = false;
        self.session = Some(Session {
            scanned_path: path,
            kind: SessionKind::Root,
            progress,
            cancel,
            handle: Some(landing),
            started: Instant::now(),
            discard_on_finish: false,
        });
    }

    /// What a scan of `kind` started now is compared against once it
    /// finishes. Settled at the start so the scan's own thread can do the
    /// comparing; see `land_in_background`.
    fn compare_for(&self, kind: SessionKind) -> Compare {
        // `settings` is public and can be set directly; start from it.
        self.live_size_kind.set(self.settings.size_kind);
        match (kind, &self.watch) {
            (SessionKind::Subtree, _) => Compare::Nothing,
            (SessionKind::Root, Some(watch)) => Compare::Watch {
                baseline: watch.baseline.clone(),
                started_at: watch.started_at,
                kind: self.live_size_kind.clone(),
            },
            (SessionKind::Root, None) => Compare::Snapshot {
                kind: self.live_size_kind.clone(),
                record: self.settings.record_snapshots,
            },
        }
    }

    /// Advance the animation clock. Called once per frame.
    pub fn advance(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Kick off a rescan when the watch interval is up.
    pub fn tick_watch(&mut self) {
        if self.confirm.is_some() || self.deleting.is_some() {
            // Held back by a deletion. The interval starts over once it is
            // settled, or an overdue rescan would refuse the next `x`.
            if let Some(watch) = &mut self.watch {
                watch.last_scan = Instant::now();
            }
            return;
        }
        // Anything other than a browsing view with a real tree and nothing
        // else in flight means there is no tree behind the screen for a
        // refresh to have anywhere sane to land — most visibly the Picker
        // after a watched mount disappears (`Ok(Err(error))` in `poll_scan`
        // switches the view there and drops the tree without touching
        // `watch`), where `refresh` would otherwise be retried every frame.
        let browsing = matches!(self.view, View::Overview | View::Explorer);
        let due = self
            .watch
            .as_ref()
            .is_some_and(|watch| watch.is_due() && self.tree.is_some());
        if due && browsing && self.session.is_none() {
            self.refresh();
        }
    }

    /// How long the running scan has been going. A scan that has stopped
    /// counting but keeps ticking is usually stuck on a slow mount, and the
    /// number is the first clue.
    pub fn scan_elapsed(&self) -> Option<Duration> {
        self.session
            .as_ref()
            .map(|session| session.started.elapsed())
    }

    pub fn progress(&self) -> Option<&Progress> {
        self.session
            .as_ref()
            .map(|session| session.progress.as_ref())
    }

    pub fn scanned_path(&self) -> Option<&Path> {
        self.session
            .as_ref()
            .map(|session| session.scanned_path.as_path())
    }

    /// True while a running scan will change what the current folder shows:
    /// a rescan from a snapshot, or a session rooted at the folder or above it.
    /// A stopped one is only winding down and fills nothing in.
    pub fn cwd_is_being_scanned(&self) -> bool {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.cancel.is_cancelled())
        {
            return false;
        }
        self.provisional.is_some() && !self.snapshot_kept
            || self
                .scanned_path()
                .is_some_and(|scanned| self.cwd.starts_with(scanned))
    }

    /// Whether the folder on screen has no scan of its own ahead of the rest:
    /// `ensure_focus` gave up on it (it now resolves elsewhere, crosses a
    /// mount the run skips, or its own scan failed), so only the full scan
    /// fills it in, when it gets there.
    pub fn cwd_focus_refused(&self) -> bool {
        self.focus_refused
            .as_ref()
            .is_some_and(|(path, revision)| *path == self.cwd && *revision == self.revision)
    }

    /// How completely the tree lists the current folder: every entry it
    /// holds, everything but what `--depth` dropped, or less than it holds
    /// for another reason — past what a focus scan keeps, pruned from a
    /// snapshot or stub, or cut short by a stop.
    pub fn cwd_listing(&self) -> Listing {
        if self
            .current_entry()
            .is_some_and(|entry| entry.scan_state() == ScanState::Skipped)
        {
            Listing::Skipped
        } else if self.cwd_shortfall().is_none() {
            Listing::Complete
        } else if self.subtree_scan_options(&self.cwd).max_depth == Some(0) {
            // `r` would keep nothing below here either.
            Listing::BeyondDepth
        } else {
            Listing::Short
        }
    }

    /// What the current folder holds beyond the rows it lists, when its
    /// listing is short and the size view is showing: `(entries, bytes)`.
    /// Both are 0 for a listing a stop cut short without knowing the rest.
    pub fn cwd_unlisted(&self) -> Option<(u64, u64)> {
        if self.showing_growth() || self.cwd_listing() != Listing::Short {
            return None;
        }
        self.cwd_shortfall()
    }

    /// What the current folder holds beyond the rows it lists, `(entries,
    /// bytes)`, when it lists less than it holds. Kept like `rows`: it adds
    /// up every child, and several things ask on every frame.
    fn cwd_shortfall(&self) -> Option<(u64, u64)> {
        let kind = self.settings.size_kind;
        let key = (self.cwd.clone(), self.revision, kind);
        self.shortfall_cache.get_or_build(key, |_| {
            let entry = self
                .current_entry()
                .filter(|entry| lists_less_than_it_holds(entry))?;
            let (items, bytes) =
                entry
                    .children
                    .iter()
                    .fold((1u64, 0u64), |(items, bytes), child| {
                        (
                            items.saturating_add(child.items),
                            bytes.saturating_add(child.size(kind)),
                        )
                    });
            Some((
                entry.items.saturating_sub(items),
                entry.size(kind).saturating_sub(bytes),
            ))
        })
    }

    /// When the screen shows a stored snapshot, the time it was taken — unless
    /// the folder on screen has been counted afresh since, by a focus scan or
    /// an `r` rescan.
    pub fn provisional_for_cwd(&self) -> Option<u64> {
        self.provisional.filter(|_| {
            !self
                .focus_grafted
                .iter()
                .chain(&self.counted_since_snapshot)
                .any(|path| self.cwd.starts_with(path))
        })
    }

    /// A new session starts its own list of focus grafts. Under a snapshot,
    /// the folders the last one counted keep their fresh label.
    fn forget_focus_grafts(&mut self) {
        if self.provisional.is_some() {
            self.counted_since_snapshot.append(&mut self.focus_grafted);
        } else {
            self.focus_grafted.clear();
        }
    }

    /// Scan only a subtree in the background and graft the result into the current tree.
    ///
    /// An entry the scan marked `Skipped` is another filesystem or a system
    /// mount; scanning it as its own root would exempt it from the filters
    /// that skipped it and graft that volume into this tree's totals.
    pub fn scan_subtree(&mut self, path: PathBuf) {
        if self.session.is_some() || self.deleting.is_some() || self.confirm.is_some() {
            return;
        }
        if self
            .tree
            .as_ref()
            .and_then(|tree| tree.resolve(&path))
            .is_some_and(|entry| entry.scan_state() == ScanState::Skipped)
        {
            self.status = Some(format!(
                "{} was skipped by this scan — run duzo on it directly",
                model::display_path(&path)
            ));
            return;
        }
        // A canonical path that disagrees with what is on screen means the
        // folder at `path` was replaced by a symlink since the last scan (or
        // by something else `canonicalize` resolves through) — scanning what
        // the symlink now points at and grafting it in under the old name
        // would silently swap in a different subtree. A path `canonicalize`
        // cannot resolve at all (already gone, a permission error on an
        // ancestor) is not that case, so it still gets a rescan attempt and
        // fails there with its own message instead of here. The scan thread
        // checks the same before walking; this says it with a way out.
        if path.canonicalize().is_ok_and(|canonical| canonical != path) {
            self.status = Some(format!(
                "{} now points somewhere else — go back and open it again",
                model::display_path(&path)
            ));
            return;
        }
        let options = self.subtree_scan_options(&path);
        let (progress, cancel, handle) =
            scan::scan_in_background_within(path.clone(), self.root.clone(), options);
        self.focus_boundaries = scan::MountBoundaries::default();
        self.forget_focus_grafts();
        let landing = land_in_background(handle, self.compare_for(SessionKind::Subtree));
        self.session = Some(Session {
            scanned_path: path,
            kind: SessionKind::Subtree,
            progress,
            cancel,
            handle: Some(landing),
            started: Instant::now(),
            discard_on_finish: false,
        });
    }

    fn subtree_scan_options(&self, path: &Path) -> ScanOptions {
        let mut options = self.settings.scan_options.clone();
        if let Some(max_depth) = options.max_depth {
            let depth = path
                .strip_prefix(&self.root)
                .map(|relative| relative.components().count())
                .unwrap_or(max_depth);
            options.max_depth = Some(max_depth.saturating_sub(depth));
        }
        options
    }

    /// Collect a finished scan. Called once per frame; cheap while running.
    pub fn poll_scan(&mut self) {
        self.collect_streamed();
        // Before `poll_focus`: a focus that finished during the frame the
        // user went past its reach must not graft.
        self.drop_focus_left_too_far_above();
        self.poll_focus();
        self.ensure_focus();

        let Some(session) = &mut self.session else {
            return;
        };
        let finished = session
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished());
        if !finished {
            return;
        }

        let Some(landing) = session.handle.take() else {
            return;
        };
        let scanned_path = session.scanned_path.clone();
        let kind = session.kind;
        let stopped = session.discard_on_finish;
        let joined = landing.handle.join();
        // A whole-tree stop that came too late to cut the count short lands
        // whole, like any finished scan, rather than throwing its count away.
        let landed_whole = kind == SessionKind::Root
            && matches!(&joined, Ok(Ok(landed)) if landed.tree.scan_state() != ScanState::Cancelled);
        if stopped && landed_whole && self.status.as_deref() == Some(STOPPING) {
            self.status = None;
        }
        let discard = stopped && !landed_whole;
        if discard {
            // A stopped folder rescan, or a stopped scan over a snapshot: drop
            // its partial result and keep what is on screen as it was — apart
            // from folders opened during it, which a focus scan already filled
            // in with fresh counts.
            let snapshot = self.provisional.filter(|_| kind == SessionKind::Root);
            self.snapshot_kept |= snapshot.is_some();
            self.status = Some(if let Some(taken_at) = snapshot {
                format!(
                    "stopped — showing the snapshot from {}",
                    crate::timefmt::ago(duzo_core::history::now().saturating_sub(taken_at))
                )
            } else if !self.focus_grafted.is_empty() {
                format!(
                    "stopped — {} unchanged, except folders opened meanwhile",
                    model::display_path(&scanned_path)
                )
            } else {
                format!("stopped — {} unchanged", model::display_path(&scanned_path))
            });
            self.session = None;
            // A stopped whole-tree scan is a watch round, as a failed one is.
            if kind == SessionKind::Root
                && let Some(watch) = &mut self.watch
            {
                watch.last_scan = Instant::now();
            }
            self.keep_focus_grafts_as_a_change();
            self.drop_focus();
            if let Ok(Ok(landed)) = joined {
                drop_in_background(landed.tree);
            }
            return;
        }
        let mut applied = false;
        let mut failure = None;
        match joined {
            Ok(Ok(landed)) => {
                if kind == SessionKind::Root {
                    self.absorb_scan(landed);
                } else {
                    let grafted = match self.tree.as_mut() {
                        Some(current_tree) => current_tree.graft(&scanned_path, landed.tree),
                        None => Err(landed.tree),
                    };
                    match grafted {
                        Ok(replaced) => {
                            drop_in_background(replaced);
                            if self.provisional.is_some() {
                                self.counted_since_snapshot.push(scanned_path.clone());
                            }
                            self.finish_tree_mutation();
                            self.step_out_of_a_vanished_folder();
                            let rows = self.rows().len();
                            self.selection = self.selection.min(rows.saturating_sub(1));
                            applied = true;
                        }
                        Err(refused) => {
                            drop_in_background(refused);
                            self.status = Some(format!(
                                "cannot apply rescan for {}",
                                model::display_path(&scanned_path)
                            ));
                        }
                    }
                }
            }
            Ok(Err(error)) => failure = Some(format!("{error}")),
            // A panicked worker must not take the UI down with it.
            Err(_) => failure = Some("scan thread panicked".to_string()),
        }
        if let Some(failure) = failure {
            self.status = Some(failure);
            if kind == SessionKind::Root {
                self.view = View::Picker;
                // A failed root scan leaves nothing new to compare against,
                // so treat it the way a finished one moves the clock — without
                // this, `is_due()` stays true and `tick_watch` starts another
                // one on the very next frame instead of waiting out the
                // interval. A failed folder rescan is not a watch round.
                if let Some(watch) = &mut self.watch {
                    watch.last_scan = Instant::now();
                }
            }
        }
        self.session = None;
        if kind == SessionKind::Subtree && !applied {
            // A folder rescan that failed, panicked or could not be applied
            // left the tree as it was, except folders a focus scan filled in.
            self.keep_focus_grafts_as_a_change();
        }
        // The tree on screen is now the scan's own; a focus result would only
        // graft older, partial numbers back over it.
        self.drop_focus();
    }

    /// Folders a focus scan filled in during a folder rescan that did not
    /// land keep their new counts, so the tree is no longer the last scan's:
    /// say so the way a landed rescan does, or growth would compare it.
    fn keep_focus_grafts_as_a_change(&mut self) {
        if !self.focus_grafted.is_empty() {
            self.finish_tree_mutation();
        }
    }

    /// A focus left behind more levels up than it keeps would graft back a
    /// listing that stops above the folder on screen, emptying it.
    fn drop_focus_left_too_far_above(&mut self) {
        if self.focus.as_ref().is_some_and(|focus| {
            self.cwd
                .strip_prefix(&focus.path)
                .is_ok_and(|below| below.components().count() >= FOCUS_KEEP_DEPTH)
        }) {
            self.drop_focus();
        }
    }

    /// Keep browsing ahead of a running scan. When the folder on screen lists
    /// less than it holds — below what a refresh stub keeps, pruned out of
    /// the snapshot being shown, or never reached by a scan that was stopped
    /// — and the root scan or an `r` rescan above it will fill it in later,
    /// scan just that folder on the priority pool and graft it in when it
    /// lands, instead of waiting for the whole scan.
    ///
    /// Only one runs at a time. Opening another such folder — a sibling or a
    /// child — stops the previous one: the priority pool, like any rayon
    /// pool, finishes work it already started before it takes a new job, so
    /// a parent's scan left running would make the child wait for all of it.
    fn ensure_focus(&mut self) {
        // Only a scan that is still counting will be slow to arrive — the
        // root scan, or an `r` rescan of a folder above this one — and only
        // the size view lists from `self.tree`; growth lists the diff. One
        // that has counted everything is only being compared and lands soon.
        let Some(scanned) = self.session.as_ref().and_then(|session| {
            let running = !session.cancel.is_cancelled()
                && !session.progress.is_finished()
                && session
                    .handle
                    .as_ref()
                    .is_some_and(|handle| !handle.is_finished());
            running.then(|| session.scanned_path.clone())
        }) else {
            return;
        };
        if self.showing_growth()
            // The session itself is already walking that folder.
            || self.cwd == scanned
            || !self.cwd.starts_with(&scanned)
            || self.focus.as_ref().is_some_and(|f| f.path == self.cwd)
            || self.cwd_focus_refused()
            || self.cwd_shortfall().is_none()
        {
            return;
        }
        let mut options = self.subtree_scan_options(&self.cwd);
        if options.max_depth == Some(0) {
            // At the `--depth` limit: the session's scan keeps nothing here either.
            return;
        }
        options.max_depth = Some(
            options
                .max_depth
                .map_or(FOCUS_KEEP_DEPTH, |depth| depth.min(FOCUS_KEEP_DEPTH)),
        );
        self.drop_focus();
        let (_, cancel, handle) = scan::scan_in_background_prioritized(
            self.cwd.clone(),
            scanned,
            options,
            self.focus_boundaries.clone(),
        );
        self.focus = Some(Focus {
            path: self.cwd.clone(),
            cancel,
            handle,
        });
    }

    /// Graft a finished focus scan into the tree on screen.
    fn poll_focus(&mut self) {
        if !self.focus.as_ref().is_some_and(|f| f.handle.is_finished()) {
            return;
        }
        let Some(focus) = self.focus.take() else {
            return;
        };
        // The graft can re-rank the folder the cursor is on; keep the cursor
        // on the same entry rather than the same index.
        let selected = self.selected_row().and_then(|row| row.path);
        // The scan resolves its root on its own thread, so the UI never
        // waits on a path lookup. As in `scan_subtree`, a folder that now
        // resolves elsewhere (swapped for a symlink) must not be grafted in
        // under its old name.
        let landed = match focus.handle.join() {
            Ok(Ok(subtree)) => Some(subtree),
            _ => None,
        };
        let grafted = landed.and_then(|subtree| {
            let result = match self.tree.as_mut() {
                Some(tree) if subtree.root_path() == focus.path => tree.graft(&focus.path, subtree),
                _ => Err(subtree),
            };
            match result {
                Ok(replaced) => Some(replaced),
                // Five levels of a big folder, possibly.
                Err(refused) => {
                    drop_in_background(refused);
                    None
                }
            }
        });
        if let Some(replaced) = grafted {
            drop_in_background(replaced);
            self.touch();
            self.focus_grafted.push(focus.path.clone());
            if self.step_out_of_a_vanished_folder() {
                return;
            }
            let rows = self.rows();
            let still_here = |path: &PathBuf| {
                self.tree
                    .as_ref()
                    .is_some_and(|tree| tree.resolve(path).is_some())
            };
            self.selection = selected
                .and_then(|path| {
                    rows.iter()
                        .position(|row| row.path.as_ref() == Some(&path))
                        // An entry folded into "Other" is still under the
                        // cursor there; one that is gone is not.
                        .or_else(|| {
                            still_here(&path)
                                .then(|| rows.iter().position(|row| row.path.is_none()))
                                .flatten()
                        })
                })
                .unwrap_or(self.selection.min(rows.len().saturating_sub(1)));
            return;
        }
        // Failed, moved, or no longer fits the tree: leave it to the session.
        self.focus_refused = Some((focus.path, self.revision));
    }

    fn drop_focus(&mut self) {
        if let Some(focus) = self.focus.take() {
            focus.cancel.cancel();
            join_in_background(focus.handle);
        }
        self.focus_refused = None;
    }

    /// Move up from a folder the tree no longer has — or now has as a file,
    /// replaced since the snapshot — to its nearest ancestor that it does, as
    /// `h` would. Returns whether it moved.
    ///
    /// A search being typed stays open — the keys after it are still meant
    /// for the search box — but a finished one is for the folder left behind
    /// and is dropped.
    fn climb_to_a_listed_folder(&mut self) -> bool {
        let folder = self.cwd.clone();
        while !self.current_entry().is_some_and(DiskEntry::is_dir) && self.cwd != self.root {
            match self.cwd.parent() {
                Some(parent) => self.cwd = parent.to_path_buf(),
                None => {
                    self.cwd = self.root.clone();
                    break;
                }
            }
        }
        if self.cwd == folder {
            return false;
        }
        self.selection = 0;
        if !self.search_active {
            self.search = None;
        }
        true
    }

    /// After the tree on screen is replaced or grafted: if the folder being
    /// shown is no longer in it (deleted since the snapshot, or past what a
    /// stopped scan reached), climb to the nearest folder that is. Returns
    /// whether it moved.
    ///
    /// Not in the growth view, which lists from the diff and keeps removed
    /// folders on purpose.
    fn step_out_of_a_vanished_folder(&mut self) -> bool {
        !self.showing_growth() && self.climb_to_a_listed_folder()
    }

    /// Absorb whatever the running scan has finished counting since the last
    /// frame, keeping the largest first.
    fn collect_streamed(&mut self) {
        if self.view != View::Scanning {
            return;
        }
        let Some(session) = &self.session else { return };
        let fresh = session.progress.drain_completed();
        if fresh.is_empty() {
            return;
        }

        self.streamed.extend(fresh);
        // A parent supersedes children it already contains, so the list stays
        // a set of non-overlapping totals rather than double-counting.
        self.streamed
            .sort_by_key(|done| std::cmp::Reverse(done.allocated));
        let mut kept: Vec<Finished> = Vec::with_capacity(self.streamed.len());
        for done in std::mem::take(&mut self.streamed) {
            if !kept.iter().any(|other| done.path.starts_with(&other.path)) {
                kept.push(done);
            }
        }
        kept.truncate(STREAM_KEEP);
        self.streamed = kept;
    }

    /// Take a finished scan: the comparison `land_in_background` worked out,
    /// then the tree itself.
    ///
    /// A cancelled scan is a partial count: it lands without a diff and does
    /// not count as a watch round.
    fn absorb_scan(&mut self, landed: Landed) {
        let Landed {
            tree,
            diff,
            baseline,
        } = landed;
        let cancelled = tree.scan_state() == ScanState::Cancelled;

        // `None` keeps the comparison already showing.
        let replacement = match &mut self.watch {
            Some(watch) if cancelled => {
                watch.last_scan = Instant::now();
                Some(None)
            }
            Some(watch) => {
                watch.last_scan = Instant::now();
                watch.rounds += 1;
                match baseline {
                    // The first pass is the baseline; there is nothing to
                    // compare it against yet.
                    Some(baseline) => {
                        watch.baseline = Some(Arc::new(baseline));
                        None
                    }
                    None => Some(diff),
                }
            }
            None => Some(diff),
        };
        if let Some(diff) = replacement {
            self.set_diff(diff);
        }

        // The tree being replaced can be a whole disk.
        if let Some(previous) = self.tree.replace(tree) {
            drop_in_background(previous);
        }
        self.changed_since_scan = false;
        // A stop that came too late to cut the count short said so, or
        // promised a partial count; the full count is here now.
        if self.status.as_deref() == Some(FINISHING)
            || !cancelled
                && matches!(
                    self.status.as_deref(),
                    Some(STOPPED_SHORT | STOPPED_OVER_SNAPSHOT)
                )
        {
            self.status = None;
        }
        self.touch();
        // Whatever is on screen is now the real thing.
        self.provisional = None;
        self.counted_since_snapshot.clear();
        self.streamed.clear();
        // A view already on screen (a snapshot painted while the scan ran)
        // stays whichever one the user stepped to.
        if self.mode == ScanMode::Fresh && self.view == View::Scanning {
            self.view = self.browsing_view.into();
        }
        // A stopped scan may not have reached the folder on screen, which a
        // focus scan had filled in ahead of it.
        self.step_out_of_a_vanished_folder();
        // A snapshot's tree is pruned, so the cursor may have been sitting on
        // a row that the full scan renders differently.
        let rows = self.rows().len();
        self.selection = self.selection.min(rows.saturating_sub(1));
    }

    /// Frame counter, so overlays can animate without threading it through
    /// every draw call.
    pub fn tick(&self) -> usize {
        self.tick
    }

    /// Switch between "what is big" and "what changed".
    pub fn toggle_metric(&mut self) {
        if self.diff.is_none() {
            let stopped = self
                .tree
                .as_ref()
                .is_some_and(|tree| tree.scan_state() == ScanState::Cancelled);
            self.status = Some(if stopped {
                "scan was stopped — no growth comparison for a partial count".into()
            } else if let Some(watch) = &self.watch {
                if watch.baseline.is_none() {
                    "waiting for the first rescan".into()
                } else {
                    "waiting for the next rescan".into()
                }
            } else if self.snapshot_kept
                || self.provisional.is_some()
                    && self
                        .session
                        .as_ref()
                        .is_some_and(|session| session.discard_on_finish)
            {
                STOPPED_OVER_SNAPSHOT.into()
            } else if self.provisional.is_some() && self.session.is_some() {
                "comparing with the snapshot once this scan lands".into()
            } else if self.changed_since_scan {
                "totals changed since the scan — R rescans and compares again".into()
            } else if self.settings.record_snapshots {
                "no earlier scan to compare against — this one is now the baseline".into()
            } else {
                "no earlier scan to compare against".into()
            });
            return;
        }
        self.metric = match self.metric {
            Metric::Size => Metric::Growth,
            Metric::Growth => Metric::Size,
        };
        self.selection = 0;
        self.status = Some(format!("showing {}", self.metric.label()));
    }

    /// The change tree node for wherever the user is standing.
    pub fn current_change(&self) -> Option<&duzo_core::diff::ChangeNode> {
        self.diff.as_ref()?.tree.find(&self.cwd)
    }

    pub fn showing_growth(&self) -> bool {
        self.metric == Metric::Growth && self.diff.is_some()
    }

    pub fn cancel_scan(&mut self) {
        if let Some(session) = &self.session {
            session.cancel.cancel();
        }
    }

    /// Stop a background scan (`r`, `R`, the automatic rescan after a drifted
    /// deletion, or the scan correcting a snapshot) from the browsing views.
    /// Also re-enables deletion, which a running scan otherwise refuses,
    /// unless a snapshot stays on screen.
    ///
    /// A subtree scan (`r`, or a drift rescan scoped to one directory) still
    /// has the pre-scan folder to fall back on, so the folder already on
    /// screen is what the user is looking at, and stays as it was — apart
    /// from folders a focus scan filled in meanwhile, which keep fresh counts.
    ///
    /// A session rooted at the scan root (`R`, `r` at the root, or a drift
    /// rescan that resolves to a full [`refresh`](Self::refresh)) already
    /// trimmed the tree on screen in place with [`trim_to_visible`] the moment it began
    /// — there is no earlier tree here to hold on to. Cancelling one of those
    /// instead has to behave like `Esc` on the scanning screen: let the
    /// partial result land through the normal `poll_scan` -> `absorb_scan`
    /// path, so whatever the scan counted before it stopped replaces the
    /// stub instead of the stub itself landing, the stub never becomes what a
    /// snapshot records or what a mark or deletion sees, and watch mode's
    /// clock moves the same way a finished scan moves it instead of firing
    /// again the moment this frame ends.
    ///
    /// A scan of a new place painted from a snapshot trimmed nothing, and the
    /// snapshot shows more than a stopped count would, so that stop is
    /// discarded like a folder rescan's and the snapshot stays.
    pub fn cancel_background_scan(&mut self) {
        if self.scan_counted()
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.kind == SessionKind::Root)
        {
            self.status = Some(FINISHING.into());
            return;
        }
        let Some(session) = &mut self.session else {
            return;
        };
        // Over a snapshot painted for a new place, what the scan counted so far
        // is less than the snapshot already shows: keep the snapshot and drop
        // the partial count. A rescan trimmed what it rescans, so it lands.
        let over_snapshot = self.provisional.is_some() && self.mode == ScanMode::Fresh;
        if session.kind == SessionKind::Root && !over_snapshot {
            session.cancel.cancel();
            self.status = Some(STOPPED_SHORT.into());
            return;
        }
        session.cancel.cancel();
        session.discard_on_finish = true;
        self.status = Some(STOPPING.into());
    }

    /// Whether the running scan has counted everything and is only being
    /// compared (see `land_in_background`): too late to stop it short.
    pub fn scan_counted(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| !session.cancel.is_cancelled() && session.progress.is_finished())
    }

    /// Whether quitting now would lose a snapshot the landing has yet to
    /// write. Not tied to whether the scan was stopped: one stopped in the
    /// instant after it finished counting still lands whole and records.
    pub fn saving(&self) -> bool {
        self.session.as_ref().is_some_and(|session| {
            session.progress.is_finished() && session.handle.as_ref().is_some_and(Landing::saving)
        })
    }

    pub fn current_entry(&self) -> Option<&DiskEntry> {
        self.tree.as_ref()?.resolve(&self.cwd)
    }

    pub fn row_options(&self, cap: bool) -> RowOptions {
        RowOptions {
            size_kind: self.settings.size_kind,
            top: cap.then_some(self.settings.top),
            sort: self.sort,
            // An empty query lists what no query does; keyed the same, the
            // search box opening does not build the list again.
            filter: self.search.clone().filter(|query| !query.is_empty()),
        }
    }

    /// What a list for the folder on screen is built from.
    fn list_key(&self) -> ListKey {
        // The explorer draws every wedge, so it must not fold the tail into
        // an "Other" row that has no path to open.
        let cap = self.view != View::Explorer;
        ListKey {
            cwd: self.cwd.clone(),
            revision: self.revision,
            growth: self.showing_growth(),
            options: self.row_options(cap),
        }
    }

    /// Rows for the current directory, built once per folder, tree revision,
    /// view and row options: every frame draws them and most keys look them
    /// up. Shared, so callers can hold them while mutating the app.
    pub fn rows(&self) -> Arc<Vec<Row>> {
        let key = self.list_key();
        let cache = if key.options.filter.is_some() {
            &self.search_rows
        } else {
            &self.rows_cache
        };
        cache.get_or_build(key, |key| {
            Arc::new(if key.growth {
                self.current_change()
                    .map(|node| model::growth_rows(node, &key.options))
                    .unwrap_or_default()
            } else {
                self.current_entry()
                    .map(|entry| model::rows(&self.cwd, entry, &key.options))
                    .unwrap_or_default()
            })
        })
    }

    /// What the explorer's legend measures over every row — the widest amount
    /// it lines up and the biggest change it colours by — kept like the rows
    /// themselves.
    pub fn legend_measure(&self) -> (usize, i64) {
        let key = (self.list_key(), self.settings.unit);
        let cache = if key.0.options.filter.is_some() {
            &self.search_measure
        } else {
            &self.measure_cache
        };
        cache.get_or_build(key, |(list, unit)| {
            crate::tui::explorer::measure(&self.rows(), list.growth, *unit)
        })
    }

    pub fn selected_row(&self) -> Option<Row> {
        self.rows().get(self.selection).cloned()
    }

    /// The overview's "Largest items" for the folder on screen, with their
    /// sizes, kept like `rows`: finding them walks two levels of the tree.
    pub fn largest_items(&self, count: usize) -> Largest {
        let kind = self.settings.size_kind;
        let key = (self.cwd.clone(), self.revision, kind, count);
        self.largest_cache.get_or_build(key, |_| {
            Arc::new(
                self.current_entry()
                    .map(|entry| {
                        model::largest_items(&self.cwd, entry, count, kind)
                            .into_iter()
                            .map(|(path, item)| (path, item.size(kind)))
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        })
    }

    pub fn move_selection(&mut self, delta: isize) {
        let count = self.rows().len();
        if count == 0 {
            self.selection = 0;
            return;
        }
        let next = self.selection as isize + delta;
        self.selection = next.clamp(0, count as isize - 1) as usize;
    }

    pub fn select_first(&mut self) {
        self.selection = 0;
    }

    pub fn select_last(&mut self) {
        self.selection = self.rows().len().saturating_sub(1);
    }

    /// Descend into the selected directory.
    pub fn open_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(path) = row.path else {
            self.status = Some("\"Other\" groups several entries — raise --top to see them".into());
            return;
        };
        if !row.is_dir {
            self.status = Some(format!("{} is a file", model::display_path(&path)));
            return;
        }
        self.cwd = path.clone();
        self.selection = 0;
        self.search = None;
        self.search_active = false;
    }

    /// Back up one level. At the top of the tree that means back to the disk
    /// list, when that is where this scan came from.
    pub fn go_up(&mut self) {
        if self.cwd == self.root {
            if self.from_picker {
                self.return_to_picker();
            } else {
                self.status = Some(format!(
                    "top of this scan — run duzo on {} to go higher",
                    self.root
                        .parent()
                        .map(model::display_path)
                        .unwrap_or_else(|| "the parent".into())
                ));
            }
            return;
        }
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return;
        };
        // Remember where we came from so going up lands on the child we left.
        let leaving = self.cwd.clone();
        self.cwd = parent;
        self.search = None;
        self.search_active = false;
        self.selection = self
            .rows()
            .iter()
            .position(|row| row.path.as_ref() == Some(&leaving))
            .unwrap_or(0);
    }

    /// Ask to delete: everything marked, or whatever is selected if nothing
    /// is marked.
    pub fn request_delete(&mut self) {
        if self.read_only {
            self.status = Some("duzo is running read-only".into());
            return;
        }
        if self.session.is_some() {
            self.status = Some("wait for the scan to finish before deleting".into());
            return;
        }
        // A snapshot records no file identities to check a deletion against.
        if self.provisional.is_some() {
            self.status = Some("showing a snapshot — press R to rescan before deleting".into());
            return;
        }
        let Some(tree) = &self.tree else { return };

        let paths: Vec<PathBuf> = if self.marks.is_empty() {
            self.selected_row()
                .and_then(|row| row.path)
                .into_iter()
                .collect()
        } else {
            let mut marked: Vec<PathBuf> = self.marks.iter().cloned().collect();
            marked.sort();
            let mut pruned: Vec<PathBuf> = Vec::new();
            for path in marked {
                if !pruned.iter().any(|parent| path.starts_with(parent)) {
                    pruned.push(path);
                }
            }
            pruned
        };

        let targets: Vec<Target> = paths
            .into_iter()
            .filter_map(|path| {
                let entry = tree.resolve(&path)?;
                Some(Target {
                    size: entry.size(self.settings.size_kind),
                    allocated_size: entry.allocated_size,
                    filesystem_identity: entry.filesystem_identity(),
                    is_dir: entry.is_dir(),
                    path,
                })
            })
            .collect();

        if targets.is_empty() {
            self.status = Some("nothing selected to delete".into());
            return;
        }

        self.confirm = Some(Confirm {
            targets,
            ..Default::default()
        });
    }

    pub fn cancel_delete(&mut self) {
        self.confirm = None;
        self.status = Some("nothing was deleted".into());
    }

    pub fn type_confirmation(&mut self, ch: char) {
        if let Some(confirm) = &mut self.confirm {
            confirm.typed.push(ch);
            confirm.nagged = false;
        }
    }

    pub fn untype_confirmation(&mut self) {
        if let Some(confirm) = &mut self.confirm {
            confirm.typed.pop();
            confirm.nagged = false;
        }
    }

    /// Go through with it, if the word has been typed in full.
    ///
    /// The work happens on another thread: unlinking a few hundred thousand
    /// files takes long enough that doing it here would freeze the screen with
    /// no way to tell whether anything was happening.
    pub fn commit_delete(&mut self) {
        if self.session.is_some() {
            self.status = Some("wait for the scan to finish before deleting".into());
            return;
        }
        let Some(confirm) = &mut self.confirm else {
            return;
        };
        if !confirm.is_armed() {
            confirm.nagged = true;
            return;
        }

        let targets = std::mem::take(&mut confirm.targets);
        self.confirm = None;
        self.outcomes.clear();

        let root_identity = self.tree.as_ref().and_then(DiskEntry::filesystem_identity);
        let (progress, cancel, handle) =
            deletion::delete_in_background(targets, self.root.clone(), root_identity);
        self.deleting = Some(Deleting {
            progress,
            cancel,
            handle: Some(handle),
            stopping: false,
        });
    }

    /// Ask the running deletion to stop once it finishes the file it is on.
    pub fn stop_delete(&mut self) {
        if let Some(deleting) = &mut self.deleting {
            deleting.cancel.cancel();
            deleting.stopping = true;
        }
    }

    /// Collect a finished deletion and fold the result back into the tree.
    pub fn poll_delete(&mut self) {
        let Some(deleting) = &mut self.deleting else {
            return;
        };
        let done = deleting
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished());
        if !done {
            return;
        }

        let Some(handle) = deleting.handle.take() else {
            return;
        };
        let outcomes = handle.join().unwrap_or_default();
        self.deleting = None;
        self.apply_outcomes(outcomes);
    }

    fn apply_outcomes(&mut self, outcomes: Vec<Outcome>) {
        let needs_rescan = outcomes.iter().any(Outcome::needs_rescan);
        // What the tree still believed each deleted target held, captured on
        // the way out — `rescan_drifted` needs it to tell a hard link that
        // survived somewhere else in the tree (freed short of that number,
        // could be anywhere) from a target whose own accounting just needed
        // its parent rescanned.
        let mut removed_allocated: HashMap<PathBuf, u64> = HashMap::new();
        // Correct the totals in place rather than paying for a rescan.
        let mut removed_trees = Vec::new();
        if let Some(tree) = &mut self.tree {
            for outcome in &outcomes {
                if let Outcome::Deleted { path, .. } = outcome
                    && let Some(removed) = tree.remove(path)
                {
                    removed_allocated.insert(path.clone(), removed.allocated_size);
                    removed_trees.push(removed);
                }
            }
            self.climb_to_a_listed_folder();
        }
        // What was deleted can be a big folder; one thread frees all of it.
        if !removed_trees.is_empty() {
            drop_in_background(removed_trees);
        }

        // Invalidate diff since tree has mutated, avoiding stale entries in growth view.
        if outcomes.iter().any(Outcome::changed_tree) {
            self.finish_tree_mutation();
        }

        let freed = deletion::freed(&outcomes);
        let deleted = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Outcome::Deleted { .. }))
            .count();
        let refused = outcomes.len() - deleted;

        let deleted_paths: Vec<&Path> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                Outcome::Deleted { path, .. } => Some(path.as_path()),
                _ => None,
            })
            .collect();
        self.marks.retain(|mark| {
            !deleted_paths
                .iter()
                .any(|deleted| mark.starts_with(deleted))
        });

        self.status = Some(deletion_status(
            deleted,
            refused,
            freed,
            &outcomes,
            self.settings.unit,
        ));
        self.outcomes = outcomes;

        if deleted == 0 && !self.outcomes.is_empty() {
            self.details = true;
        }

        // The row under the cursor may have just been removed.
        let rows = self.rows().len();
        self.selection = self.selection.min(rows.saturating_sub(1));

        if needs_rescan {
            self.rescan_drifted(&self.outcomes.clone(), &removed_allocated);
        }
    }

    /// Swap in a new comparison. The old one mirrors the whole scanned tree,
    /// so it is freed off the UI thread.
    fn set_diff(&mut self, diff: Option<Diff>) {
        if let Some(old) = std::mem::replace(&mut self.diff, diff) {
            drop_in_background(old);
        }
    }

    /// Not recorded as a snapshot: the tree is no longer a scan, and a
    /// graft can count a hard link twice (see `DiskEntry::graft`).
    fn finish_tree_mutation(&mut self) {
        // Only promise a comparison that a full rescan can bring back.
        self.changed_since_scan |= self.diff.is_some() || self.settings.record_snapshots;
        self.set_diff(None);
        // Watch mode owns the growth metric and its baseline; the next tick
        // restores the diff.
        if self.watch.is_none() && self.metric == Metric::Growth {
            self.metric = Metric::Size;
        }
        self.touch();
    }

    /// Drop everything belonging to the current scan and show the disks again.
    pub fn return_to_picker(&mut self) {
        self.cancel_scan();
        if let Some(landing) = self.session.take().and_then(|session| session.handle) {
            landing.abandon();
        }
        self.drop_focus();
        self.view = View::Picker;
        if let Some(tree) = self.tree.take() {
            drop_in_background(tree);
        }
        self.set_diff(None);
        self.touch();
        self.metric = Metric::Size;
        self.root = PathBuf::new();
        self.cwd = PathBuf::new();
        self.selection = 0;
        self.search = None;
        self.search_active = false;
        // Marks are paths inside the scan that is being left behind.
        self.marks.clear();
        self.status = None;
        // Capacities move while you are busy looking at one disk. Reading
        // them waits on every mounted volume, so it happens off the UI
        // thread; the list already on screen stands in until it lands.
        let all = self.settings.show_all_filesystems;
        self.filesystems_refresh = Some(std::thread::spawn(move || duzo_core::mounts::list(all)));
    }

    /// Take the disk list `return_to_picker` started reading, once it is in.
    pub fn poll_filesystems(&mut self) {
        if !self
            .filesystems_refresh
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            return;
        }
        if let Some(Ok(filesystems)) = self.filesystems_refresh.take().map(JoinHandle::join) {
            // Keep the cursor on the same disk when one was added or removed
            // above it while the list was being read.
            let selected = self
                .filesystems
                .get(self.picker_index)
                .map(|fs| fs.mount_point.clone());
            self.filesystems = filesystems;
            self.picker_index = selected
                .and_then(|mount| {
                    self.filesystems
                        .iter()
                        .position(|fs| fs.mount_point == mount)
                })
                .unwrap_or(self.picker_index)
                .min(self.filesystems.len().saturating_sub(1));
        }
    }

    /// Show the selected entry in the desktop's file manager.
    pub fn reveal_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some(path) = row.path else {
            self.status = Some("\"Other\" is a group, not a folder".into());
            return;
        };

        match crate::reveal::reveal(&path, row.is_dir) {
            Ok(()) => {
                let shown = crate::reveal::folder_for(&path, row.is_dir).unwrap_or(path);
                self.status = Some(format!(
                    "opened {} in {}",
                    model::display_path(&shown),
                    crate::reveal::manager_name()
                ));
            }
            Err(error) => self.status = Some(error),
        }
    }

    /// Put the path under the cursor on the clipboard — the way out of duzo
    /// and into whatever you actually meant to do with the folder you found.
    pub fn copy_selected_path(&mut self) {
        // A group row stands for no single path, and an empty directory has no
        // row at all; the directory being looked at is the honest answer to
        // both.
        let path = self
            .selected_row()
            .and_then(|row| row.path)
            .unwrap_or_else(|| self.cwd.clone());

        // The clipboard gets the real path, the message gets the short one.
        self.status = Some(match crate::clipboard::copy(&path.to_string_lossy()) {
            Ok(()) => format!("copied {}", model::display_path(&path)),
            Err(error) => error,
        });
    }

    pub fn toggle_mark(&mut self) {
        let Some(path) = self.selected_row().and_then(|row| row.path) else {
            return;
        };
        if !self.marks.remove(&path) {
            self.marks.insert(path);
        }
    }

    pub fn marked_total(&self) -> u64 {
        let Some(tree) = &self.tree else { return 0 };
        self.marks
            .iter()
            .filter_map(|path| tree.resolve(path))
            .map(|entry| entry.size(self.settings.size_kind))
            .sum()
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.status = Some(format!("sorted by {}", self.sort.label()));
    }

    pub fn toggle_size_kind(&mut self) {
        self.settings.size_kind = match self.settings.size_kind {
            SizeKind::Allocated => SizeKind::Apparent,
            SizeKind::Apparent => SizeKind::Allocated,
        };
        self.live_size_kind.set(self.settings.size_kind);
        self.status = Some(match self.settings.size_kind {
            SizeKind::Allocated => "showing space used on disk".into(),
            SizeKind::Apparent => "showing apparent file sizes".into(),
        });
    }

    pub fn begin_search(&mut self) {
        self.search_active = true;
        self.search.get_or_insert_with(String::new);
    }

    pub fn push_search(&mut self, ch: char) {
        if let Some(search) = &mut self.search {
            search.push(ch);
            self.selection = 0;
        }
    }

    pub fn pop_search(&mut self) {
        if let Some(search) = &mut self.search {
            search.pop();
            self.selection = 0;
        }
    }

    pub fn clear_search(&mut self) {
        self.search = None;
        self.search_active = false;
        self.selection = 0;
    }

    pub fn rescan(&mut self) {
        if self.refuse_if_scanning() {
            return;
        }
        let target = self.cwd.clone();
        if !target.as_os_str().is_empty() {
            if target == self.root {
                self.refresh();
            } else {
                self.scan_subtree(target);
            }
        }
    }

    pub fn rescan_root(&mut self) {
        if self.refuse_if_scanning() {
            return;
        }
        if !self.root.as_os_str().is_empty() {
            self.refresh();
        }
    }

    /// Say why a rescan key did nothing when one is already running.
    fn refuse_if_scanning(&mut self) -> bool {
        if self.session.is_none() {
            return false;
        }
        self.status = Some("a scan is already running".into());
        true
    }

    /// The path components that make up the explorer's breadcrumb.
    pub fn breadcrumb_parts(&self) -> Vec<String> {
        let volume = self
            .filesystem
            .as_ref()
            .map(|fs| fs.name.clone())
            .unwrap_or_else(|| model::display_path(&self.root));

        let relative = self.cwd.strip_prefix(&self.root).ok();
        let mut parts = vec![volume];
        if let Some(relative) = relative {
            parts.extend(
                relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().to_string()),
            );
        }
        parts
    }

    /// The sunburst for the current directory, plus the table taking a path
    /// back to the wedge that stands for it.
    ///
    /// Building it walks `rings` levels of the tree and clones a path per node,
    /// which is far too much to redo for every keystroke — so the last few
    /// are kept until something they were built from moves.
    pub fn radial_tree(&self, rings: usize) -> Arc<RadialCache> {
        let key = RadialKey {
            cwd: self.cwd.clone(),
            rings,
            size_kind: self.settings.size_kind,
            metric: self.metric,
            revision: self.revision,
        };
        self.radial
            .get_or_build(key, |key| Arc::new(self.build_radial(key)))
    }

    fn build_radial(&self, key: &RadialKey) -> RadialCache {
        let mut ids = Vec::new();

        let root = if self.showing_growth() {
            match self.current_change() {
                Some(change) => {
                    let scale = biggest_change(change);
                    build_growth_radial(change, key.rings, scale, &mut ids)
                }
                None => RadialNode::leaf(0, "", 0),
            }
        } else {
            match self.current_entry() {
                Some(entry) => {
                    // A wedge's arc is its share of the whole chart, so
                    // anything smaller than this is too thin to be drawn.
                    let total = entry.size(key.size_kind) as f64;
                    let drawable = (total * LayoutOptions::default().min_span) as u64;
                    build_radial(
                        &self.cwd,
                        entry,
                        key.size_kind,
                        key.rings,
                        drawable,
                        &mut ids,
                    )
                }
                None => RadialNode::leaf(0, "", 0),
            }
        };

        RadialCache {
            rings: key.rings,
            root,
            ids: ids
                .into_iter()
                .enumerate()
                .map(|(id, path)| (path, id))
                .collect(),
            visible: OnceLock::new(),
            all: OnceLock::new(),
        }
    }

    /// Note that the tree behind the view has changed, so anything derived
    /// from it has to be built again.
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        // Nothing built from the tree as it was is asked for again.
        self.radial.clear();
        self.rows_cache.clear();
        self.search_rows.clear();
        self.measure_cache.clear();
        self.search_measure.clear();
        self.largest_cache.clear();
        self.shortfall_cache.clear();
    }

    pub fn pick_selected_filesystem(&mut self) {
        let Some(fs) = self.filesystems.get(self.picker_index) else {
            return;
        };
        let mount = fs.mount_point.clone();
        self.from_picker = true;
        self.start_scan(&mount);
    }

    pub fn move_picker(&mut self, delta: isize) {
        if self.filesystems.is_empty() {
            return;
        }
        let next = self.picker_index as isize + delta;
        self.picker_index = next.clamp(0, self.filesystems.len() as isize - 1) as usize;
    }
}

/// The deepest directory at or above both `a` and `b`, but never above `root`
/// — the walk stops there even if the two targets otherwise share nothing.
fn common_ancestor(a: &Path, b: &Path, root: &Path) -> PathBuf {
    let mut candidate = a.to_path_buf();
    while !b.starts_with(&candidate) {
        if candidate == root || !candidate.pop() {
            return root.to_path_buf();
        }
    }
    candidate
}

/// True when `entry` is a directory whose numbers on screen are missing some
/// of what it holds. A scanned directory counts itself plus exactly what its
/// children count, and stubs, snapshot pruning and grafts keep `items`
/// exact, so any shortfall is a listing that was cut. A directory a stopped
/// scan did not finish is `Cancelled`, whether or not it lists anything.
///
/// A folder at the `--depth` limit is short too (the scan counted what it
/// did not keep); `ensure_focus` leaves those alone through
/// `subtree_scan_options`, since nothing below them would be kept either.
fn lists_less_than_it_holds(entry: &DiskEntry) -> bool {
    if entry.entry_type != duzo_core::EntryType::Directory {
        return false;
    }
    !entry.lists_everything() || entry.scan_state() == ScanState::Cancelled
}

/// Cut `entry`, at `path`, down to the part the screen can still reach with
/// the cursor at `cwd`: `below` levels under the cursor, and enough of the way
/// back up that leaving the directory lists what it listed before.
///
/// Everything else goes into `cut`, for the caller to free elsewhere. The
/// numbers that survive are the same numbers the last scan produced — this
/// trades depth the view is not showing for the memory a second full tree
/// would need. It works in place: copying the kept part was a pause on the UI
/// thread at every rescan.
fn trim_to_visible(
    path: &mut PathBuf,
    entry: &mut DiskEntry,
    cwd: &Path,
    below: usize,
    cut: &mut Vec<Vec<DiskEntry>>,
) {
    if path.starts_with(cwd) {
        // At or under the cursor: a few levels is everything that is drawn.
        if below == 0 {
            cut_children(entry, cut);
            return;
        }
        for child in &mut entry.children {
            path.push(child.name_os());
            trim_to_visible(path, child, cwd, below - 1, cut);
            path.pop();
        }
    } else if cwd.starts_with(&*path) {
        // Above the cursor: keep every child, so going back up still lists the
        // same rows, but only follow the one that leads back down to it.
        for child in &mut entry.children {
            path.push(child.name_os());
            if cwd.starts_with(&*path) {
                trim_to_visible(path, child, cwd, below, cut);
            } else {
                cut_children(child, cut);
            }
            path.pop();
        }
    } else {
        cut_children(entry, cut);
    }
}

/// Move `entry`'s children into `cut`; a file's empty list is not worth it.
fn cut_children(entry: &mut DiskEntry, cut: &mut Vec<Vec<DiskEntry>>) {
    if !entry.children.is_empty() {
        cut.push(std::mem::take(&mut entry.children));
    }
}

/// The sunburst under `entry`, `rings` deep. Every child of the folder on
/// screen (id 0) gets a node, since any of them can be the selected row, which
/// the layout places however thin it is. Further out, entries under
/// `drawable` bytes are left out: the layout skips them and everything inside
/// them, and a big folder holds hundreds of thousands of them.
fn build_radial(
    path: &Path,
    entry: &DiskEntry,
    kind: SizeKind,
    rings: usize,
    drawable: u64,
    ids: &mut Vec<PathBuf>,
) -> RadialNode {
    let id = ids.len();
    ids.push(path.to_path_buf());

    let mut children = Vec::new();
    if rings > 0 && entry.size(kind) >= drawable {
        let mut sorted: Vec<&DiskEntry> = entry
            .children
            .iter()
            .filter(|child| id == 0 || child.size(kind) >= drawable)
            .collect();
        sorted.sort_by(|a, b| {
            b.size(kind)
                .cmp(&a.size(kind))
                .then_with(|| a.name().cmp(&b.name()))
        });
        children = sorted
            .into_iter()
            .map(|child| {
                build_radial(
                    &path.join(child.name_os()),
                    child,
                    kind,
                    rings - 1,
                    drawable,
                    ids,
                )
            })
            .collect();
    }

    RadialNode {
        id,
        label: entry.name().to_string(),
        size: entry.size(kind),
        color: None,
        children,
    }
}

/// The same sunburst, but wedges are sized by how much moved and coloured by
/// which way: growth glows, shrinkage cools, and anything that stayed put
/// recedes into the background.
fn build_growth_radial(
    node: &duzo_core::diff::ChangeNode,
    rings: usize,
    scale: i64,
    ids: &mut Vec<PathBuf>,
) -> RadialNode {
    let id = ids.len();
    ids.push(node.change.path.clone());

    let mut children = Vec::new();
    if rings > 0 {
        let mut sorted: Vec<&duzo_core::diff::ChangeNode> = node
            .children
            .iter()
            .filter(|child| child.change.delta != 0)
            .collect();
        sorted.sort_by_key(|child| std::cmp::Reverse(child.change.delta.abs()));
        children = sorted
            .into_iter()
            .map(|child| build_growth_radial(child, rings - 1, scale, ids))
            .collect();
    }

    RadialNode {
        id,
        label: node
            .change
            .path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        size: node.change.delta.unsigned_abs(),
        color: Some(palette::growth_color(node.change.delta, scale)),
        children,
    }
}

fn biggest_change(node: &duzo_core::diff::ChangeNode) -> i64 {
    node.children
        .iter()
        .map(|child| child.change.delta.abs())
        .max()
        .unwrap_or(0)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use duzo_core::EntryType;

    /// `land_in_background`, except that a scan which has already finished
    /// has also landed when this returns, as tests that poll once expect.
    fn landing(scan: JoinHandle<Result<DiskEntry>>, compare: Compare) -> Landing {
        let finished = scan.is_finished();
        let landing = land_in_background(scan, compare);
        while finished && !landing.is_finished() {
            std::thread::yield_now();
        }
        landing
    }

    /// Hand `scan` to the running session the way `begin` and
    /// `scan_subtree` do, so it lands through the same comparison.
    fn hand_over(app: &mut App, scan: JoinHandle<Result<DiskEntry>>) {
        let kind = app.session.as_ref().unwrap().kind;
        let landing = landing(scan, app.compare_for(kind));
        app.session.as_mut().unwrap().handle = Some(landing);
    }

    fn settings() -> Settings {
        Settings {
            size_kind: SizeKind::Allocated,
            unit: Unit::Decimal,
            top: 20,
            scan_options: ScanOptions::default(),
            show_all_filesystems: false,
            // Tests must never touch the user's real snapshot history.
            record_snapshots: false,
        }
    }

    fn entry(path: &str, size: u64, children: Vec<DiskEntry>) -> DiskEntry {
        let mut entry = DiskEntry::new(PathBuf::from(path), EntryType::Directory);
        entry.allocated_size = size;
        entry.apparent_size = size;
        entry.children = children;
        entry
    }

    /// An app with a canned tree, skipping the real scan.
    fn app_with_tree() -> App {
        let tree = entry(
            "/files-test-root",
            1000,
            vec![
                entry(
                    "/files-test-root/big",
                    700,
                    vec![entry("/files-test-root/big/inner", 500, vec![])],
                ),
                entry("/files-test-root/small", 300, vec![]),
            ],
        );
        let mut app = App::new(settings(), false);
        app.root = PathBuf::from("/files-test-root");
        app.cwd = PathBuf::from("/files-test-root");
        app.tree = Some(tree);
        app.view = View::Overview;
        app
    }

    /// `/files-test-root` -> `a` -> `deep` -> `deeper`, with a sibling at every level, so
    /// a stub can be checked for both what it keeps and what it lets go.
    fn deep_tree() -> DiskEntry {
        entry(
            "/files-test-root",
            1000,
            vec![
                entry(
                    "/files-test-root/a",
                    700,
                    vec![
                        entry(
                            "/files-test-root/a/deep",
                            600,
                            vec![entry(
                                "/files-test-root/a/deep/deeper",
                                500,
                                vec![entry("/files-test-root/a/deep/deeper/deepest", 400, vec![])],
                            )],
                        ),
                        entry(
                            "/files-test-root/a/sibling",
                            100,
                            vec![entry("/files-test-root/a/sibling/x", 1, vec![])],
                        ),
                    ],
                ),
                entry(
                    "/files-test-root/b",
                    300,
                    vec![entry("/files-test-root/b/hidden", 50, vec![])],
                ),
            ],
        )
    }

    /// `deep_tree` cut down for the cursor at `cwd`.
    fn trimmed(cwd: &str, below: usize) -> DiskEntry {
        let mut tree = deep_tree();
        trim_to_visible(
            &mut PathBuf::from("/files-test-root"),
            &mut tree,
            Path::new(cwd),
            below,
            &mut Vec::new(),
        );
        tree
    }

    #[test]
    fn a_stub_keeps_the_levels_under_the_cursor() {
        let stub = trimmed("/files-test-root/a", 2);
        let a = stub.child("a").unwrap();

        // Two levels under the cursor survive with their sizes intact...
        assert_eq!(a.allocated_size, 700);
        let deeper = a.child("deep").unwrap().child("deeper").unwrap();
        assert_eq!(deeper.allocated_size, 500);
        // ...and the third does not.
        assert!(
            deeper.children.is_empty(),
            "the stub stops at the depth asked for"
        );
    }

    #[test]
    fn a_stub_keeps_enough_to_go_back_up() {
        let stub = trimmed("/files-test-root/a", 2);

        // Leaving `/files-test-root/a` must still list the same rows it listed before,
        // with the same numbers, even though nothing under them is kept.
        let b = stub.child("b").unwrap();
        assert_eq!(b.allocated_size, 300);
        assert!(
            b.children.is_empty(),
            "a branch off the path is not followed"
        );
    }

    #[test]
    fn a_stub_at_the_root_is_bounded_by_depth() {
        let stub = trimmed("/files-test-root", 1);

        assert_eq!(stub.allocated_size, 1000);
        assert_eq!(stub.children.len(), 2);
        // One level down, and no further.
        assert!(stub.children.iter().all(|child| child.children.is_empty()));
    }

    #[test]
    fn opening_a_directory_descends_and_resets_the_selection() {
        let mut app = app_with_tree();
        app.selection = 1;
        app.selection = 0; // "big" is largest, so it is first
        app.open_selected();

        assert_eq!(app.cwd, PathBuf::from("/files-test-root/big"));
        assert_eq!(app.selection, 0);
        assert_eq!(app.rows().len(), 1);
    }

    #[test]
    fn going_up_lands_back_on_the_directory_just_left() {
        let mut app = app_with_tree();
        app.cwd = PathBuf::from("/files-test-root/small");
        app.go_up();

        assert_eq!(app.cwd, PathBuf::from("/files-test-root"));
        assert_eq!(app.selected_row().unwrap().name, "small");
    }

    #[test]
    fn the_scan_root_is_the_floor() {
        let mut app = app_with_tree();
        app.go_up();
        assert_eq!(app.cwd, PathBuf::from("/files-test-root"));
        assert!(app.status.is_some());
    }

    #[test]
    fn selection_stays_inside_the_list() {
        let mut app = app_with_tree();
        app.move_selection(-5);
        assert_eq!(app.selection, 0);
        app.move_selection(99);
        assert_eq!(app.selection, 1);
    }

    #[test]
    fn marks_toggle_and_total_up() {
        let mut app = app_with_tree();
        app.toggle_mark();
        assert_eq!(app.marked_total(), 700);
        app.toggle_mark();
        assert_eq!(app.marked_total(), 0);
    }

    #[test]
    fn searching_filters_the_rows() {
        let mut app = app_with_tree();
        app.begin_search();
        for ch in "sma".chars() {
            app.push_search(ch);
        }
        assert_eq!(app.rows().len(), 1);
        assert_eq!(app.rows()[0].name, "small");

        app.clear_search();
        assert_eq!(app.rows().len(), 2);
    }

    #[test]
    fn the_breadcrumb_follows_the_current_directory() {
        let mut app = app_with_tree();
        assert_eq!(app.breadcrumb_parts(), ["/files-test-root"]);
        app.cwd = PathBuf::from("/files-test-root/big/inner");
        assert_eq!(app.breadcrumb_parts(), ["/files-test-root", "big", "inner"]);
    }

    /// A fresh scan opens on the saved view, but only if nothing is on screen
    /// yet: someone who stepped to the other view while a snapshot was
    /// painted stays there when the scan lands.
    #[test]
    fn a_landing_scan_keeps_the_view_stepped_to_over_a_snapshot() {
        let mut app = app_with_tree();
        let tree = app.tree.clone().unwrap();
        let landed = |tree: DiskEntry| Landed {
            tree,
            diff: None,
            baseline: None,
        };

        app.view = View::Scanning;
        app.absorb_scan(landed(tree.clone()));
        assert_eq!(app.view, View::Explorer, "nothing was on screen yet");

        app.view = View::Overview;
        app.absorb_scan(landed(tree));
        assert_eq!(app.view, View::Overview);
    }

    #[test]
    fn the_radial_tree_ids_map_back_to_paths() {
        let app = app_with_tree();
        let cache = app.radial_tree(2);

        assert_eq!(cache.root.size, 1000);
        // Largest first, depth first.
        for (id, path) in [
            (0, "/files-test-root"),
            (1, "/files-test-root/big"),
            (2, "/files-test-root/big/inner"),
            (3, "/files-test-root/small"),
        ] {
            assert_eq!(cache.id_of(Path::new(path)), Some(id), "{path}");
        }
        assert_eq!(cache.root.children[0].children[0].id, 2);
    }

    #[test]
    fn the_radial_tree_stops_at_the_ring_limit() {
        let app = app_with_tree();
        let cache = app.radial_tree(1);
        assert!(cache.root.children[0].children.is_empty());
    }

    /// Rebuilding the sunburst for every keystroke is what made holding an
    /// arrow key down drag, so the same directory has to hand back the same
    /// build — and a changed one must not.
    #[test]
    fn the_radial_tree_is_only_rebuilt_when_something_moves() {
        let mut app = app_with_tree();
        let first = app.radial_tree(2).root.size;
        assert_eq!(first, 1000);
        assert_eq!(app.revision, 0, "reading it should change nothing");

        // Moving the cursor leaves the chart alone...
        app.move_selection(1);
        assert_eq!(app.revision, 0);

        // ...but editing the tree does not.
        app.apply_outcomes(vec![Outcome::Deleted {
            path: PathBuf::from("/files-test-root/small"),
            freed: 300,
            tree_needs_rescan: false,
            links_survived: false,
        }]);
        assert_eq!(app.revision, 1);
        assert_eq!(
            app.radial_tree(2)
                .id_of(Path::new("/files-test-root/small")),
            None
        );
    }

    /// The explorer lays a directory out once and adds the selected sliver
    /// to that, rather than laying it out again for every selection. What it
    /// draws must be what a layout pinned to the selection would have drawn.
    #[test]
    fn the_shared_layout_places_a_selected_sliver_like_a_pinned_layout() {
        let mut children = vec![entry("/root/big", 100_000, vec![])];
        for index in 0..40 {
            let path = format!("/root/tiny{index:02}");
            children.push(entry(
                &path,
                10,
                vec![entry(&format!("{path}/in"), 10, vec![])],
            ));
        }
        let mut app = App::new(settings(), false);
        app.root = PathBuf::from("/root");
        app.cwd = PathBuf::from("/root");
        app.tree = Some(entry("/root", 100_400, children));

        let cache = app.radial_tree(3);
        let visible = cache.segments(None).len();
        for id in 0..cache.ids.len() {
            let pinned = radial::layout(
                &cache.root,
                &LayoutOptions {
                    rings: 3,
                    pinned: Some(id),
                    ..Default::default()
                },
            );
            assert_eq!(*cache.segments(Some(id)), pinned, "selected id {id}");
        }
        assert!(
            cache.segments(Some(5)).len() > visible,
            "a sliver should have been added"
        );
    }

    #[test]
    fn opening_a_file_explains_itself_instead_of_navigating() {
        let mut app = app_with_tree();
        if let Some(tree) = &mut app.tree {
            tree.children[1].entry_type = EntryType::File;
        }
        app.selection = 1;
        app.open_selected();

        assert_eq!(app.cwd, PathBuf::from("/files-test-root"));
        assert!(app.status.unwrap().contains("is a file"));
    }

    #[test]
    fn switching_size_kind_changes_what_rows_report() {
        let mut app = app_with_tree();
        if let Some(tree) = &mut app.tree {
            tree.children[0].apparent_size = 10;
        }
        assert_eq!(app.rows()[0].size, 700);
        app.toggle_size_kind();
        assert_eq!(app.settings.size_kind, SizeKind::Apparent);
        // Now "small" is the biggest by apparent size.
        assert_eq!(app.rows()[0].name, "small");
    }

    #[test]
    fn nested_marks_are_pruned_before_deletion() {
        let mut app = app_with_tree();
        app.marks.insert(PathBuf::from("/files-test-root/big"));
        app.marks
            .insert(PathBuf::from("/files-test-root/big/inner"));
        app.request_delete();

        let confirm = app.confirm.as_ref().unwrap();
        assert_eq!(confirm.targets.len(), 1);
        assert_eq!(
            confirm.targets[0].path,
            PathBuf::from("/files-test-root/big")
        );
    }

    /// Once a scan has counted everything, stopping it has nothing partial to
    /// keep (Esc says it is finishing instead), and quitting waits until its
    /// landing has nothing left to write.
    #[test]
    fn a_counted_scan_that_has_not_landed_is_finishing_and_saving() {
        let mut app = app_with_tree();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _ = wait.recv();
            Ok(DiskEntry::new("/files-test-root", EntryType::Directory))
        });
        let progress = Arc::new(Progress::default());
        app.session = Some(Session {
            scanned_path: PathBuf::from("/files-test-root"),
            kind: SessionKind::Root,
            progress: Arc::clone(&progress),
            cancel: Cancel::new(),
            handle: Some(landing(handle, Compare::Nothing)),
            started: Instant::now(),
            discard_on_finish: false,
        });

        assert!(!app.scan_counted() && !app.saving(), "still counting");
        progress.finish();
        assert!(app.scan_counted() && app.saving(), "counted, not landed");

        release.send(()).unwrap();
        while app.saving() {
            std::thread::yield_now();
        }
        assert!(app.scan_counted(), "landed, not yet collected");
    }

    #[test]
    fn deletion_cannot_start_while_a_scan_can_still_replace_the_tree() {
        let mut app = app_with_tree();
        let handle =
            std::thread::spawn(|| Ok(DiskEntry::new("/files-test-root", EntryType::Directory)));
        app.session = Some(Session {
            scanned_path: PathBuf::from("/files-test-root"),
            kind: SessionKind::Root,
            progress: Arc::new(Progress::default()),
            cancel: Cancel::new(),
            handle: Some(landing(handle, app.compare_for(SessionKind::Root))),
            started: Instant::now(),
            discard_on_finish: false,
        });

        app.request_delete();

        assert!(app.confirm.is_none());
        assert!(app.status.as_deref().unwrap().contains("scan to finish"));
    }

    #[test]
    fn an_armed_confirmation_is_rechecked_if_a_scan_started_after_it_opened() {
        let mut app = app_with_tree();
        app.request_delete();
        app.confirm.as_mut().unwrap().typed = CONFIRM_WORD.to_string();
        let handle =
            std::thread::spawn(|| Ok(DiskEntry::new("/files-test-root", EntryType::Directory)));
        app.session = Some(Session {
            scanned_path: PathBuf::from("/files-test-root"),
            kind: SessionKind::Root,
            progress: Arc::new(Progress::default()),
            cancel: Cancel::new(),
            handle: Some(landing(handle, app.compare_for(SessionKind::Root))),
            started: Instant::now(),
            discard_on_finish: false,
        });

        app.commit_delete();

        assert!(app.deleting.is_none());
        assert!(app.confirm.is_some());
    }

    #[test]
    fn an_armed_confirmation_blocks_a_background_refresh() {
        let mut app = app_with_tree();
        app.request_delete();
        app.confirm.as_mut().unwrap().typed = CONFIRM_WORD.to_string();

        app.refresh();

        assert!(app.session.is_none());
        assert!(app.confirm.is_some());
    }

    #[test]
    fn a_partial_deletion_starts_a_rescan_before_the_tree_is_trusted_again() {
        let mut app = app_with_tree();
        app.metric = Metric::Growth;
        app.diff = Some(duzo_core::diff::diff(
            app.tree.as_ref().unwrap(),
            app.tree.as_ref().unwrap(),
            SizeKind::Allocated,
            0,
            1,
            0,
        ));

        app.apply_outcomes(vec![Outcome::Failed {
            path: PathBuf::from("/files-test-root/big"),
            error: "stopped part-way through".into(),
            removed_bytes: 0,
            removed_entries: 0,
            tree_may_have_changed: true,
            links_survived: false,
        }]);

        assert!(app.session.is_some());
        assert!(app.diff.is_none());
        assert_eq!(app.metric, Metric::Size);
        assert!(
            app.status
                .as_deref()
                .unwrap()
                .contains("deletion incomplete")
        );
        app.cancel_scan();
    }

    #[test]
    fn deletion_accounting_drift_starts_a_rescan() {
        let mut app = app_with_tree();

        app.apply_outcomes(vec![Outcome::Deleted {
            path: PathBuf::from("/files-test-root/small"),
            freed: 0,
            tree_needs_rescan: true,
            links_survived: false,
        }]);

        assert!(app.session.is_some());
        // The drifted target's parent is the scan root itself here, so this
        // is indistinguishable from the full-refresh case the next test
        // isolates for a target whose parent is a strict subtree.
        assert_eq!(app.scanned_path(), Some(Path::new("/files-test-root")));
        app.cancel_scan();
    }

    #[test]
    fn a_deleted_targets_undercount_forces_a_full_rescan_even_under_a_strict_subtree() {
        let mut app = app_with_tree();

        // freed (0) short of the 500 the tree believed `inner` held: with no
        // surviving link to explain it, something outside duzo changed the
        // disk since the scan, so the whole tree is rescanned.
        app.apply_outcomes(vec![Outcome::Deleted {
            path: PathBuf::from("/files-test-root/big/inner"),
            freed: 0,
            tree_needs_rescan: true,
            links_survived: false,
        }]);

        assert!(app.session.is_some());
        assert_eq!(
            app.scanned_path(),
            Some(Path::new("/files-test-root")),
            "a scoped rescan of the target's parent could miss a surviving hard link elsewhere"
        );
        app.cancel_scan();
    }

    #[test]
    fn growth_after_a_stopped_scan_says_the_count_is_partial() {
        let mut app = app_with_tree();
        app.rescan_root();

        let mut partial = DiskEntry::new(PathBuf::from("/files-test-root"), EntryType::Directory);
        partial.set_scan_state(ScanState::Cancelled);
        let handle = std::thread::spawn(move || Ok(partial));
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        hand_over(&mut app, handle);
        app.cancel_background_scan();
        app.poll_scan();
        app.toggle_metric();

        assert!(app.diff.is_none());
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("partial count")),
            "status was: {:?}",
            app.status
        );
    }

    #[test]
    fn growth_after_a_deletion_says_the_totals_changed_instead_of_no_earlier_scan() {
        let delete_small = |app: &mut App| {
            app.apply_outcomes(vec![Outcome::Deleted {
                path: PathBuf::from("/files-test-root/small"),
                freed: 300,
                tree_needs_rescan: false,
                links_survived: false,
            }]);
            app.toggle_metric();
            app.status.clone().unwrap_or_default()
        };

        // Without snapshots or a diff, a rescan has nothing to compare with.
        let mut app = app_with_tree();
        app.toggle_metric();
        assert_eq!(
            app.status.as_deref(),
            Some("no earlier scan to compare against")
        );
        assert_eq!(delete_small(&mut app), "no earlier scan to compare against");

        let mut app = app_with_tree();
        app.settings.record_snapshots = true;
        let status = delete_small(&mut app);
        assert!(
            status.contains("totals changed since the scan"),
            "status was: {status:?}"
        );

        // A scan of a different root starts clean. Snapshots stay off while it
        // begins, so the test does not open the real snapshot store.
        let elsewhere = PathBuf::from("/files-test-missing-root");
        app.settings.record_snapshots = false;
        app.start_scan(&elsewhere);
        app.settings.record_snapshots = true;
        app.tree = Some(DiskEntry::new(elsewhere, EntryType::Directory));
        app.toggle_metric();
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("now the baseline")),
            "status was: {:?}",
            app.status
        );
    }

    #[test]
    fn a_surviving_link_with_any_drift_rescans_the_whole_tree() {
        let mut app = app_with_tree();
        app.tree = Some(deep_tree());

        // `deeper` kept a link outside and still freed more than its 500: a
        // file that grew inside it can cover the bytes that link kept.
        app.apply_outcomes(vec![Outcome::Deleted {
            path: PathBuf::from("/files-test-root/a/deep/deeper"),
            freed: 600,
            tree_needs_rescan: true,
            links_survived: true,
        }]);

        assert_eq!(app.scanned_path(), Some(Path::new("/files-test-root")));
        app.cancel_scan();
    }

    #[test]
    fn a_surviving_link_with_matching_totals_needs_no_full_rescan() {
        let mut app = app_with_tree();
        app.tree = Some(deep_tree());

        // The scan credited the shared file to the link outside, so `deeper`
        // freeing exactly what the tree held leaves its totals right; only
        // `sibling`, which grew since the scan, needs its parent rescanned.
        app.apply_outcomes(vec![
            Outcome::Deleted {
                path: PathBuf::from("/files-test-root/a/deep/deeper"),
                freed: 500,
                tree_needs_rescan: false,
                links_survived: true,
            },
            Outcome::Deleted {
                path: PathBuf::from("/files-test-root/a/sibling"),
                freed: 150,
                tree_needs_rescan: true,
                links_survived: false,
            },
        ]);

        assert_eq!(app.scanned_path(), Some(Path::new("/files-test-root/a")));
        app.cancel_scan();
    }

    #[test]
    fn one_targets_shortfall_is_not_offset_by_anothers_excess() {
        let mut app = app_with_tree();
        app.tree = Some(deep_tree());

        // `deeper` freed nothing of the 500 the tree credited it with, and
        // `sibling` freed 500 more than its 100. The batch total matches,
        // but where `deeper`'s bytes went is unknown.
        app.apply_outcomes(vec![
            Outcome::Deleted {
                path: PathBuf::from("/files-test-root/a/deep/deeper"),
                freed: 0,
                tree_needs_rescan: true,
                links_survived: false,
            },
            Outcome::Deleted {
                path: PathBuf::from("/files-test-root/a/sibling"),
                freed: 600,
                tree_needs_rescan: true,
                links_survived: false,
            },
        ]);

        assert_eq!(app.scanned_path(), Some(Path::new("/files-test-root")));
        app.cancel_scan();
    }

    #[test]
    fn a_failed_deletions_drift_rescans_only_the_targets_parent() {
        let mut app = app_with_tree();

        app.apply_outcomes(vec![Outcome::Failed {
            path: PathBuf::from("/files-test-root/big/inner"),
            error: "stopped part-way through".into(),
            removed_bytes: 0,
            removed_entries: 0,
            tree_may_have_changed: true,
            links_survived: false,
        }]);

        assert!(app.session.is_some());
        assert_eq!(
            app.scanned_path(),
            Some(Path::new("/files-test-root/big")),
            "a whole-root refresh would block deletion under / until the full disk rescans"
        );
        app.cancel_scan();
    }

    #[test]
    fn a_failed_deletions_surviving_link_forces_a_full_rescan() {
        let mut app = app_with_tree();

        app.apply_outcomes(vec![Outcome::Failed {
            path: PathBuf::from("/files-test-root/big/inner"),
            error: "stopped part-way through".into(),
            removed_bytes: 100,
            removed_entries: 1,
            tree_may_have_changed: true,
            links_survived: true,
        }]);

        assert!(app.session.is_some());
        assert_eq!(
            app.scanned_path(),
            Some(Path::new("/files-test-root")),
            "a failed deletion that still unlinked a surviving link needs the whole tree, \
             not just its own parent"
        );
        app.cancel_scan();
    }

    #[test]
    fn two_failed_deletions_drift_rescans_their_common_ancestor() {
        let mut app = app_with_tree();

        app.apply_outcomes(vec![
            Outcome::Failed {
                path: PathBuf::from("/files-test-root/big/inner"),
                error: "stopped part-way through".into(),
                removed_bytes: 0,
                removed_entries: 0,
                tree_may_have_changed: true,
                links_survived: false,
            },
            Outcome::Failed {
                path: PathBuf::from("/files-test-root/small"),
                error: "stopped part-way through".into(),
                removed_bytes: 0,
                removed_entries: 0,
                tree_may_have_changed: true,
                links_survived: false,
            },
        ]);

        assert!(app.session.is_some());
        assert_eq!(app.scanned_path(), Some(Path::new("/files-test-root")));
        app.cancel_scan();
    }

    #[test]
    fn a_deleted_targets_overcount_still_gets_only_a_scoped_rescan() {
        let mut app = app_with_tree();

        // freed (600) at or above the 500 the tree believed `inner` held —
        // not a hard link surviving elsewhere, so the narrower scoped rescan
        // still applies.
        app.apply_outcomes(vec![Outcome::Deleted {
            path: PathBuf::from("/files-test-root/big/inner"),
            freed: 600,
            tree_needs_rescan: true,
            links_survived: false,
        }]);

        assert!(app.session.is_some());
        assert_eq!(app.scanned_path(), Some(Path::new("/files-test-root/big")));
        app.cancel_scan();
    }

    #[test]
    fn deleting_a_pruned_parent_clears_descendant_marks() {
        let mut app = app_with_tree();
        app.marks.insert(PathBuf::from("/files-test-root/big"));
        app.marks
            .insert(PathBuf::from("/files-test-root/big/inner"));
        app.request_delete();

        app.apply_outcomes(vec![Outcome::Deleted {
            path: PathBuf::from("/files-test-root/big"),
            freed: 700,
            tree_needs_rescan: false,
            links_survived: false,
        }]);

        assert!(
            app.marks.is_empty(),
            "deleted descendants cannot remain marked"
        );
    }

    #[test]
    fn deleting_current_directory_ascends_to_surviving_ancestor() {
        let mut app = app_with_tree();
        app.cwd = PathBuf::from("/files-test-root/big/inner");
        assert_eq!(app.current_entry().unwrap().name(), "inner");
        app.selection = 1;
        app.search = Some("log".into());

        app.apply_outcomes(vec![Outcome::Deleted {
            path: PathBuf::from("/files-test-root/big/inner"),
            freed: 200,
            tree_needs_rescan: false,
            links_survived: false,
        }]);

        assert_eq!(app.cwd, PathBuf::from("/files-test-root/big"));
        assert!(app.current_entry().is_some());
        assert_eq!(app.selection, 0);
        assert_eq!(app.search, None, "the search was for the deleted folder");
    }

    #[test]
    fn deletion_invalidates_growth_diff_and_resets_metric() {
        let mut app = app_with_tree();
        app.metric = Metric::Growth;
        app.diff = Some(duzo_core::diff::diff(
            app.tree.as_ref().unwrap(),
            app.tree.as_ref().unwrap(),
            SizeKind::Allocated,
            0,
            1,
            0,
        ));

        app.apply_outcomes(vec![Outcome::Deleted {
            path: PathBuf::from("/files-test-root/small"),
            freed: 300,
            tree_needs_rescan: false,
            links_survived: false,
        }]);

        assert!(app.diff.is_none());
        assert_eq!(app.metric, Metric::Size);
    }

    #[test]
    fn a_rescan_that_no_longer_finds_the_folder_on_screen_climbs_out_of_it() {
        let base = std::env::temp_dir().join(format!("duzo-vanished-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a")).unwrap();
        let base = base.canonicalize().unwrap();
        let path = |rel: &str| base.join(rel).to_string_lossy().into_owned();
        let mut app = App::new(settings(), false);
        app.root = base.clone();
        app.tree = Some(entry(
            &path(""),
            10,
            vec![entry(&path("a"), 10, vec![entry(&path("a/b"), 10, vec![])])],
        ));
        app.view = View::Overview;
        // `b` was listed by the last scan and has gone from disk since.
        app.cwd = base.join("a/b");

        app.scan_subtree(base.join("a"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.session.is_some() {
            assert!(Instant::now() < deadline, "the rescan never landed");
            app.poll_scan();
            std::thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(app.cwd, base.join("a"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_skipped_mount_does_not_claim_to_be_empty() {
        let mut app = app_with_tree();
        app.tree.as_mut().unwrap().children[1].set_scan_state(ScanState::Skipped);
        app.cwd = PathBuf::from("/files-test-root/small");

        assert_eq!(app.cwd_listing(), Listing::Skipped);
    }

    #[test]
    fn subtree_rescan_does_not_leave_the_old_growth_diff_visible() {
        let mut app = app_with_tree();
        app.cwd = PathBuf::from("/files-test-root/big");
        app.metric = Metric::Growth;
        app.diff = Some(duzo_core::diff::diff(
            app.tree.as_ref().unwrap(),
            app.tree.as_ref().unwrap(),
            SizeKind::Allocated,
            0,
            1,
            0,
        ));

        let mut replacement =
            DiskEntry::new(PathBuf::from("/files-test-root/big"), EntryType::Directory);
        replacement.allocated_size = 900;
        replacement.apparent_size = 900;
        let handle = std::thread::spawn(move || Ok(replacement));
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        app.session = Some(Session {
            scanned_path: PathBuf::from("/files-test-root/big"),
            kind: SessionKind::Subtree,
            progress: Arc::new(Progress::default()),
            cancel: Cancel::new(),
            handle: Some(landing(handle, Compare::Nothing)),
            started: Instant::now(),
            discard_on_finish: false,
        });

        app.poll_scan();

        assert!(
            !app.showing_growth(),
            "the pre-rescan diff is stale after grafting"
        );
    }

    #[test]
    fn cancelling_a_background_scan_leaves_the_screen_unchanged_and_deletion_allowed() {
        let mut app = app_with_tree();
        app.cwd = PathBuf::from("/files-test-root/big");

        // A rescan that would have replaced `big` with different totals, were
        // it allowed to land.
        let mut replacement =
            DiskEntry::new(PathBuf::from("/files-test-root/big"), EntryType::Directory);
        replacement.allocated_size = 900;
        replacement.apparent_size = 900;
        let handle = std::thread::spawn(move || Ok(replacement));
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        app.session = Some(Session {
            scanned_path: PathBuf::from("/files-test-root/big"),
            kind: SessionKind::Subtree,
            progress: Arc::new(Progress::default()),
            cancel: Cancel::new(),
            handle: Some(landing(handle, Compare::Nothing)),
            started: Instant::now(),
            discard_on_finish: false,
        });

        app.cancel_background_scan();
        app.poll_scan();

        assert!(app.session.is_none());
        assert_eq!(
            app.tree
                .as_ref()
                .unwrap()
                .resolve(&app.cwd)
                .unwrap()
                .allocated_size,
            700,
            "the pre-scan tree must survive a cancel untouched"
        );
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("unchanged")),
            "status was: {:?}",
            app.status
        );

        // Deletion is refused only while `session` is set; cancelling clears
        // it, so a delete request right after must not be turned away for
        // "wait for the scan to finish".
        app.request_delete();
        assert!(
            app.status
                .as_deref()
                .is_none_or(|s| !s.contains("wait for the scan")),
            "status was: {:?}",
            app.status
        );
    }

    #[test]
    fn cancelling_a_root_rescan_absorbs_the_partial_tree_instead_of_the_stub() {
        let mut app = app_with_tree();
        app.watch = Some(Watch::new(60));
        // Backdated well past due, so a bug that leaves `last_scan` untouched
        // would still show `is_due()` true after cancelling — only the fix
        // moving the clock makes the assertion below discriminate.
        app.watch.as_mut().unwrap().last_scan = Instant::now() - Duration::from_secs(120);

        app.rescan_root();
        assert!(app.session.is_some());

        // What the background scan actually counted before `z` stopped it —
        // smaller than the original tree, standing in for a partial result.
        let mut partial = DiskEntry::new(PathBuf::from("/files-test-root"), EntryType::Directory);
        partial.allocated_size = 400;
        partial.apparent_size = 400;
        partial.children = vec![{
            let mut small =
                DiskEntry::new(PathBuf::from("/files-test-root/small"), EntryType::File);
            small.allocated_size = 400;
            small.apparent_size = 400;
            small
        }];
        partial.set_scan_state(ScanState::Cancelled);
        let handle = std::thread::spawn(move || Ok(partial));
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        hand_over(&mut app, handle);
        // `z` arrives while the scan is still counting. The scan
        // `rescan_root` started (of a path that does not exist) is long done,
        // so its progress would say the count is over.
        app.session.as_mut().unwrap().progress = Arc::new(Progress::default());

        app.cancel_background_scan();
        app.poll_scan();

        assert!(app.session.is_none());
        let tree = app.tree.as_ref().unwrap();
        assert_eq!(
            tree.allocated_size, 400,
            "the scan's own partial result must land, not the pre-scan stub"
        );
        assert_eq!(
            tree.children.len(),
            1,
            "the stub's empty child list must not be what gets kept"
        );
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("counted so far")),
            "status was: {:?}",
            app.status
        );

        // Watch mode's clock must move the same way a finished scan moves it,
        // or the very next tick fires another rescan immediately.
        assert!(
            !app.watch.as_ref().unwrap().is_due(),
            "cancelling a root rescan must not leave watch mode ready to fire again instantly"
        );
        // A partial count is not a watch round and must not feed a diff.
        assert_eq!(app.watch.as_ref().unwrap().rounds, 0);
        assert!(app.watch.as_ref().unwrap().baseline.is_none());
        assert!(app.diff.is_none());
        app.toggle_metric();
        assert!(
            app.status.as_deref().is_some_and(|s| s.contains("stopped")),
            "status was: {:?}",
            app.status
        );

        // Deletion is refused only while `session` is set or a snapshot is
        // on screen.
        app.request_delete();
        assert!(
            app.status
                .as_deref()
                .is_none_or(|s| !s.contains("wait for the scan")),
            "status was: {:?}",
            app.status
        );
    }

    /// `z` on the scan running under the snapshot on screen, two hours old,
    /// with `landed` as what that scan had counted when it stopped.
    fn stop_over_a_snapshot(app: &mut App, landed: DiskEntry) {
        // Snapshots are painted only for a new place.
        app.mode = ScanMode::Fresh;
        app.provisional = Some(duzo_core::history::now().saturating_sub(7200));
        let handle = std::thread::spawn(move || Ok(landed));
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        app.session = Some(Session {
            scanned_path: app.root.clone(),
            kind: SessionKind::Root,
            progress: Arc::new(Progress::default()),
            cancel: Cancel::new(),
            handle: Some(landing(handle, Compare::Nothing)),
            started: Instant::now(),
            discard_on_finish: false,
        });

        app.cancel_background_scan();
        assert!(app.session.as_ref().unwrap().cancel.is_cancelled());
        app.poll_scan();
    }

    fn stopped_count(size: u64) -> DiskEntry {
        let mut partial = DiskEntry::new(PathBuf::from("/files-test-root"), EntryType::Directory);
        partial.allocated_size = size;
        partial.apparent_size = size;
        partial.set_scan_state(ScanState::Cancelled);
        partial
    }

    #[test]
    fn z_over_a_snapshot_stops_the_scan_and_keeps_the_snapshot() {
        let mut app = app_with_tree();
        stop_over_a_snapshot(&mut app, stopped_count(400));

        assert!(app.session.is_none());
        assert_eq!(app.tree.as_ref().unwrap().allocated_size, 1000);
        assert!(app.provisional.is_some(), "still labelled as a snapshot");
        assert!(app.snapshot_kept);
        assert!(!app.cwd_is_being_scanned());
        assert!(
            app.status.as_deref().unwrap().contains("snapshot"),
            "{:?}",
            app.status
        );

        app.request_delete();
        assert!(
            app.confirm.is_none(),
            "a snapshot has no identities to delete by"
        );
    }

    #[test]
    fn a_stop_over_a_snapshot_counts_as_a_watch_round() {
        let mut app = app_with_tree();
        app.watch = Some(Watch::new(60));
        app.watch.as_mut().unwrap().last_scan = Instant::now() - Duration::from_secs(120);

        stop_over_a_snapshot(&mut app, stopped_count(400));

        assert!(app.snapshot_kept);
        assert!(
            !app.watch.as_ref().unwrap().is_due(),
            "watch would rescan at once"
        );
    }

    #[test]
    fn a_stop_too_late_to_cut_the_count_lands_the_whole_scan() {
        let mut app = app_with_tree();
        let mut whole = DiskEntry::new(PathBuf::from("/files-test-root"), EntryType::Directory);
        whole.allocated_size = 400;
        whole.apparent_size = 400;

        stop_over_a_snapshot(&mut app, whole);

        assert_eq!(app.tree.as_ref().unwrap().allocated_size, 400);
        assert_eq!(app.provisional, None);
        assert!(!app.snapshot_kept);
        assert_eq!(app.status, None);
    }

    #[test]
    fn growth_while_a_scan_corrects_the_snapshot_says_it_is_coming() {
        let mut app = app_with_tree();
        app.provisional = Some(duzo_core::history::now().saturating_sub(7200));
        app.session = Some(Session {
            scanned_path: app.root.clone(),
            kind: SessionKind::Root,
            progress: Arc::new(Progress::default()),
            cancel: Cancel::new(),
            handle: None,
            started: Instant::now(),
            discard_on_finish: false,
        });

        app.toggle_metric();

        assert!(
            app.status
                .as_deref()
                .unwrap()
                .contains("once this scan lands"),
            "{:?}",
            app.status
        );
    }

    #[test]
    fn a_rescan_stopped_too_late_drops_its_partial_count_promise() {
        let mut app = app_with_tree();
        app.rescan_root();
        let mut whole = DiskEntry::new(PathBuf::from("/files-test-root"), EntryType::Directory);
        whole.allocated_size = 400;
        hand_over(&mut app, {
            let handle = std::thread::spawn(move || Ok(whole));
            while !handle.is_finished() {
                std::thread::yield_now();
            }
            handle
        });
        app.session.as_mut().unwrap().progress = Arc::new(Progress::default());

        app.cancel_background_scan();
        app.poll_scan();

        assert_eq!(app.tree.as_ref().unwrap().allocated_size, 400);
        assert_eq!(app.status, None);
    }

    #[test]
    fn growth_after_a_kept_snapshot_in_watch_mode_waits_for_the_first_round() {
        let mut app = app_with_tree();
        app.watch = Some(Watch::new(60));
        stop_over_a_snapshot(&mut app, stopped_count(400));

        app.toggle_metric();

        assert_eq!(app.status.as_deref(), Some("waiting for the first rescan"));
    }

    #[test]
    fn growth_while_a_stopped_scan_winds_down_says_it_was_stopped() {
        let mut app = app_with_tree();
        app.provisional = Some(duzo_core::history::now().saturating_sub(7200));
        app.session = Some(Session {
            scanned_path: app.root.clone(),
            kind: SessionKind::Root,
            progress: Arc::new(Progress::default()),
            cancel: Cancel::new(),
            handle: None,
            started: Instant::now(),
            discard_on_finish: false,
        });
        app.cancel_background_scan();

        app.toggle_metric();

        assert!(
            app.status.as_deref().unwrap().contains("R rescans"),
            "{:?}",
            app.status
        );
    }

    #[test]
    fn a_whole_landing_after_a_stop_drops_the_stopped_growth_message() {
        let mut app = app_with_tree();
        app.provisional = Some(duzo_core::history::now().saturating_sub(7200));
        app.session = Some(Session {
            scanned_path: app.root.clone(),
            kind: SessionKind::Root,
            progress: Arc::new(Progress::default()),
            cancel: Cancel::new(),
            handle: None,
            started: Instant::now(),
            discard_on_finish: false,
        });
        app.cancel_background_scan();
        app.toggle_metric();
        assert_eq!(app.status.as_deref(), Some(STOPPED_OVER_SNAPSHOT));

        // The stop came too late: the whole count lands.
        let mut whole = DiskEntry::new(PathBuf::from("/files-test-root"), EntryType::Directory);
        whole.allocated_size = 400;
        hand_over(&mut app, {
            let handle = std::thread::spawn(move || Ok(whole));
            while !handle.is_finished() {
                std::thread::yield_now();
            }
            handle
        });
        app.poll_scan();

        assert_eq!(app.tree.as_ref().unwrap().allocated_size, 400);
        assert_eq!(app.status, None);
    }

    #[test]
    fn growth_after_a_kept_snapshot_points_at_a_rescan() {
        let mut app = app_with_tree();
        stop_over_a_snapshot(&mut app, stopped_count(400));

        app.toggle_metric();

        assert!(
            app.status.as_deref().unwrap().contains("R rescans"),
            "{:?}",
            app.status
        );
    }

    #[test]
    fn a_stopped_rescan_over_a_kept_snapshot_shows_what_it_counted() {
        let mut app = app_with_tree();
        stop_over_a_snapshot(&mut app, stopped_count(400));

        // `R` trims the snapshot it rescans, so its stop lands its own count.
        app.rescan_root();
        hand_over(&mut app, {
            let handle = std::thread::spawn(|| Ok(stopped_count(250)));
            while !handle.is_finished() {
                std::thread::yield_now();
            }
            handle
        });
        app.session.as_mut().unwrap().progress = Arc::new(Progress::default());
        app.cancel_background_scan();
        app.poll_scan();

        assert!(app.session.is_none());
        assert_eq!(app.tree.as_ref().unwrap().allocated_size, 250);
        assert_eq!(app.provisional, None);
    }

    #[test]
    fn folders_counted_after_a_kept_snapshot_lose_its_age() {
        let mut app = app_with_tree();
        // A focus scan filled `small` in before `z`.
        app.focus_grafted = vec![PathBuf::from("/files-test-root/small")];
        stop_over_a_snapshot(&mut app, stopped_count(400));

        // `r` on `big` counts it afresh.
        app.scan_subtree(PathBuf::from("/files-test-root/big"));
        assert_eq!(app.session.as_ref().unwrap().kind, SessionKind::Subtree);
        let replacement = entry("/files-test-root/big", 900, vec![]);
        let handle = std::thread::spawn(move || Ok(replacement));
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        hand_over(&mut app, handle);
        app.poll_scan();
        assert!(app.session.is_none());

        for (folder, fresh) in [("big", true), ("small", true), ("", false)] {
            app.cwd = Path::new("/files-test-root").join(folder);
            assert_eq!(app.provisional_for_cwd().is_none(), fresh, "{folder:?}");
        }
    }

    #[test]
    fn subtree_scan_depth_is_relative_to_the_original_root() {
        let mut app = app_with_tree();
        app.settings.scan_options.max_depth = Some(2);

        let options = app.subtree_scan_options(Path::new("/files-test-root/big"));

        assert_eq!(options.max_depth, Some(1));
    }

    #[test]
    fn a_skipped_mount_is_not_rescanned_as_its_own_root() {
        let mut app = app_with_tree();
        let tree = app.tree.as_mut().unwrap();
        let small = tree
            .children
            .iter_mut()
            .find(|c| c.name() == "small")
            .unwrap();
        small.set_scan_state(ScanState::Skipped);
        app.cwd = PathBuf::from("/files-test-root/small");

        app.rescan();

        assert!(
            app.session.is_none(),
            "the skipped mount must not be walked"
        );
        assert!(app.status.as_deref().is_some_and(|s| s.contains("skipped")));
    }

    #[test]
    fn a_running_scan_is_shown_only_for_folders_it_covers() {
        let mut app = app_with_tree();
        app.scan_subtree(PathBuf::from("/files-test-root/big"));

        app.cwd = PathBuf::from("/files-test-root/big/inner");
        assert!(app.cwd_is_being_scanned());
        app.cwd = PathBuf::from("/files-test-root/small");
        assert!(
            !app.cwd_is_being_scanned(),
            "a sibling is not being rescanned"
        );
        app.cancel_scan();
    }

    #[test]
    fn the_deletion_hint_names_the_outcome_that_changed_the_tree() {
        let outcomes = vec![
            Outcome::Refused {
                path: PathBuf::from("/r/refused"),
                reason: deletion::Refusal::OutsideScan,
            },
            Outcome::Failed {
                path: PathBuf::from("/r/interrupted"),
                error: "stopped".into(),
                removed_bytes: 10,
                removed_entries: 1,
                tree_may_have_changed: true,
                links_survived: false,
            },
        ];

        let status = deletion_status(0, 2, 10, &outcomes, Unit::Decimal);

        assert!(status.starts_with("deletion incomplete"), "{status}");
        assert!(status.contains("interrupted"), "{status}");
        assert!(status.contains("after removing 1 entry"), "{status}");

        // Others deleted first: the half-removed item is still not "skipped".
        for skipped in [1, 3] {
            let status = deletion_status(1, skipped, 10, &outcomes, Unit::Decimal);
            assert!(status.contains("deletion incomplete"), "{status}");
            assert!(!status.contains("skipped"), "{status}");
        }
    }

    #[test]
    fn watch_mode_keeps_showing_growth_after_a_deletion() {
        let mut app = app_with_tree().watching(Watch::new(5));
        app.diff = Some(duzo_core::diff::diff(
            app.tree.as_ref().unwrap(),
            app.tree.as_ref().unwrap(),
            SizeKind::Allocated,
            0,
            0,
            0,
        ));

        app.finish_tree_mutation();

        assert_eq!(app.metric, Metric::Growth);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_replaced_by_a_symlink_since_the_scan_refuses_the_rescan() {
        use std::fs;
        use std::os::unix::fs::symlink;

        // A real directory that stood in for the tree's `big` child at scan
        // time, and a second directory the tree knows nothing about.
        let base =
            std::env::temp_dir().join(format!("duzo-scan-subtree-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        // `/var` and `/private/var` are one place on macOS; without this the
        // path would never match its canonical form, swapped or not.
        let base = base.canonicalize().unwrap();
        let original = base.join("original");
        let elsewhere = base.join("elsewhere");
        fs::create_dir_all(&original).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        fs::remove_dir(&original).unwrap();
        symlink(&elsewhere, &original).unwrap();

        let mut app = app_with_tree();

        app.scan_subtree(original.clone());

        assert!(
            app.session.is_none(),
            "a path that no longer resolves to itself must not be rescanned"
        );
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("points somewhere else")),
            "status was: {:?}",
            app.status
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// A real folder tree, and an app showing it as a stub whose `deep` and
    /// `other` children are unlisted, with a root scan that stays running
    /// until the returned sender is dropped.
    fn browsing_ahead_of_a_root_scan(tag: &str) -> (App, PathBuf, std::sync::mpsc::Sender<()>) {
        use std::fs;
        let base = std::env::temp_dir().join(format!("duzo-focus-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("deep/inner/b")).unwrap();
        fs::create_dir_all(base.join("other")).unwrap();
        fs::write(base.join("deep/inner/a.bin"), vec![b'x'; 1000]).unwrap();
        fs::write(base.join("deep/inner/b/c.bin"), vec![b'x'; 500]).unwrap();
        fs::write(base.join("other/d.bin"), vec![b'x'; 10]).unwrap();
        let base = base.canonicalize().unwrap();

        let unlisted = |name: &str, items| {
            let mut child = DiskEntry::new(name, EntryType::Directory);
            child.items = items;
            child
        };
        let mut root = DiskEntry::new(&base, EntryType::Directory);
        root.children = vec![unlisted("deep", 5), unlisted("other", 2)];
        root.items = 8;

        let mut app = App::new(settings(), false);
        app.root = base.clone();
        app.cwd = base.clone();
        app.tree = Some(root);
        app.view = View::Overview;
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let scanned = base.clone();
        app.session = Some(Session {
            scanned_path: base.clone(),
            kind: SessionKind::Root,
            progress: Arc::new(Progress::default()),
            cancel: Cancel::new(),
            handle: Some(landing(
                std::thread::spawn(move || {
                    let _ = wait.recv();
                    Ok(DiskEntry::new(&scanned, EntryType::Directory))
                }),
                // Nothing to compare: the tests must not reach the
                // user's real snapshot store.
                Compare::Nothing,
            )),
            started: Instant::now(),
            discard_on_finish: false,
        });
        (app, base, release)
    }

    #[test]
    fn an_unlisted_folder_fills_in_before_the_root_scan_finishes() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("fill");
        app.cwd = base.join("deep");

        let deadline = Instant::now() + Duration::from_secs(10);
        while app.current_entry().unwrap().children.is_empty() {
            assert!(Instant::now() < deadline, "the focus scan never landed");
            app.poll_scan();
            std::thread::sleep(Duration::from_millis(5));
        }

        let deep = app.current_entry().unwrap();
        assert_eq!(deep.children[0].name(), "inner");
        assert_eq!(deep.apparent_size, 1500);
        // Two levels further down is listed too, without another wait.
        let inner_b = app
            .tree
            .as_ref()
            .unwrap()
            .resolve(&base.join("deep/inner/b"));
        assert_eq!(inner_b.unwrap().children[0].name(), "c.bin");
        assert!(app.session.is_some(), "the root scan keeps going");

        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A focus scan of `path` that stays running until the sender is dropped.
    fn blocked_focus(path: PathBuf) -> (Focus, std::sync::mpsc::Sender<()>) {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let landed = path.clone();
        let focus = Focus {
            path,
            cancel: Cancel::new(),
            handle: std::thread::spawn(move || {
                let _ = wait.recv();
                Ok(DiskEntry::new(&landed, EntryType::Directory))
            }),
        };
        (focus, release)
    }

    #[test]
    fn opening_another_unlisted_folder_moves_the_focus_there() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("retarget");
        let (focus, focus_release) = blocked_focus(base.join("deep"));
        let first = focus.cancel.clone();
        app.focus = Some(focus);

        app.cwd = base.join("other");
        app.poll_scan();

        assert_eq!(app.focus.as_ref().unwrap().path, base.join("other"));
        assert!(
            first.is_cancelled(),
            "the folder left behind stops scanning"
        );

        drop((release, focus_release));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_root_scan_that_lands_first_drops_the_focus_scan() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("lands");
        app.cwd = base.join("deep");
        let (focus, focus_release) = blocked_focus(base.join("deep"));
        let token = focus.cancel.clone();
        app.focus = Some(focus);

        drop(release);
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.session.is_some() {
            assert!(Instant::now() < deadline, "the root scan never landed");
            app.poll_scan();
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(app.focus.is_none());
        assert!(token.is_cancelled());
        drop(focus_release);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_a_stopped_scan_never_reached_is_focus_scanned() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("stopped");
        let tree = app.tree.as_mut().unwrap();
        tree.children[1].items = 1;
        tree.children[1].set_scan_state(ScanState::Cancelled);
        tree.items = 7;
        app.cwd = base.join("other");

        app.poll_scan();

        assert_eq!(app.focus.as_ref().unwrap().path, base.join("other"));
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn folders_whose_view_would_not_use_a_focus_scan_get_none() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("limits");
        app.cwd = base.join("deep");

        // At the --depth limit nothing below would be kept.
        app.settings.scan_options.max_depth = Some(1);
        app.poll_scan();
        assert!(app.focus.is_none());

        // The growth view lists the diff, not the tree a focus grafts into.
        app.settings.scan_options.max_depth = None;
        app.metric = Metric::Growth;
        app.diff = Some(duzo_core::diff::diff(
            app.tree.as_ref().unwrap(),
            app.tree.as_ref().unwrap(),
            SizeKind::Allocated,
            0,
            0,
            0,
        ));
        app.poll_scan();
        assert!(app.focus.is_none());

        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_landing_focus_scan_keeps_the_cursor_on_the_same_folder() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("cursor");
        // `other` outranks the unlisted `deep` (0 bytes) until `deep` is
        // counted; one byte, so any real allocation of `deep` beats it.
        app.tree.as_mut().unwrap().children[1].allocated_size = 1;
        let (_, cancel, handle) = scan::scan_in_background_prioritized(
            base.join("deep"),
            base.clone(),
            ScanOptions::default(),
            scan::MountBoundaries::default(),
        );
        app.focus = Some(Focus {
            path: base.join("deep"),
            cancel,
            handle,
        });
        app.selection = 1;
        assert_eq!(app.selected_row().unwrap().path, Some(base.join("deep")));

        land_focus(&mut app);

        assert_eq!(app.selection, 0, "deep now outranks other");
        assert_eq!(app.selected_row().unwrap().path, Some(base.join("deep")));
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Poll until the focus scan has landed.
    fn land_focus(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.focus.is_some() {
            assert!(Instant::now() < deadline, "the focus scan never landed");
            app.poll_scan();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// `deep` as a pruned snapshot listed it, holding `entries` as children,
    /// with a real scan of `deep` running as the focus.
    fn focus_landing_on(app: &mut App, base: &Path, entries: Vec<DiskEntry>) {
        let deep = &mut app.tree.as_mut().unwrap().children[0];
        deep.items = 1 + entries.iter().map(|entry| entry.items).sum::<u64>();
        deep.children = entries;
        let (_, cancel, handle) = scan::scan_in_background_prioritized(
            base.join("deep"),
            base.to_path_buf(),
            ScanOptions::default(),
            scan::MountBoundaries::default(),
        );
        app.focus = Some(Focus {
            path: base.join("deep"),
            cancel,
            handle,
        });
    }

    #[test]
    fn opening_a_subfolder_stops_the_parent_folder_scan() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("chain");
        // A snapshot-style `deep` that lists `inner` but not everything in it,
        // and an `inner` that lists nothing.
        let mut inner = DiskEntry::new("inner", EntryType::Directory);
        inner.items = 4;
        app.tree.as_mut().unwrap().children[0].children = vec![inner];
        let (focus, focus_release) = blocked_focus(base.join("deep"));
        let parent = focus.cancel.clone();
        app.focus = Some(focus);

        app.cwd = base.join("deep/inner");
        app.poll_scan();

        // Left running, the parent's walk would be served first by the pool.
        assert!(parent.is_cancelled());
        assert_eq!(app.focus.as_ref().unwrap().path, base.join("deep/inner"));

        drop((release, focus_release));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_the_fresh_count_no_longer_has_returns_to_its_parent() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("climb");
        // The snapshot listed `deep/gone/{x,y}`; the disk no longer has
        // `gone`. The fresh count of `deep` has `inner` and a small file.
        std::fs::write(base.join("deep/extra.bin"), b"x").unwrap();
        let mut gone = DiskEntry::new("gone", EntryType::Directory);
        gone.children = vec![
            DiskEntry::new("x", EntryType::Directory),
            DiskEntry::new("y", EntryType::Directory),
        ];
        gone.items = 3;
        focus_landing_on(&mut app, &base, vec![gone]);
        app.cwd = base.join("deep/gone");
        app.selection = 1;
        assert!(app.selected_row().unwrap().path.is_some());
        // A finished search is open there, and `top = 1` folds `deep`'s file
        // into "Other" — the row an old index of 1 would land on.
        app.search = Some(String::new());
        app.settings.top = 1;

        land_focus(&mut app);

        assert_eq!(app.cwd, base.join("deep"));
        assert_eq!(app.selection, 0, "not the old index, nor an Other row");
        assert!(app.search.is_none(), "a search for the folder left behind");
        assert_eq!(
            app.selected_row().unwrap().path,
            Some(base.join("deep/inner"))
        );
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_search_being_typed_survives_a_climb() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("typing");
        focus_landing_on(
            &mut app,
            &base,
            vec![{
                let mut gone = DiskEntry::new("gone", EntryType::Directory);
                gone.items = 1;
                gone
            }],
        );
        app.cwd = base.join("deep/gone");
        app.search = Some("i".into());
        app.search_active = true;

        land_focus(&mut app);

        assert_eq!(app.cwd, base.join("deep"));
        // The next keys are still meant for the search box, not as commands.
        assert!(app.search_active);
        assert_eq!(app.search.as_deref(), Some("i"));
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_stopped_scan_that_never_reached_the_folder_returns_to_one_it_did() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("stopped-root");
        drop(release);
        // On screen: `deep/inner`, filled ahead by a focus scan. The root scan
        // lands (as after `z`) the way a real stop leaves it: the root lists
        // `deep`, but `deep` was never walked.
        let mut deep = DiskEntry::new("deep", EntryType::Directory);
        deep.set_scan_state(ScanState::Cancelled);
        let mut stopped = DiskEntry::new(&base, EntryType::Directory);
        stopped.absorb(&deep);
        stopped.children = vec![deep];
        stopped.set_scan_state(ScanState::Cancelled);
        hand_over(&mut app, std::thread::spawn(move || Ok(stopped)));
        app.cwd = base.join("deep/inner");
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.session.is_some() {
            assert!(Instant::now() < deadline, "the root scan never landed");
            app.poll_scan();
            std::thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(app.cwd, base.join("deep"), "the nearest folder it listed");
        // That folder was never walked: say so rather than "empty".
        assert_eq!(app.cwd_listing(), Listing::Short);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_counted_but_not_listed_does_not_claim_to_be_empty() {
        let mut app = app_with_tree();
        // `small` holds two entries the tree counted but did not keep, as a
        // focus scan's 5-level cut leaves it once no scan is running.
        app.tree.as_mut().unwrap().children[1].items = 3;
        app.cwd = PathBuf::from("/files-test-root/small");

        assert_eq!(app.cwd_listing(), Listing::Short);
        app.cwd = PathBuf::from("/files-test-root/big/inner");
        assert_eq!(app.cwd_listing(), Listing::Complete, "really empty");
    }

    #[test]
    fn the_unlisted_remainder_is_what_the_rows_do_not_add_up_to() {
        let mut app = app_with_tree();
        // `big` (700) lists `inner` (500, 1 entry) but counted 3 more entries.
        app.tree.as_mut().unwrap().children[0].items = 5;
        app.cwd = PathBuf::from("/files-test-root/big");
        assert_eq!(app.cwd_unlisted(), Some((3, 200)));

        // The growth view lists the diff, not the tree: nothing to add there.
        app.metric = Metric::Growth;
        app.diff = Some(duzo_core::diff::diff(
            app.tree.as_ref().unwrap(),
            app.tree.as_ref().unwrap(),
            SizeKind::Allocated,
            0,
            0,
            0,
        ));
        assert_eq!(app.cwd_unlisted(), None);

        // A complete folder has nothing unlisted.
        app.metric = Metric::Size;
        app.cwd = PathBuf::from("/files-test-root");
        assert_eq!(app.cwd_unlisted(), None);
    }

    #[test]
    fn the_unlisted_line_keeps_its_action_in_a_narrow_legend() {
        let mut app = app_with_tree();
        // Millions of entries and terabytes unlisted: the widest numbers.
        let big = &mut app.tree.as_mut().unwrap().children[0];
        big.items = 9_999_999;
        big.allocated_size = 999_900_000_000_000;
        app.cwd = PathBuf::from("/files-test-root/big");

        let line = crate::tui::overview::unlisted_line(&app, 0, 34).unwrap();
        assert!(crate::tui::text::width(&line) <= 34, "{line}");
        assert!(line.ends_with("— press r"), "{line}");
        let wide = crate::tui::overview::unlisted_line(&app, 0, 100).unwrap();
        assert!(wide.contains("9,999,997 more"), "{wide}");
    }

    #[test]
    fn a_short_folder_outside_a_running_rescan_does_not_offer_r() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("r-busy");
        // `r` on `deep` is running; `other` is short and outside it.
        let session = app.session.as_mut().unwrap();
        session.kind = SessionKind::Subtree;
        session.scanned_path = base.join("deep");
        app.cwd = base.join("other");

        let message = crate::tui::overview::no_rows_message(&app, 0);

        assert_eq!(message, "not fully counted — r after scan");
        // The explorer legend is 34 columns at most.
        assert!(crate::tui::text::width(&message) <= 34);
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_counted_afresh_drops_the_snapshot_age() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("fresh-label");
        app.provisional = Some(1);
        app.cwd = base.join("deep");
        app.poll_scan();
        land_focus(&mut app);

        assert_eq!(app.provisional_for_cwd(), None, "`deep` was just counted");
        app.cwd = base.clone();
        assert_eq!(
            app.provisional_for_cwd(),
            Some(1),
            "the rest is the snapshot"
        );
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_swapped_for_a_symlink_is_not_grafted_in() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("swapped");
        std::os::unix::fs::symlink(base.join("other"), base.join("link")).unwrap();
        let mut link = DiskEntry::new("link", EntryType::Directory);
        link.items = 2;
        let tree = app.tree.as_mut().unwrap();
        tree.children.push(link);
        tree.items += 2;
        app.cwd = base.join("link");
        app.poll_scan();
        assert!(app.focus.is_some());
        land_focus(&mut app);

        let link = app
            .tree
            .as_ref()
            .unwrap()
            .resolve(&base.join("link"))
            .unwrap();
        assert!(link.children.is_empty(), "`other` grafted in under `link`");
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_replaced_by_a_file_returns_to_its_parent() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("became-file");
        app.cwd = base.join("deep");
        let path = base.join("deep");
        app.focus = Some(Focus {
            path: path.clone(),
            cancel: Cancel::new(),
            handle: std::thread::spawn(move || Ok(DiskEntry::new(&path, EntryType::File))),
        });

        land_focus(&mut app);

        assert_eq!(app.cwd, base);
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_growth_view_stays_on_a_removed_folder_after_a_graft() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("growth-climb");
        let gone = DiskEntry::new("gone", EntryType::Directory);
        app.tree.as_mut().unwrap().children[0].children = vec![gone];
        app.metric = Metric::Growth;
        app.diff = Some(duzo_core::diff::diff(
            app.tree.as_ref().unwrap(),
            app.tree.as_ref().unwrap(),
            SizeKind::Allocated,
            0,
            0,
            0,
        ));
        app.cwd = base.join("deep/gone");
        let (focus, focus_release) = blocked_focus(base.join("deep"));
        app.focus = Some(focus);

        drop(focus_release);
        land_focus(&mut app);

        // Growth lists from the diff, which still has `gone`.
        assert_eq!(app.cwd, base.join("deep/gone"));
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `r` rescans the folder on screen and `R` the whole tree, each in the
    /// background. While a scan runs both are refused rather than restarting
    /// it; `z` is what stops one.
    #[test]
    fn r_and_shift_r_start_background_scans_and_wait_for_a_running_one() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("r-keys");
        let running = app.session.as_ref().unwrap().cancel.clone();
        let settle = |app: &mut App| {
            let started = Instant::now();
            while app.session.is_some() {
                assert!(
                    started.elapsed() < Duration::from_secs(30),
                    "scan never landed"
                );
                app.poll_scan();
                std::thread::yield_now();
            }
        };

        app.rescan();
        assert_eq!(app.status.as_deref(), Some("a scan is already running"));
        app.status = None;
        app.rescan_root();
        assert_eq!(app.status.as_deref(), Some("a scan is already running"));
        assert!(!running.is_cancelled(), "the running scan was left alone");

        drop(release);
        settle(&mut app);

        app.cwd = base.join("deep");
        app.rescan();
        let session = app.session.as_ref().expect("r started a scan");
        assert_eq!(session.kind, SessionKind::Subtree);
        assert_eq!(session.scanned_path, base.join("deep"));
        settle(&mut app);

        // In watch mode a whole-tree scan compares against its own first
        // round, so this one never opens the user's snapshot store.
        app.watch = Some(Watch::new(60));
        app.rescan_root();
        let session = app.session.as_ref().expect("R started a scan");
        assert_eq!(session.kind, SessionKind::Root);
        assert_eq!(session.scanned_path, base);
        settle(&mut app);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_r_rescan_above_the_folder_also_scans_it_first() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("subtree");
        app.session.as_mut().unwrap().kind = SessionKind::Subtree;
        app.cwd = base.join("deep");

        app.poll_scan();

        assert_eq!(app.focus.as_ref().unwrap().path, base.join("deep"));
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn going_further_down_than_a_focus_keeps_stops_it() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("too-deep");
        let (focus, focus_release) = blocked_focus(base.join("deep"));
        let token = focus.cancel.clone();
        app.focus = Some(focus);

        // Within reach: the focus result will list this folder.
        app.cwd = base.join("deep/1/2/3/4");
        app.poll_scan();
        assert!(!token.is_cancelled());

        // One level past what it keeps: its result would empty this folder.
        app.cwd = base.join("deep/1/2/3/4/5");
        app.poll_scan();
        assert!(token.is_cancelled());

        drop((release, focus_release));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_focus_that_finished_as_the_user_went_past_it_is_not_grafted() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("past-reach");
        let (focus, focus_release) = blocked_focus(base.join("deep"));
        drop(focus_release);
        while !focus.handle.is_finished() {
            std::thread::yield_now();
        }
        app.focus = Some(focus);
        // In one burst of keys the user went past what it keeps.
        app.cwd = base.join("deep/1/2/3/4/5");

        app.poll_scan();

        assert!(app.focus.is_none());
        let deep = app
            .tree
            .as_ref()
            .unwrap()
            .resolve(&base.join("deep"))
            .unwrap();
        assert_eq!(deep.items, 5, "the stale result did not replace `deep`");
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_stopped_rescan_says_which_folders_it_did_change() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("stopped-r");
        app.session.as_mut().unwrap().kind = SessionKind::Subtree;
        app.cwd = base.join("deep");
        app.poll_scan();
        land_focus(&mut app);
        // A comparison was showing before the rescan.
        app.diff = Some(duzo_core::diff::diff(
            app.tree.as_ref().unwrap(),
            app.tree.as_ref().unwrap(),
            SizeKind::Allocated,
            0,
            0,
            0,
        ));

        app.cancel_background_scan();
        drop(release);
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.session.is_some() {
            assert!(Instant::now() < deadline, "the rescan never ended");
            app.poll_scan();
            std::thread::sleep(Duration::from_millis(5));
        }

        let status = app.status.clone().unwrap_or_default();
        assert!(
            status.contains("except folders opened meanwhile"),
            "{status}"
        );
        // The tree is no longer the last scan's: no stale comparison over it.
        assert!(app.diff.is_none());
        assert!(app.changed_since_scan);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn no_rows_messages_fit_their_cases() {
        let mut app = app_with_tree();
        // The run's own root at `--depth 0`: not "run duzo here" again.
        app.settings.scan_options.max_depth = Some(0);
        // Counted three entries, kept none.
        let root = app.tree.as_mut().unwrap();
        root.children.clear();
        root.items = 4;
        assert_eq!(
            crate::tui::overview::no_rows_message(&app, 0),
            "beyond --depth"
        );

        // A search just opened, nothing typed: about the folder, not the query.
        app.settings.scan_options.max_depth = None;
        app.search = Some(String::new());
        assert_eq!(
            crate::tui::overview::no_rows_message(&app, 0),
            "not fully counted — press r"
        );

        // A typed search in a folder not fully listed: a match may still come.
        app.search = Some("zz".into());
        assert_eq!(
            crate::tui::overview::no_rows_message(&app, 0),
            "nothing listed matches yet"
        );
    }

    #[test]
    fn a_failed_rescan_keeps_its_focus_grafts_as_a_change() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("failed-r");
        app.session.as_mut().unwrap().kind = SessionKind::Subtree;
        app.cwd = base.join("deep");
        app.poll_scan();
        land_focus(&mut app);
        app.diff = Some(duzo_core::diff::diff(
            app.tree.as_ref().unwrap(),
            app.tree.as_ref().unwrap(),
            SizeKind::Allocated,
            0,
            0,
            0,
        ));
        // The `r` rescan itself fails (the folder went away, say).
        drop(release);
        hand_over(
            &mut app,
            std::thread::spawn(|| Err(anyhow::anyhow!("gone"))),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.session.is_some() {
            assert!(Instant::now() < deadline, "the rescan never ended");
            app.poll_scan();
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(app.diff.is_none(), "no stale comparison over fresh folders");
        assert!(app.changed_since_scan);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_pushed_into_other_keeps_the_cursor_on_other() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("other-row");
        app.settings.top = 1;
        // `deep` leads on stale numbers, then counts in far smaller than `other`.
        let tree = app.tree.as_mut().unwrap();
        tree.children[1].allocated_size = 100_000_000;
        tree.children[0].allocated_size = 900_000_000;
        let (_, cancel, handle) = scan::scan_in_background_prioritized(
            base.join("deep"),
            base.clone(),
            ScanOptions::default(),
            scan::MountBoundaries::default(),
        );
        app.focus = Some(Focus {
            path: base.join("deep"),
            cancel,
            handle,
        });
        app.selection = 0;
        assert_eq!(app.selected_row().unwrap().path, Some(base.join("deep")));

        land_focus(&mut app);

        let selected = app.selected_row().unwrap();
        assert_eq!(selected.path, None, "the cursor is on {:?}", selected.name);
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_entry_gone_from_disk_does_not_send_the_cursor_to_other() {
        let (mut app, base, release) = browsing_ahead_of_a_root_scan("gone-entry");
        // The snapshot listed `deep/stale` first; the disk no longer has it.
        std::fs::write(base.join("deep/extra.bin"), b"x").unwrap();
        let mut stale = DiskEntry::new("stale", EntryType::Directory);
        stale.allocated_size = 900_000_000;
        focus_landing_on(&mut app, &base, vec![stale]);
        app.cwd = base.join("deep");
        app.selection = 0;
        assert_eq!(
            app.selected_row().unwrap().path,
            Some(base.join("deep/stale"))
        );
        app.settings.top = 1;

        land_focus(&mut app);

        // Rows are now `inner` and "Other" (holding `extra.bin`, not `stale`).
        assert_eq!(
            app.selected_row().unwrap().path,
            Some(base.join("deep/inner"))
        );
        drop(release);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_listing_is_short_when_it_counts_less_than_it_holds() {
        let dir = |items, children: Vec<DiskEntry>| {
            let mut entry = DiskEntry::new("d", EntryType::Directory);
            entry.items = items;
            entry.children = children;
            entry
        };
        let file = || DiskEntry::new("f", EntryType::File);

        assert!(!lists_less_than_it_holds(&dir(3, vec![file(), file()])));
        assert!(lists_less_than_it_holds(&dir(3, vec![])), "stub");
        assert!(
            lists_less_than_it_holds(&dir(4, vec![file()])),
            "pruned snapshot"
        );
        assert!(
            !lists_less_than_it_holds(&dir(1, vec![])),
            "empty or denied"
        );
        let mut stopped = dir(1, vec![]);
        stopped.set_scan_state(ScanState::Cancelled);
        assert!(lists_less_than_it_holds(&stopped), "never reached");
        let mut cut_short = dir(2, vec![file()]);
        cut_short.set_scan_state(ScanState::Cancelled);
        assert!(
            lists_less_than_it_holds(&cut_short),
            "stopped while listing"
        );
        let mut big_file = file();
        big_file.items = 5;
        assert!(!lists_less_than_it_holds(&big_file));
    }

    #[cfg(unix)]
    #[test]
    fn a_refresh_whose_root_now_points_elsewhere_is_refused() {
        use std::fs;
        use std::os::unix::fs::symlink;

        // The scan root itself was replaced by a symlink since the last
        // scan, the same way `a_folder_replaced_by_a_symlink_since_the_scan_refuses_the_rescan`
        // covers a folder under it — `refresh` (`R`, or the watch tick) must
        // refuse the same way instead of grafting a stub with no session to
        // ever fill it back in.
        let base =
            std::env::temp_dir().join(format!("duzo-refresh-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let original = base.join("original");
        let elsewhere = base.join("elsewhere");
        fs::create_dir_all(&original).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        fs::remove_dir(&original).unwrap();
        symlink(&elsewhere, &original).unwrap();

        let mut app = app_with_tree();
        app.root = original.clone();
        app.cwd = original.clone();

        app.refresh();

        assert!(
            app.session.is_none(),
            "a root that no longer resolves to itself must not start a scan"
        );
        assert!(
            app.tree.is_some(),
            "the tree on screen must survive the refusal untouched"
        );
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("points somewhere else")),
            "status was: {:?}",
            app.status
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn tick_watch_does_not_refresh_outside_a_browsing_view() {
        let mut app = app_with_tree();
        app.watch = Some(Watch::new(60));
        app.watch.as_mut().unwrap().last_scan = Instant::now() - Duration::from_secs(120);
        app.view = View::Picker;

        app.tick_watch();

        assert!(
            app.session.is_none(),
            "the Picker has no tree on screen for a refresh to update"
        );
    }

    #[test]
    fn cancelling_a_deletion_gives_the_watch_a_full_interval() {
        let mut app = app_with_tree();
        app.watch = Some(Watch::new(60));
        // Just short of due: the reset must not wait for a due frame.
        app.watch.as_mut().unwrap().last_scan = Instant::now() - Duration::from_secs(59);
        app.request_delete();
        app.tick_watch();

        app.cancel_delete();
        app.tick_watch();

        assert!(app.session.is_none());
        assert!(app.watch.as_ref().unwrap().last_scan.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn watch_mode_does_not_rescan_while_a_deletion_runs() {
        let mut app = app_with_tree();
        app.watch = Some(Watch::new(60));
        app.watch.as_mut().unwrap().last_scan = Instant::now() - Duration::from_secs(59);
        let (progress, cancel, handle) =
            deletion::delete_in_background(Vec::new(), app.root.clone(), None);
        app.deleting = Some(Deleting {
            progress,
            cancel,
            handle: Some(handle),
            stopping: false,
        });

        app.tick_watch();
        app.refresh();
        assert!(app.session.is_none());

        // Once it is done, the watch waits a full interval again.
        app.deleting = None;
        app.tick_watch();
        assert!(app.session.is_none());
        assert!(app.watch.as_ref().unwrap().last_scan.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_failed_refresh_makes_the_watch_wait_a_full_interval_before_retrying() {
        let mut app = app_with_tree();
        app.watch = Some(Watch::new(60));
        // Backdated past due, so only the failure arm moving the clock makes
        // the final assertion hold.
        app.watch.as_mut().unwrap().last_scan = Instant::now() - Duration::from_secs(120);
        app.rescan_root();
        assert!(app.session.is_some());

        let handle: JoinHandle<Result<DiskEntry>> =
            std::thread::spawn(|| Err(anyhow::anyhow!("mount vanished")));
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        hand_over(&mut app, handle);

        app.poll_scan();

        assert!(app.session.is_none());
        assert!(
            !app.watch.as_ref().unwrap().is_due(),
            "a failed scan must move the clock the same way a finished one does, \
             or tick_watch retries on the very next frame"
        );
    }

    #[test]
    fn beginning_a_fresh_scan_cancels_the_session_it_replaces() {
        let mut app = app_with_tree();
        let old_root = app.root.clone();
        app.from_picker = true;

        let handle: JoinHandle<Result<DiskEntry>> = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            Ok(DiskEntry::new(
                PathBuf::from("/files-test-root"),
                EntryType::Directory,
            ))
        });
        let old_cancel = Cancel::new();
        app.session = Some(Session {
            scanned_path: old_root,
            kind: SessionKind::Root,
            progress: Arc::new(Progress::default()),
            cancel: old_cancel.clone(),
            handle: Some(landing(handle, app.compare_for(SessionKind::Root))),
            started: Instant::now(),
            discard_on_finish: false,
        });

        app.start_scan(Path::new("/files-other-root"));

        assert!(
            old_cancel.is_cancelled(),
            "the replaced session's cancel token must be set, not just dropped"
        );
        assert_eq!(app.root, PathBuf::from("/files-other-root"));
        app.cancel_scan();
    }

    #[test]
    fn growth_in_watch_mode_waits_for_the_next_rescan_once_a_baseline_exists() {
        let mut app = app_with_tree();
        app.watch = Some(Watch::new(60));
        app.toggle_metric();
        assert_eq!(app.status.as_deref(), Some("waiting for the first rescan"));

        app.watch.as_mut().unwrap().baseline = Some(Arc::new(
            duzo_core::history::prune_for_storage(app.tree.as_ref().unwrap()),
        ));
        app.toggle_metric();
        assert_eq!(app.status.as_deref(), Some("waiting for the next rescan"));
    }

    #[test]
    fn a_fresh_scan_to_a_new_root_resets_the_watch_baseline() {
        let mut app = app_with_tree();
        app.watch = Some(Watch::new(60));
        // The baseline a completed scan of the old root would have left
        // behind for watch mode to diff future scans against.
        app.watch.as_mut().unwrap().baseline = Some(Arc::new(
            duzo_core::history::prune_for_storage(app.tree.as_ref().unwrap()),
        ));
        app.watch.as_mut().unwrap().rounds = 3;
        let baseline = app.watch.as_ref().unwrap().baseline.clone().unwrap();
        let now = duzo_core::history::now();
        app.diff = Some(duzo_core::diff::diff(
            &baseline,
            app.tree.as_ref().unwrap(),
            app.settings.size_kind,
            now,
            now,
            duzo_core::history::storage_floor(&baseline),
        ));

        // Reachable without the Picker: a watched root's session can fail
        // (`poll_scan`'s `Ok(Err(_))` arm) and drop the view to `Picker`
        // without touching `watch`; picking a different filesystem from
        // there reaches `begin(Fresh)` with a new root while `watch` is
        // still set from the old one.
        app.start_scan(Path::new("/files-other-root"));

        assert!(
            app.watch.as_ref().unwrap().baseline.is_none(),
            "a new root must not be diffed against the old root's baseline"
        );
        assert!(
            app.diff.is_none(),
            "the old root's growth view must not survive a change of root"
        );
        assert_eq!(
            app.watch.as_ref().unwrap().every,
            60,
            "the interval the user chose must survive a change of root"
        );
        app.cancel_scan();
    }
}
