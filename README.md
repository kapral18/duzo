<div align="center">
  <h1>duzo</h1>

  <p><strong>Explore disk usage from your terminal</strong></p>

  <p>
    <img alt="License" src="https://img.shields.io/badge/license-MIT-green">
    <img alt="Rust" src="https://img.shields.io/badge/rust-1.88%2B-orange">
    <img alt="Edition" src="https://img.shields.io/badge/edition-2024-blue">
  </p>

  <p>
    <a href="#install">Install</a>
    &nbsp;·&nbsp;
    <a href="#what-changed">What changed</a>
    &nbsp;·&nbsp;
    <a href="#what-can-i-delete">What can I delete</a>
    &nbsp;·&nbsp;
    <a href="#exploring">Exploring</a>
    &nbsp;·&nbsp;
    <a href="#library">Library</a>
  </p>

  <p>
    <img
      src="https://raw.githubusercontent.com/kapral18/duzo/main/assets/screenshot.png"
      alt="duzo's radial explorer showing a 178 GB sample directory, with a legend naming every wedge"
      width="900">
  </p>

  <p><em>The radial explorer, showing apparent sizes in a sample directory.</em></p>
</div>

---

Duzo is a terminal disk-usage explorer for macOS and Linux. Browse folder sizes
as a radial chart or ranked list, inspect what occupies space, and delete
selected entries after confirmation. Background rescans keep navigation
available while new results are counted.

Duzo is an independent project, originally based on
[Disko](https://github.com/cesarferreira/disko).

## Install

Build from source with [Rust](https://rustup.rs) **1.88+** and `~/.cargo/bin`
on your `PATH`:

```bash
git clone https://github.com/kapral18/duzo
cd duzo
cargo install --path . --locked
```

```bash
duzo --help
duzo ~
```

Duzo uses its own configuration and snapshot directories; it does not read or
migrate Disko's saved data.

## What changed

A completed full scan records a snapshot for later comparisons. Up to 64 are
kept per scanned directory. Cancelled scans do not become comparison baselines;
history cannot remove the wait for a complete initial scan.

Snapshots live under `duzo/snapshots` in the platform data directory:
`$XDG_DATA_HOME` or `~/.local/share` on Linux, `~/Library/Application Support`
on macOS, and `%APPDATA%` on Windows.

```bash
duzo diff                # since the previous scan
duzo diff --since 7d     # since a week ago
duzo diff ~/ --since 3mo # a specific directory, a specific window
duzo history             # every snapshot, and what moved between them
```

Inside the TUI, press **`t`** and the same data becomes the view: growth glows,
shrinking directories cool to blue, everything that did not move recedes into
the background.

```text
 Macintosh HD                                          404 GB used of 494 GB
 ████████████████████████████████████████████░░░░░░░░  82%

 Current folder: /    404 GB    +82 GB in 7 days

   +46 GB  Users         ████████████████████████████████     257 GB
   +34 GB  Applications  ███████████████████████▋              34 GB
    +4 GB  private       ██▊                                    4 GB
```

Select anything and press `d` to see when it grew and what produced it.

### Watching it happen

```bash
duzo watch                    # the current directory, every 3s
duzo watch ~/ --interval 30s
```

Growth since the moment you started watching, updated live — for when a build,
a download or an agent is filling the disk right now and you want to see which
directory is doing it.

### Saved results while rescanning

duzo paints the last snapshot the moment you open it, labelled for what it is,
and updates it when the fresh scan finishes (shown here in the list view):

```text
 Current folder: ~/code    78 GB    as of 2 hours ago · ⠋ rescanning…

   41 GB  monorepo   ████████████████▉   52.7%
   18 GB  .cache     ███████▋            23.6%
   18 GB  models     ███████▌            23.6%
```

On a first scan there is nothing to paint from, so the progress screen shows
what has actually been counted so far instead of a bare spinner:

```text
  ⠙  113689 items · 62 GB · 2s
     ~/code/.cache/bazel/63bf4…/site-packages/urllib3

  Biggest so far
     40 GB   ~/code/monorepo/.git
     56 MB   ~/code/monorepo/services
```

Those are finished directories, not running estimates — each number is final
for that folder. The list is incomplete, never wrong.

Saved snapshots provide previous results while a fresh scan runs. They do not
make traversal incremental: every full scan walks the selected tree again.

## What can I delete

```console
$ duzo clean

 Reclaimable developer storage                 73 GB

  28 GB  Xcode DerivedData        safe to regenerate
         regenerate: rebuild in Xcode · last used 2 days ago
  19 GB  Gradle caches            safe to regenerate
         regenerate: ./gradlew build · last used 6 hours ago · 3 locations
  14 GB  Docker data              review first
         regenerate: docker system prune · last used 3 weeks ago
   7 GB  Android emulator images  review first
         regenerate: recreate the AVD · unused for 4 months
   5 GB  Rust target directories  safe to regenerate
         regenerate: cargo build · last used 1 hour ago · 12 locations
```

Not "this folder is large" but **what produced it**, **whether removing it is
safe**, **what would bring it back**, and **when anything last touched it** —
that last one from real modification times gathered during the scan, not a
guess.

```bash
duzo clean --safe-only          # only things that regenerate themselves
duzo clean --idle-for 3mo       # only what nobody has touched in a season
duzo clean --delete             # after typing "delete" to confirm
```

Deletion is opt-in, prompts for a typed confirmation, and re-checks every path
against the category rules immediately before removing it. A `target` directory
only counts as build output when a `Cargo.toml` sits beside it.

## Exploring

```bash
duzo              # pick a disk, then explore it
duzo ~            # scan your home directory right away
duzo / --top 20   # limit the list view to the 20 largest entries
```

Disks open in the radial chart by default. Use `l` / `→` to open a folder
and `h` / `←` to go up. Press `Tab` to switch between the chart and this list:

```text
 Macintosh HD                                      404 GB used of 494 GB
 ████████████████████████████████████████░░░░░░░░  82%

 Current folder: /    404 GB

   257 GB  Users         ████████████████████▍             63.6%
    84 GB  Library       ██████▋                           20.8%
    34 GB  Applications  ██▊                                8.4%

 Largest items
   71 GB   ~/Library
   48 GB   ~/Downloads
   32 GB   ~/code
```

The view you pick with `Tab` stays in place across scans and disk changes.
It is saved when you quit, to `duzo/view` in your config directory
(`$XDG_CONFIG_HOME` or `~/.config` on Linux, `~/Library/Application Support` on
macOS, `%APPDATA%` on Windows), and restored next time.

| Key | Does |
| --- | --- |
| `j` `k` / `↓` `↑` | move the selection |
| `Tab` | switch between chart and list |
| `Enter` | open the radial explorer (and drill in, once there) |
| `l` `→` / `h` `←` `Backspace` | open the selected folder / go back up |
| `Esc` | close details or search, step from the chart to the list, then back to the disk list if you came from it |
| **`t`** | **switch between size and growth** |
| `/` | filter the list as you type |
| `s` | cycle sort: size, name, item count |
| `d` | details — device, type, inodes, change, category |
| `a` | apparent sizes instead of blocks used |
| `o` | show it in Finder / your file manager |
| `y` `c` | copy the selected path to the clipboard |
| `Space` | mark an entry; the footer totals what you marked |
| **`x`** | **delete what is marked** (or the selection), after confirmation |
| `r` | rescan current directory |
| `R` | rescan from root |
| `z` | stop a background rescan |
| `q` | quit |

`r`, `R`, and the automatic rescan after a deletion (below) run in the
background, and both views stay interactive. `r` and `R` are refused while one
runs; press `z` to stop it. A stopped folder rescan leaves the folder as it
was, except folders you opened while it ran (below). A stopped whole-tree
rescan shows what it counted so far, marked incomplete, with no growth
comparison; if it never listed the folder you are in, you are moved up to the
nearest one it did. Stopping the scan that corrects the snapshot duzo opens
with leaves the snapshot on screen, labelled with its age; deleting waits for a
full rescan (`R`), since a snapshot cannot tell whether a file is still the one
it recorded. A `z` that comes after a whole-tree rescan has finished
counting stops nothing: duzo says it is finishing, and the full result lands,
with its comparison if there is one. Quitting right then waits until that
scan's snapshot is saved (`q` again skips it). A folder duzo did not fully
list says so ("not fully counted — press r", or "beyond --depth" at the depth
limit) instead of looking empty.

While a scan runs you can keep going deeper than the numbers on screen
reach: opening a folder under what it covers that is listed only in part — or
not at all — scans that folder first, on threads of its own, and fills it in
as soon as it is counted. The scan carries on and replaces those numbers when
it finishes. Until a folder lists everything it holds, the line under its
entries says how much is missing and whether it is being counted
(`⠋ counting 1,200 more · 85 GB…`), so entries that appear later are no
surprise.

### Deleting

Mark things with `Space`, press **`x`**, and duzo shows exactly what will go:

```text
 ┌ This cannot be undone ───────────────────────────────────────────┐
 │Permanently delete 2 items, 16 GB                                 │
 │                                                                  │
 │  • ~/Library/Developer/Xcode/DerivedData/    12 GB                │
 │  • ~/.gradle/caches/                          4 GB                │
 │                                                                  │
 │ Type delete to confirm: del▏  Esc to cancel                      │
 └──────────────────────────────────────────────────────────────────┘
```

Nothing happens until the word is typed in full — `Enter` on its own is not
enough, and navigation keys cannot reach the list behind the prompt.

The removal itself runs on a background thread with live counters, because
clearing 68 GB of build caches means unlinking hundreds of thousands of files
and a screen that stops redrawing for a minute is indistinguishable from a
crash:

```text
 ┌ Deleting ────────────────────────────────────────────────────┐
 │⠙ Deleting 3 of 12                                            │
 │                                                              │
 │ ~/.stax/worktrees/stax/ready-ci-oneline                      │
 │                                                              │
 │ freed 19 GB  ·  482310 files removed                         │
 │ Esc to stop after the current item                           │
 └──────────────────────────────────────────────────────────────┘
```

`Esc` stops it after the file it is on (on Windows, between items), and reports
which items it never reached. Every path is re-checked against the filesystem
immediately before removal, so duzo will refuse the scan root, anything
outside the current scan, a mount point or a folder with another filesystem
mounted inside it, and something that has vanished since the scan (or, on Unix,
been replaced). Symlinks are unlinked, never followed into. `x` is refused
while a scan runs.

Totals usually correct themselves instantly, by subtracting what was removed.
When a deletion fails or stops after removing something, or frees more than the
tree expected (a file grew since the scan), duzo rescans only the parent
directory, in the background. When it frees less than expected, or unlinks a
file that another hard link still held at the time and then fails or frees a
different amount than expected, the missing bytes could be anywhere, so duzo
rescans the whole tree. Press `z` to stop that rescan.

`duzo --read-only` removes the ability entirely.

For caches specifically, `duzo clean --delete` is usually the better tool: it
knows what regenerates them.

### Opening it where you can act on it

`o` hands the selection to the desktop's file manager — Finder on macOS,
Explorer on Windows, `xdg-open` elsewhere.

duzo only ever opens **folders**, never files. Selecting a file shows it inside
its parent (`open -R` on macOS, `/select` on Windows, the containing folder on
Linux). The files duzo surfaces are by definition the enormous ones, and
handing a 40 GB video to whatever application claims the extension is not a
thing a disk tool should do by accident.

### Disk capacity and directory usage never share a line

The header always describes the **filesystem** (`Macintosh HD — 404 GB used of
494 GB`). The body always describes the **directory you are in** (`Current
folder: /Users/alice — 257 GB`). Conflating those two numbers is the single
most confusing thing a disk tool can do.

### Details are one key away, not in your face

Filesystem types, device identifiers, inode counts and pseudo-filesystems are
real information — they are just not the answer to "what is filling my disk".

```bash
duzo --details        # device, type, read-only, inodes
duzo --filesystems    # every mount, pseudo-filesystems included
duzo --inodes         # inode usage instead of bytes
```

On macOS, one disk is mounted as two volumes: a sealed, read-only system volume
at `/` and the writable Data volume at `/System/Volumes/Data`. duzo lists them
as one writable `/` row with inodes counted from both, in
`--filesystems --json` too. Reports about one path (`--filesystems <path>`,
`--inodes <path>`, `--details`, `d` in the TUI) name the volume the path lives
on: `/System` is on the system volume, `~` on the Data volume.

## Scripting

Piping switches to text automatically — no flag needed.

```bash
duzo ~ --plain            # bytes, human size, share, path (tab separated)
duzo ~ --json             # the whole tree, structured
duzo diff --plain         # signed bytes, human, kind, path
duzo clean --json         # categories, safety, regenerate command, last used
duzo watch --plain        # one line per interval, forever
```

Raw bytes come first so `cut -f1` and `sort -n` work.

### Network filesystems are skipped by default

NFS, SMB, sshfs and blobfuse mounts are not on your disk, and stat-ing them one
round trip at a time is brutal — a single directory listing on a blob-storage
mount can take five seconds. duzo stops at the mount point and says so rather
than silently reporting a low number:

```console
$ duzo /mnt --plain          # a blobfuse2 mount lives under here
16384   16 KB   57.1%   /mnt/lost+found
4096    4.1 KB  14.3%   /mnt/remote
                                        # 0.009s, vs ~10 minutes walking Azure
```

Pass `--remote` to walk them anyway. Naming a network mount as the scan root
still scans it — asking for it is asking for it.

Mounts and system folders that hold operating-system internals rather than your
files are skipped too, and `--remote` does not change that: on Linux, pseudo
filesystems such as `proc`, `sysfs`, `tmpfs` (`/tmp` included, where it is
tmpfs), `overlay`, `squashfs` and snap mounts; on macOS, `/dev`, `/Volumes` and
`/System/Volumes`. They show up in the tree as skipped, and naming one as the
scan root still scans it. A mount point is judged by what is mounted there now,
so a real filesystem mounted over an autofs placeholder (a systemd automount on
`/boot`, say) is walked.

Note that `--depth` limits what is *kept*, not what is *walked*: sizes stay
exact, so it does not make a slow mount fast.

### Options

| Flag | Does |
| --- | --- |
| `-t, --top <N>` | limit list and text output to N entries, grouping the rest as "Other" |
| `--depth <N>` | keep only N levels of tree; sizes stay exact either way |
| `--apparent` | count file lengths (`ls`) instead of blocks used (`du`) |
| `--binary` | GiB/MiB instead of GB/MB |
| `-x, --one-file-system` | do not cross into other mounts |
| `--count-hardlinks` | count a hard-linked file once per link |
| `-a, --all` | include pseudo-filesystems in the disk list |
| `--remote` | walk network filesystems too (skipped by default) |
| `--read-only` | disable deleting entirely |
| `--no-snapshot` | do not record this scan in the history |

Sizes match `du` byte for byte: `--apparent` agrees with `du -sb`, and the
default agrees with `du -s --block-size=1`.

## Library

The engine is split out so the TUI, `--json` and anything you build sit on the
same implementation.

```text
duzo-core      scanning · size aggregation · filesystem metadata · tree model
                snapshots · diffing and growth attribution · category rules
duzo-render    radial layout · half-block canvas · braille canvas · bars
duzo           commands · TUI · output modes
```

`duzo-core` returns neutral data and never formats anything for a display:

```rust
use std::path::Path;
use duzo_core::{scan, ScanOptions, SizeKind, Store};

let tree = scan::scan(
    Path::new("/Users/alice"),
    &ScanOptions::default(),
    &scan::Progress::default(),
    &scan::Cancel::new(),
)?;

// Compare against last time, then remember this time.
let store = Store::open()?;
if let Some(before) = store.latest(tree.root_path()) {
    let diff = duzo_core::diff::diff(
        &before.tree, &tree, SizeKind::Allocated,
        before.taken_at, duzo_core::history::now(), before.floor,
    );
    for change in diff.growth(10) {
        println!("{:>14}  {}", change.delta, change.path.display());
    }
}
store.record(&tree)?;
```

Scans run on rayon and can be driven from a UI thread:

```rust
let (progress, cancel, handle) = scan::scan_in_background(path, ScanOptions::default());
println!("{} items so far", progress.entries());
cancel.cancel();               // returns whatever was counted
let tree = handle.join().unwrap()?;
```

## Development

```bash
make            # check, build, test
make test       # nextest + doctests, or cargo test if nextest is not installed
make lint       # rustfmt + clippy -D warnings
make install    # debug build into ~/.cargo/bin
make release    # bump, changelog, tag, publish (LEVEL=patch|minor|major)
```

Every screen is rendered into an off-screen buffer and asserted at real
terminal widths, so layout regressions — a column that overflows at 60 columns,
a section that eats the list at 12 rows — fail the build rather than the user.

## Platforms

macOS and Linux. Windows compiles but is untested and ships no release binary:
allocated sizes, hard-link dedup and the mount table all fall back to
approximations there.

## License

[MIT](LICENSE) © Cesar Ferreira and Karen Grigoryan
