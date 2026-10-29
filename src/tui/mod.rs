//! Terminal setup, the event loop, and the key map.

pub mod app;
mod confirm;
mod details;
mod explorer;
mod footer;
mod overview;
mod picker;
mod preferences;
mod scanning;
mod text;
mod theme;

use std::io::{self, Write, stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::ListState;
use ratatui::{Frame, Terminal};

use app::{App, View};
use preferences::BrowseView;

/// How long a frame waits for input before redrawing anyway. Fast enough that
/// the scan progress looks live, slow enough to stay idle when nothing moves.
const FRAME: Duration = Duration::from_millis(120);

/// A stream of ready keys must still yield to drawing and scan polling.
const INPUT_BUDGET: Duration = Duration::from_millis(8);

type TuiTerminal = Terminal<CrosstermBackend<FrameWriter>>;

/// Collect cell commands before touching stdout. Discard them during unwinding
/// so terminal cleanup cannot repaint after the hook restores the shell.
#[derive(Default)]
struct FrameWriter {
    pending: Vec<u8>,
}

impl Write for FrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Ratatui's drop restores the cursor by flushing its backend.
        if std::thread::panicking() {
            self.pending.clear();
            return Ok(());
        }
        let mut out = stdout().lock();
        let result = out.write_all(&self.pending).and_then(|()| out.flush());
        // A failed write may have sent a prefix; never replay it on cleanup.
        self.pending.clear();
        result
    }
}

/// Widget state that belongs to the screen rather than the app.
#[derive(Default)]
struct Ui {
    list: ListState,
    picker: ListState,
    chart: explorer::ChartCache,
}

pub fn run(mut app: App) -> Result<()> {
    let preference_path = preferences::path();
    let saved_view = preference_path
        .as_deref()
        .map(preferences::load)
        .unwrap_or_default();
    apply_saved_view(&mut app, saved_view);
    let mut terminal = enter()?;
    let result = event_loop(&mut terminal, &mut app);
    let browsing_view = app.browsing_view;
    // The process is about to exit, which hands the memory back at once;
    // freeing a whole-disk tree entry by entry would keep the shell waiting.
    std::mem::forget(app);
    let left = leave(&mut terminal);
    if browsing_view != saved_view
        && let Some(path) = preference_path
        && let Err(error) = preferences::save(&path, browsing_view)
    {
        eprintln!("duzo: could not save the view preference: {error}");
    }
    result.and(left)
}

fn enter() -> Result<TuiTerminal> {
    // Without this a panic leaves the user in raw mode on the alternate
    // screen, with no echo and no prompt.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), EndSynchronizedUpdate, LeaveAlternateScreen, Show);
        previous(info);
    }));

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(
        CrosstermBackend::new(FrameWriter::default()),
    )?)
}

fn leave(terminal: &mut TuiTerminal) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EndSynchronizedUpdate,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn event_loop(terminal: &mut TuiTerminal, app: &mut App) -> Result<()> {
    let mut ui = Ui::default();
    let mut tick = 0usize;

    loop {
        app.poll_scan();
        app.poll_delete();
        app.poll_filesystems();
        app.tick_watch();
        app.advance();
        draw_frame(terminal, app, &mut ui, tick)?;

        // Batch ready keys, but yield while input is still arriving so a
        // sustained burst cannot postpone every redraw until it stops.
        if event::poll(FRAME)? {
            let started = Instant::now();
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key),
                    _ => {}
                }
                if app.quit || started.elapsed() >= INPUT_BUDGET || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }

        tick = tick.wrapping_add(1);
        if app.quit {
            // A scan that has counted everything may still be writing its
            // snapshot; say why the screen waits rather than look hung. Marks
            // or the search box would take the footer's place, and the
            // process is leaving them.
            if app.saving() {
                app.marks.clear();
                app.search_active = false;
                app.status = Some("saving this scan… (q again to skip)".into());
                let _ = draw_frame(terminal, app, &mut ui, tick);
                while app.saving() && !quit_again() {}
            }
            app.cancel_scan();
            return Ok(());
        }
    }
}

fn draw_frame(terminal: &mut TuiTerminal, app: &mut App, ui: &mut Ui, tick: usize) -> Result<()> {
    // Buffering reduces writes; synchronization keeps the terminal from showing
    // part of a frame while it consumes those bytes.
    queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let drawn = terminal
        .draw(|frame| draw(frame, app, ui, tick))
        .map(|_| ());
    let ended = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    drawn?;
    ended?;
    Ok(())
}

/// Wait a moment for a key that says not to wait any longer: `q` or Ctrl-C.
/// A terminal that cannot be read any more ends the wait too, rather than
/// spin on it.
fn quit_again() -> bool {
    match event::poll(Duration::from_millis(5)) {
        Ok(false) => return false,
        Ok(true) => {}
        Err(_) => return true,
    }
    match event::read() {
        Ok(Event::Key(key)) => {
            key.kind == KeyEventKind::Press
                && (key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)))
        }
        Ok(_) => false,
        Err(_) => true,
    }
}

fn draw(frame: &mut Frame, app: &mut App, ui: &mut Ui, tick: usize) {
    let area = frame.area();
    match app.view {
        View::Picker => picker::draw(frame, area, app, &mut ui.picker),
        View::Scanning => scanning::draw(frame, area, app, tick),
        View::Overview => overview::draw(frame, area, app, &mut ui.list, tick),
        View::Explorer => explorer::draw(frame, area, app, &mut ui.chart, tick),
    }
    if app.details {
        details::draw(frame, area, app, tick);
    }
    // The confirmation sits above everything, including the details panel.
    if let Some(pending) = app.confirm.clone() {
        confirm::draw(frame, area, app, &pending);
    }
    if let Some(deleting) = &app.deleting {
        confirm::draw_progress(frame, area, app, deleting);
    }
}

/// Draw one frame into an off-screen buffer of the given size.
///
/// Every screen is laid out by hand against the terminal width, so the layout
/// is worth testing at real sizes rather than only at whatever the developer's
/// window happens to be.
pub fn render(app: &mut App, width: u16, height: u16) -> Result<ratatui::buffer::Buffer> {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
    let mut ui = Ui::default();
    terminal.draw(|frame| draw(frame, app, &mut ui, 0))?;
    Ok(terminal.backend().buffer().clone())
}

/// The rendered frame as plain lines, trailing spaces trimmed.
pub fn render_lines(app: &mut App, width: u16, height: u16) -> Result<Vec<String>> {
    let buffer = render(app, width, height)?;
    Ok((0..height)
        .map(|y| {
            let line: String = (0..width)
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .map(|cell| cell.symbol())
                        .unwrap_or(" ")
                        .to_string()
                })
                .collect();
            line.trim_end().to_string()
        })
        .collect())
}

/// A key press applied to an app, for tests and scripted walkthroughs.
pub fn press(app: &mut App, code: KeyCode) {
    handle_key(app, KeyEvent::new(code, KeyModifiers::NONE));
}

/// Block until any background deletion has finished and been folded back in.
///
/// The event loop polls for this between frames; tests and scripted runs need
/// somewhere to wait instead.
pub fn settle(app: &mut App) {
    while app.deleting.is_some() {
        app.poll_delete();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.quit = true;
        return;
    }

    // A deletion in flight takes only "stop"; nothing else should reach the
    // tree while it is being changed underneath.
    if app.deleting.is_some() {
        if matches!(key.code, KeyCode::Esc) {
            app.stop_delete();
        }
        return;
    }

    // A pending deletion owns the keyboard until it is resolved one way or
    // the other — no navigation key should reach the list behind it.
    if app.confirm.is_some() {
        handle_confirm(app, key);
        return;
    }

    if app.search_active {
        handle_search(app, key);
        return;
    }

    // Any deliberate keystroke means the last message has been read — except
    // `d`, which opens the panel that lists what the last deletion did.
    let opens_deletion_log = matches!(key.code, KeyCode::Char('d'))
        && matches!(app.view, View::Overview | View::Explorer);
    if !opens_deletion_log {
        app.status = None;
        app.outcomes.clear();
    }

    match app.view {
        View::Picker => handle_picker(app, key),
        View::Scanning => handle_scanning(app, key),
        View::Overview | View::Explorer => handle_browsing(app, key),
    }
}

fn handle_confirm(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.cancel_delete(),
        KeyCode::Enter => app.commit_delete(),
        KeyCode::Backspace => app.untype_confirmation(),
        KeyCode::Char(ch) => app.type_confirmation(ch),
        _ => {}
    }
}

fn handle_search(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.clear_search(),
        // Enter keeps the filter but hands the keyboard back to navigation.
        KeyCode::Enter => app.search_active = false,
        KeyCode::Backspace => app.pop_search(),
        KeyCode::Char(ch) => app.push_search(ch),
        _ => {}
    }
}

fn handle_picker(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Down | KeyCode::Char('j') => app.move_picker(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_picker(-1),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.pick_selected_filesystem(),
        _ => {}
    }
}

fn handle_scanning(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Esc => {
            // Once the scan has counted everything there is no partial
            // count to stop at; it is only being compared.
            app.status = Some(if app.scan_counted() {
                app::FINISHING.into()
            } else {
                app.cancel_scan();
                "stopping — showing what was counted so far".into()
            });
        }
        _ => {}
    }
}

fn handle_browsing(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') => app.quit = true,

        KeyCode::Esc => {
            if app.details {
                app.details = false;
            } else if app.search.is_some() {
                app.clear_search();
            } else if app.view == View::Explorer {
                app.show_view(BrowseView::List);
            } else if app.from_picker {
                // Nothing left to close: step back out to the disk list.
                app.return_to_picker();
            }
        }

        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::Home | KeyCode::Char('g') => app.select_first(),
        KeyCode::End | KeyCode::Char('G') => app.select_last(),

        // From the overview, Enter switches to the radial explorer; once
        // there, Enter drills into whatever is selected.
        KeyCode::Enter => match app.view {
            View::Overview => app.show_view(BrowseView::Chart),
            _ => app.open_selected(),
        },
        KeyCode::Tab => {
            let view = if app.view == View::Overview {
                BrowseView::Chart
            } else {
                BrowseView::List
            };
            app.set_browsing_view(view);
        }
        KeyCode::Right | KeyCode::Char('l') => app.open_selected(),
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => app.go_up(),

        KeyCode::Char('/') => app.begin_search(),
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Char('d') => app.details = !app.details,
        KeyCode::Char('a') => app.toggle_size_kind(),
        KeyCode::Char('t') => app.toggle_metric(),
        KeyCode::Char('o') => app.reveal_selected(),
        KeyCode::Char('y') | KeyCode::Char('c') => app.copy_selected_path(),
        KeyCode::Char(' ') => app.toggle_mark(),
        KeyCode::Char('x') | KeyCode::Delete => app.request_delete(),
        KeyCode::Char('r') => app.rescan(),
        KeyCode::Char('R') => app.rescan_root(),
        KeyCode::Char('z') => app.cancel_background_scan(),
        _ => {}
    }
}

/// Open on the view saved last time. A remembered snapshot may already be on
/// screen, painted in the default view before the preference was read.
fn apply_saved_view(app: &mut App, view: BrowseView) {
    app.browsing_view = view;
    if matches!(app.view, View::Overview | View::Explorer) {
        app.show_view(view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Settings;
    use duzo_core::{DiskEntry, EntryType, ScanOptions, SizeKind, Unit};
    use std::path::PathBuf;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn test_app() -> App {
        let mut root = DiskEntry::new(PathBuf::from("/root"), EntryType::Directory);
        root.allocated_size = 300;
        for (name, size) in [("big", 200u64), ("small", 100u64)] {
            let mut child =
                DiskEntry::new(PathBuf::from(format!("/root/{name}")), EntryType::Directory);
            child.allocated_size = size;
            root.children.push(child);
        }

        let mut app = App::new(
            Settings {
                size_kind: SizeKind::Allocated,
                unit: Unit::Decimal,
                top: 20,
                scan_options: ScanOptions::default(),
                show_all_filesystems: false,
                record_snapshots: false,
            },
            false,
        );
        app.root = PathBuf::from("/root");
        app.cwd = PathBuf::from("/root");
        app.tree = Some(root);
        app.view = View::Overview;
        app
    }

    /// The explorer keeps its chart between frames and only reshades it when
    /// the selection moves. Whatever it reuses, each frame must match one drawn
    /// from scratch.
    #[test]
    fn a_kept_chart_draws_what_a_fresh_one_would() {
        fn frame(app: &mut App, ui: &mut Ui, width: u16, height: u16) -> ratatui::buffer::Buffer {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, app, ui, 0)).unwrap();
            terminal.backend().buffer().clone()
        }

        let mut app = test_app();
        app.view = View::Explorer;
        let mut kept = Ui::default();
        let mut check = |app: &mut App, width, height, what: &str| {
            let reused = frame(app, &mut kept, width, height);
            let fresh = frame(app, &mut Ui::default(), width, height);
            assert_eq!(reused, fresh, "{what}");
        };

        check(&mut app, 100, 30, "first frame");
        app.move_selection(1);
        check(&mut app, 100, 30, "selection moved");
        check(&mut app, 80, 24, "terminal resized");
        app.move_selection(-1);
        app.open_selected();
        assert_eq!(app.cwd, PathBuf::from("/root/big"));
        check(&mut app, 80, 24, "another folder");
    }

    #[test]
    fn enter_opens_the_explorer_then_drills_down() {
        let mut app = test_app();
        handle_key(&mut app, press(KeyCode::Enter));
        assert_eq!(app.view, View::Explorer);

        handle_key(&mut app, press(KeyCode::Enter));
        assert_eq!(app.cwd, PathBuf::from("/root/big"));
    }

    /// The chart lists every entry and the list stops at `--top`, so leaving
    /// the chart must not leave the cursor past the end of the list.
    #[test]
    fn switching_to_the_list_keeps_the_cursor_on_a_row() {
        let mut app = test_app();
        app.settings.top = 1;
        let mut extra = DiskEntry::new(PathBuf::from("/root/extra"), EntryType::Directory);
        extra.allocated_size = 50;
        app.tree.as_mut().unwrap().children.push(extra);
        app.view = View::Explorer;
        app.selection = 2;

        handle_key(&mut app, press(KeyCode::Tab));
        assert_eq!(app.view, View::Overview);
        assert!(app.selection < app.rows().len(), "{}", app.selection);
    }

    /// Enter and Esc step between the views; only Tab picks the one scans
    /// open on, so backing out of the chart does not undo opening on it.
    #[test]
    fn only_tab_changes_the_view_scans_open_on() {
        let mut app = test_app();
        app.view = View::Explorer;
        for (key, view, kept) in [
            (KeyCode::Tab, View::Overview, BrowseView::List),
            (KeyCode::Enter, View::Explorer, BrowseView::List),
            (KeyCode::Esc, View::Overview, BrowseView::List),
            (KeyCode::Tab, View::Explorer, BrowseView::Chart),
            (KeyCode::Esc, View::Overview, BrowseView::Chart),
        ] {
            handle_key(&mut app, press(key));
            assert_eq!((app.view, app.browsing_view), (view, kept), "{key:?}");
        }
    }

    #[test]
    fn the_saved_view_replaces_a_snapshot_painted_in_the_default_one() {
        let mut app = test_app();
        app.settings.top = 1;
        let mut extra = DiskEntry::new(PathBuf::from("/root/extra"), EntryType::Directory);
        extra.allocated_size = 50;
        app.tree.as_mut().unwrap().children.push(extra);
        app.view = View::Explorer;
        app.selection = 2;

        apply_saved_view(&mut app, BrowseView::List);
        assert_eq!(app.view, View::Overview);
        assert!(app.selection < app.rows().len(), "{}", app.selection);

        // A scan still running opens on the saved view when it lands.
        app.view = View::Scanning;
        apply_saved_view(&mut app, BrowseView::Chart);
        assert_eq!(app.view, View::Scanning);
        assert_eq!(app.browsing_view, BrowseView::Chart);
    }

    #[test]
    fn escape_unwinds_one_layer_at_a_time() {
        let mut app = test_app();
        app.view = View::Explorer;
        app.details = true;
        app.begin_search();
        app.search_active = false;

        handle_key(&mut app, press(KeyCode::Esc));
        assert!(!app.details);
        handle_key(&mut app, press(KeyCode::Esc));
        assert!(app.search.is_none());
        handle_key(&mut app, press(KeyCode::Esc));
        assert_eq!(app.view, View::Overview);
        // Escape at the top does not quit by accident.
        handle_key(&mut app, press(KeyCode::Esc));
        assert!(!app.quit);
    }

    #[test]
    fn d_keeps_the_deletion_log_until_something_else_is_pressed() {
        let mut app = test_app();
        app.outcomes = vec![crate::deletion::Outcome::Refused {
            path: PathBuf::from("/root/big"),
            reason: crate::deletion::Refusal::MountPoint,
        }];
        app.status = Some("nothing deleted".into());

        handle_key(&mut app, press(KeyCode::Char('d')));
        assert!(app.details);
        assert_eq!(app.outcomes.len(), 1);
        assert!(app.status.is_some());

        handle_key(&mut app, press(KeyCode::Char('j')));
        assert!(app.outcomes.is_empty());
        assert!(app.status.is_none());
    }

    #[test]
    fn typing_a_search_does_not_trigger_navigation_shortcuts() {
        let mut app = test_app();
        handle_key(&mut app, press(KeyCode::Char('/')));
        for ch in "sad".chars() {
            handle_key(&mut app, press(KeyCode::Char(ch)));
        }

        assert_eq!(app.search.as_deref(), Some("sad"));
        assert!(
            !app.details,
            "'d' should have been typed, not toggled details"
        );
        assert_eq!(
            app.sort,
            crate::model::Sort::Size,
            "'s' should not have re-sorted"
        );
    }

    #[test]
    fn enter_leaves_the_search_filter_in_place() {
        let mut app = test_app();
        handle_key(&mut app, press(KeyCode::Char('/')));
        handle_key(&mut app, press(KeyCode::Char('s')));
        handle_key(&mut app, press(KeyCode::Enter));

        assert!(!app.search_active);
        assert_eq!(app.search.as_deref(), Some("s"));
        assert_eq!(app.rows().len(), 1);
    }

    #[test]
    fn ctrl_c_always_quits() {
        let mut app = test_app();
        app.search_active = true;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(app.quit);
    }

    #[test]
    fn navigation_keys_move_the_selection_and_the_directory() {
        let mut app = test_app();
        handle_key(&mut app, press(KeyCode::Down));
        assert_eq!(app.selection, 1);
        handle_key(&mut app, press(KeyCode::Right));
        assert_eq!(app.cwd, PathBuf::from("/root/small"));
        handle_key(&mut app, press(KeyCode::Backspace));
        assert_eq!(app.cwd, PathBuf::from("/root"));
    }

    /// The reported bug: after picking a disk, "up" from the top of the tree
    /// left you stranded with no way back to the list you came from.
    #[test]
    fn going_up_from_the_top_returns_to_the_disk_list() {
        let mut app = test_app();
        app.from_picker = true;
        app.cwd = PathBuf::from("/root/big");

        // First press climbs out of the subdirectory...
        handle_key(&mut app, press(KeyCode::Left));
        assert_eq!(app.cwd, PathBuf::from("/root"));
        assert_eq!(app.view, View::Overview);

        // ...the second leaves the scan entirely.
        handle_key(&mut app, press(KeyCode::Left));
        assert_eq!(app.view, View::Picker);
        assert!(app.tree.is_none(), "the finished scan should be released");
        assert!(!app.quit);
    }

    #[test]
    fn backspace_and_escape_leave_the_scan_the_same_way() {
        for key in [KeyCode::Backspace, KeyCode::Esc, KeyCode::Char('h')] {
            let mut app = test_app();
            app.from_picker = true;
            handle_key(&mut app, press(key));
            assert_eq!(app.view, View::Picker, "{key:?} should step back out");
        }
    }

    /// A path named on the command line has no list behind it, so leaving
    /// would mean landing nowhere.
    #[test]
    fn a_scan_started_from_the_command_line_stays_put() {
        let mut app = test_app();
        app.from_picker = false;

        handle_key(&mut app, press(KeyCode::Left));

        assert_eq!(app.view, View::Overview);
        assert!(app.tree.is_some());
        let status = app.status.unwrap();
        assert!(status.contains("top of this scan"), "{status}");
    }

    #[test]
    fn leaving_a_scan_forgets_what_belonged_to_it() {
        let mut app = test_app();
        app.from_picker = true;
        app.toggle_mark();
        app.begin_search();
        app.push_search('b');
        assert!(!app.marks.is_empty());

        app.return_to_picker();

        assert!(app.marks.is_empty(), "marks point into the abandoned scan");
        assert!(app.search.is_none());
        assert_eq!(app.selection, 0);
        assert!(app.root.as_os_str().is_empty());
    }

    #[test]
    fn escape_during_a_scan_stops_it_rather_than_quitting() {
        let mut app = test_app();
        app.view = View::Scanning;
        handle_key(&mut app, press(KeyCode::Esc));
        assert!(!app.quit);
        assert!(app.status.is_some());
    }
}
