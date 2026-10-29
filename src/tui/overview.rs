//! The ranked list: what is full, what is using the space, where to look
//! next — and nothing else.

use duzo_core::size::{format, format_percent, format_percent_whole};
use duzo_core::{DiskEntry, ScanState};
use duzo_render::{bar, palette};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::model::{self, Row};
use crate::report::signed;
use crate::tui::app::{App, Listing};
use crate::tui::text::{fit, width};
use crate::tui::theme;

/// Below this the percentage column is the first thing to go: the bar and the
/// absolute size already carry the message.
const PERCENT_MIN_WIDTH: u16 = 76;

const BAR_MAX_WIDTH: usize = 32;

/// Four is enough to point somewhere without becoming a second ranked list.
const LARGEST_ITEMS: usize = 4;

pub fn draw(frame: &mut Frame, area: Rect, app: &App, list_state: &mut ListState, tick: usize) {
    let rows = app.rows();
    let (list_height, largest_height) = split_body(app, area, rows.len());

    let chunks = Layout::vertical([
        Constraint::Length(1), // volume and capacity
        Constraint::Length(1), // capacity bar
        Constraint::Length(1),
        Constraint::Length(1), // current folder
        Constraint::Length(1),
        Constraint::Length(list_height), // ranked entries
        Constraint::Length(largest_height),
        Constraint::Min(0),    // slack, so both blocks sit under the header
        Constraint::Length(1), // footer
    ])
    .horizontal_margin(1)
    .split(area);

    draw_capacity(frame, chunks[0], chunks[1], app);
    draw_current_folder(frame, chunks[3], app, tick);
    draw_rows(frame, chunks[5], app, &rows, list_state, tick);
    if largest_height > 0 {
        draw_largest(frame, chunks[6], app);
    }
    crate::tui::footer::draw(frame, chunks[8], app, tick);
}

/// Divide the space between the header and the footer.
///
/// The ranked list is the answer and gets what it needs first; "Largest items"
/// is the follow-up question and only appears if there is room left for a
/// heading and at least two entries.
fn split_body(app: &App, area: Rect, row_count: usize) -> (u16, u16) {
    const HEADER: usize = 5;
    const FOOTER: usize = 1;
    const LARGEST_MIN: usize = 4; // blank, heading, two entries

    let body = (area.height as usize).saturating_sub(HEADER + FOOTER);
    if body == 0 {
        return (0, 0);
    }

    // The line saying what a partly listed folder does not list yet is drawn
    // inside the list's area, so it needs a line of its own there.
    let wanted = row_count.max(1) + usize::from(app.cwd_unlisted().is_some());
    let largest_available = body.saturating_sub(wanted);
    let largest = if largest_available >= LARGEST_MIN
        && app.current_entry().is_some()
        && !app.showing_growth()
    {
        largest_available.min(LARGEST_ITEMS + 2)
    } else {
        0
    };

    let list = wanted.min(body - largest);
    (list as u16, largest as u16)
}

/// The header always describes the *filesystem*, never the directory being
/// explored. Conflating the two is the single most confusing thing a disk tool
/// can do.
fn draw_capacity(frame: &mut Frame, name_area: Rect, bar_area: Rect, app: &App) {
    let unit = app.settings.unit;

    let (name, right) = match &app.filesystem {
        Some(fs) => (
            fs.name.clone(),
            format!(
                "{} used of {}",
                format(fs.used, unit),
                format(fs.total, unit)
            ),
        ),
        None => (model::display_path(&app.root), String::new()),
    };

    let total = name_area.width as usize;
    let gap = total.saturating_sub(width(&name) + width(&right));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(name, theme::heading()),
            Span::raw(" ".repeat(gap)),
            Span::styled(right, theme::secondary()),
        ])),
        name_area,
    );

    let Some(fs) = &app.filesystem else { return };
    let fraction = fs.used_fraction();
    let percent = format_percent_whole(fraction);
    let gauge_width = (bar_area.width as usize)
        .saturating_sub(width(&percent) + 2)
        .min(64);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                bar::gauge(fraction, gauge_width),
                Style::default().fg(theme::color(palette::capacity_color(fraction))),
            ),
            Span::raw("  "),
            Span::styled(percent, theme::secondary()),
        ])),
        bar_area,
    );
}

/// And this line always describes the directory, so the two numbers can never
/// be mistaken for each other.
fn draw_current_folder(frame: &mut Frame, area: Rect, app: &App, tick: usize) {
    let Some(entry) = app.current_entry() else {
        return;
    };
    let unit = app.settings.unit;
    let total = area.width as usize;
    if total < 8 {
        return;
    }

    let prefix = "Current folder: ";
    let size_str = format!("    {}", format(entry.size(app.settings.size_kind), unit));
    let left_fixed_width = crate::tui::text::width(prefix) + crate::tui::text::width(&size_str);

    let mut growth_span = None;
    let mut growth_width = 0;
    if let (true, Some(diff)) = (app.showing_growth(), &app.diff) {
        let diff_str = format!(
            "    {} in {}",
            signed(diff.total_delta(), unit),
            crate::timefmt::humanize(diff.elapsed())
        );
        growth_width = crate::tui::text::width(&diff_str);
        growth_span = Some(Span::styled(
            diff_str,
            Style::default().fg(theme::color(palette::growth_color(
                diff.total_delta(),
                diff.total_delta().abs().max(1),
            ))),
        ));
    }

    let mut right_span = None;
    let mut right_width = 0;

    // A refresh keeps the old numbers on screen until it lands; say so, even
    // when the footer is busy reporting a deletion — but never at the cost of
    // the folder's own name.
    let display_p = model::display_path(&app.cwd);
    let path_floor = MIN_PATH_WIDTH.min(crate::tui::text::width(&display_p));
    let room = total.saturating_sub(left_fixed_width + growth_width + 2 + path_floor);
    let note = rescan_note(app, tick, room)
        .or_else(|| incomplete_note(entry).filter(|note| crate::tui::text::width(note) <= room));
    if let Some(note) = note {
        right_width = crate::tui::text::width(&note);
        right_span = Some(Span::styled(note, theme::warning()));
    }

    let gap = if right_span.is_some() { 2 } else { 0 };
    let path_budget = total.saturating_sub(left_fixed_width + growth_width + right_width + gap);
    let path_text = if crate::tui::text::width(&display_p) <= path_budget {
        display_p
    } else if path_budget >= 6 {
        crate::tui::text::shorten_path(&display_p, path_budget)
    } else {
        crate::tui::text::truncate(&display_p, path_budget.max(1))
    };

    let path_width = crate::tui::text::width(&path_text);
    let left_width = crate::tui::text::width(prefix)
        + path_width
        + crate::tui::text::width(&size_str)
        + growth_width;
    let padding = total.saturating_sub(left_width + right_width);

    let mut spans = vec![
        Span::styled(prefix, theme::muted()),
        Span::raw(path_text),
        Span::styled(size_str, theme::accent()),
    ];
    if let Some(g) = growth_span {
        spans.push(g);
    }
    if let Some(r) = right_span {
        if padding > 0 {
            spans.push(Span::raw(" ".repeat(padding)));
        }
        spans.push(r);
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Columns a header keeps for the folder's name (or all of a shorter one)
/// before it makes room for a note beside it.
pub(crate) const MIN_PATH_WIDTH: usize = 12;

/// The header note while the folder on screen is still being counted, shared
/// by the list and the chart so the two never disagree. It drops the
/// snapshot's age, then the whole note, when it would not fit in `room`.
pub(crate) fn rescan_note(app: &App, tick: usize, room: usize) -> Option<String> {
    let short = format!("{} rescanning…", theme::spinner(tick));
    let full = if let Some(taken_at) = app.provisional_for_cwd() {
        let age = crate::timefmt::ago(duzo_core::history::now().saturating_sub(taken_at));
        if !app.cwd_is_being_scanned() {
            // A stopped scan left the snapshot on screen.
            return Some(format!("as of {age}"))
                .filter(|note| crate::tui::text::width(note) <= room);
        }
        format!("as of {age} · {short}")
    } else if app.cwd_is_being_scanned() {
        short.clone()
    } else {
        return None;
    };
    [full, short]
        .into_iter()
        .find(|note| crate::tui::text::width(note) <= room)
}

/// Says so out loud when the numbers are a floor rather than a total.
fn incomplete_note(entry: &DiskEntry) -> Option<String> {
    match entry.scan_state() {
        ScanState::Complete => None,
        ScanState::Denied => Some("unreadable".to_string()),
        ScanState::Cancelled => Some("scan cancelled — sizes incomplete".to_string()),
        ScanState::Skipped => Some("not scanned — mount or system folder".to_string()),
        ScanState::Partial => Some("some folders skipped or unreadable".to_string()),
    }
}

fn draw_rows(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    rows: &[Row],
    list_state: &mut ListState,
    tick: usize,
) {
    if rows.is_empty() {
        let message = no_rows_message(app, tick);
        let message = crate::tui::text::truncate(&message, area.width as usize);
        frame.render_widget(Paragraph::new(Span::styled(message, theme::muted())), area);
        return;
    }

    // The bottom line, kept clear of the rows so it shows however long the
    // list is.
    let mut area = area;
    if area.height >= 2
        && let Some(line) = unlisted_line(app, tick, area.width as usize)
    {
        area.height -= 1;
        let line_area = Rect {
            y: area.y + area.height,
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new(Span::styled(line, theme::warning())),
            line_area,
        );
    }

    let unit = app.settings.unit;
    let growth = app.showing_growth();
    let show_percent = area.width >= PERCENT_MIN_WIDTH && !growth;

    // In growth mode the leading column is the signed change, which is what
    // the whole view exists to show; the current size moves to the right.
    let amount = |row: &Row| -> String {
        if growth {
            signed(row.delta.unwrap_or(0), unit)
        } else {
            format(row.size, unit)
        }
    };

    let size_width = rows
        .iter()
        .map(|row| width(&amount(row)))
        .max()
        .unwrap_or(8);
    let name_width = rows
        .iter()
        .map(|row| width(&row.name))
        .max()
        .unwrap_or(12)
        .clamp(10, 28);
    let percent_width = if show_percent {
        7
    } else if growth {
        11
    } else {
        0
    };
    let scale = rows
        .iter()
        .map(|row| row.delta.unwrap_or(0).abs())
        .max()
        .unwrap_or(1)
        .max(1);

    // Marker, size, name, then whatever is left goes to the bar.
    let fixed = 2 + size_width + 2 + name_width + 2 + percent_width;
    let bar_width = (area.width as usize)
        .saturating_sub(fixed + 1)
        .min(BAR_MAX_WIDTH);

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let color = if growth {
                palette::growth_color(row.delta.unwrap_or(0), scale)
            } else if row.is_other() {
                palette::CATEGORICAL[7]
            } else {
                palette::categorical(row.color_index)
            };
            let marked = row
                .path
                .as_ref()
                .is_some_and(|path| app.marks.contains(path));

            let mut spans = vec![
                Span::styled(if marked { "✓ " } else { "  " }, theme::accent()),
                Span::styled(format!("{:>size_width$}", amount(row)), theme::heading()),
                Span::raw("  "),
                Span::raw(fit(&row.name, name_width)),
                Span::raw("  "),
                Span::styled(
                    fit(&bar::bar(row.fraction, bar_width), bar_width),
                    Style::default().fg(theme::color(color)),
                ),
            ];
            if show_percent {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{:>5}", format_percent(row.fraction)),
                    theme::muted(),
                ));
            } else if growth {
                // What it is now, so a "+2 GB" has something to be 2 GB of.
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{:>9}", format(row.size, unit)),
                    theme::muted(),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    list_state.select(Some(app.selection.min(rows.len() - 1)));
    frame.render_stateful_widget(
        List::new(items).highlight_style(theme::selected()),
        area,
        list_state,
    );
}

/// The line under a folder's rows when they are not all it holds: how much
/// is missing, and whether it is on its way. Without it, entries the running
/// scan adds later appear out of nowhere above rows that looked complete.
/// Shared by the list here and the explorer's legend: the longest wording
/// that fits `max_width` wins, dropping detail before the action to take.
pub(crate) fn unlisted_line(app: &App, tick: usize, max_width: usize) -> Option<String> {
    let (items, bytes) = app.cwd_unlisted()?;
    let spinner = theme::spinner(tick);
    let count = crate::output::count(items);
    let size = format(bytes, app.settings.unit);
    let candidates: Vec<String> = if app.cwd_is_being_scanned() && app.cwd_focus_refused() {
        // Nothing counts this folder ahead of the rest.
        let mut candidates = Vec::new();
        if items > 0 {
            candidates.push(format!(
                "{spinner} {count} more · {size} — waiting for the full scan"
            ));
            candidates.push(format!(
                "{spinner} {count} more — waiting for the full scan"
            ));
        }
        candidates.push(format!("{spinner} waiting for the full scan"));
        candidates
    } else if app.cwd_is_being_scanned() {
        if items == 0 {
            vec![format!("{spinner} counting the rest…")]
        } else {
            vec![
                format!("{spinner} counting {count} more · {size}…"),
                format!("{spinner} counting {count} more…"),
                format!("{spinner} counting the rest…"),
            ]
        }
    } else {
        // `r` waits for a scan elsewhere.
        let action = if app.progress().is_some() {
            "r after scan"
        } else {
            "press r"
        };
        let mut candidates = Vec::new();
        if items > 0 {
            candidates.push(format!("+ {count} more · {size} not listed — {action}"));
            candidates.push(format!("+{count} · {size} unlisted — {action}"));
            candidates.push(format!("+{count} unlisted — {action}"));
        }
        candidates.push(format!("not all listed — {action}"));
        candidates
    };
    let fits = |text: &&String| crate::tui::text::width(text) <= max_width;
    let chosen = candidates.iter().find(fits).or(candidates.last())?;
    Some(crate::tui::text::truncate(chosen, max_width))
}

/// What to say when the current folder has no rows to show. Shared by the
/// list here and the explorer's legend, so the two never disagree; every
/// message fits the legend's 34 columns.
pub(crate) fn no_rows_message(app: &App, tick: usize) -> String {
    // `/` opens an empty query, which matches everything: until something is
    // typed, no rows says nothing about the search.
    if app.search.as_deref().is_some_and(|query| !query.is_empty()) {
        // In a folder not fully listed yet, a match may still be on its way;
        // past `--depth` nothing below was kept to search.
        return match app.cwd_listing() {
            Listing::Complete => "nothing matches that search".to_string(),
            Listing::Short => "nothing listed matches yet".to_string(),
            Listing::BeyondDepth => "nothing listed — beyond --depth".to_string(),
            Listing::Skipped => "nothing listed — not scanned".to_string(),
        };
    }
    if app.showing_growth() {
        return "nothing here changed".to_string();
    }
    match app.cwd_listing() {
        // Nothing is missing, so no scan will add anything: say so now
        // rather than spin for as long as the whole scan takes.
        Listing::Complete => "this folder is empty".to_string(),
        // The run's own root: running duzo here is what was just done.
        Listing::BeyondDepth if app.cwd == app.root => "beyond --depth".to_string(),
        Listing::BeyondDepth => "beyond --depth — run duzo here".to_string(),
        Listing::Skipped => "not scanned — run duzo here".to_string(),
        Listing::Short if app.cwd_is_being_scanned() && app.cwd_focus_refused() => {
            format!("{} waiting for the full scan…", theme::spinner(tick))
        }
        Listing::Short if app.cwd_is_being_scanned() => {
            format!("{} scanning this folder…", theme::spinner(tick))
        }
        // A scan elsewhere is running, and `r` waits for it.
        Listing::Short if app.progress().is_some() => {
            "not fully counted — r after scan".to_string()
        }
        Listing::Short => "not fully counted — press r".to_string(),
    }
}

fn draw_largest(frame: &mut Frame, area: Rect, app: &App) {
    // In growth mode the ranked list already answers "where should I look
    // next"; a second list of merely-large things would compete with it.
    if app.showing_growth() {
        return;
    }
    // One line goes to the blank separator and one to the heading.
    let count = (area.height as usize).saturating_sub(2);
    let items = app.largest_items(count);
    if items.is_empty() {
        return;
    }

    let unit = app.settings.unit;
    let size_width = items
        .iter()
        .map(|(_, size)| width(&format(*size, unit)))
        .max()
        .unwrap_or(8);

    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled("Largest items", theme::muted())),
    ];
    lines.extend(items.iter().map(|(path, size)| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{:>size_width$}", format(*size, unit)),
                theme::heading(),
            ),
            Span::raw("   "),
            Span::styled(model::display_path(path), theme::secondary()),
        ])
    }));

    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use duzo_core::EntryType;

    #[test]
    fn skipped_entries_use_a_reason_valid_for_every_mount_kind() {
        let mut entry = DiskEntry::new("mounted", EntryType::Directory);
        entry.set_scan_state(ScanState::Skipped);

        assert_eq!(
            incomplete_note(&entry).as_deref(),
            Some("not scanned — mount or system folder")
        );
    }
}
