//! The radial explorer: a sunburst of the current directory, with a legend
//! tying every wedge to a name and a size.

use std::sync::{Arc, Weak};

use duzo_core::size::{Unit, format};
use duzo_render::radial::{self, Drawn, RenderOptions};
use duzo_render::{BrailleCanvas, Canvas, Rgb, palette};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::model::Row;
use crate::report::signed;
use crate::tui::app::{App, RadialCache};
use crate::tui::text::{fit, truncate, width};
use crate::tui::theme;

/// Three rings is the sweet spot: enough to see structure two levels down,
/// few enough that the outer ring is still thick enough to read.
const RINGS: usize = 3;

const LEGEND_WIDTH: u16 = 34;

/// Only the chart pixels are cached; progress, labels and the legend stay live.
#[derive(Default)]
pub(super) struct ChartCache {
    frame: Option<ChartFrame>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ChartKey {
    width: u16,
    height: u16,
    no_color: bool,
}

struct ChartFrame {
    // Which sunburst this was drawn from. Weak, so a frame left over from an
    // old tree does not keep that tree alive.
    source: Weak<RadialCache>,
    key: ChartKey,
    pixels: ChartPixels,
    selected: Option<usize>,
    buffer: Buffer,
    drawn: Drawn,
}

enum ChartPixels {
    Color {
        raster: radial::Raster,
        canvas: Canvas,
    },
    Outline(BrailleCanvas),
}

impl ChartFrame {
    fn new(source: &Arc<RadialCache>, key: ChartKey, selected: Option<usize>) -> Self {
        let area = Rect::new(0, 0, key.width, key.height);
        let pixels = if key.no_color {
            ChartPixels::Outline(BrailleCanvas::new(key.width as usize, key.height as usize))
        } else {
            let canvas = Canvas::new(key.width as usize, key.height as usize);
            let raster = radial::Raster::new(
                &source.segments(None),
                canvas.width(),
                canvas.height(),
                &RenderOptions {
                    rings: RINGS,
                    ..Default::default()
                },
            );
            ChartPixels::Color { raster, canvas }
        };
        let mut frame = Self {
            source: Arc::downgrade(source),
            key,
            pixels,
            selected,
            buffer: Buffer::empty(area),
            drawn: Drawn::default(),
        };
        frame.select(source, selected);
        frame
    }

    fn select(&mut self, source: &RadialCache, selected: Option<usize>) {
        let segments = source.segments(selected);
        self.buffer.reset();
        self.drawn = match &mut self.pixels {
            ChartPixels::Color { raster, canvas } => {
                canvas.clear();
                let drawn = raster.paint(&segments, canvas, selected);
                color_cells(canvas, &mut self.buffer);
                drawn
            }
            ChartPixels::Outline(canvas) => {
                canvas.clear();
                let drawn = radial::render_outline(
                    &segments,
                    canvas,
                    &RenderOptions {
                        rings: RINGS,
                        selected,
                        ..Default::default()
                    },
                );
                let lines: Vec<_> = canvas.lines().into_iter().map(Line::from).collect();
                Paragraph::new(lines).render(self.buffer.area, &mut self.buffer);
                drawn
            }
        };
        self.selected = selected;
    }
}

fn color_cells(canvas: &Canvas, buffer: &mut Buffer) {
    for (row, cells) in canvas
        .cell_rows()
        .into_iter()
        .zip(buffer.content.chunks_exact_mut(canvas.width()))
    {
        for (half, cell) in row.into_iter().zip(cells) {
            match (half.top, half.bottom) {
                (None, None) => {}
                (Some(top), None) => {
                    cell.set_symbol("▀").set_fg(theme::color(top));
                }
                (None, Some(bottom)) => {
                    cell.set_symbol("▄").set_fg(theme::color(bottom));
                }
                (Some(top), Some(bottom)) => {
                    cell.set_symbol("▀")
                        .set_fg(theme::color(top))
                        .set_bg(theme::color(bottom));
                }
            }
        }
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App, chart: &mut ChartCache, tick: usize) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // breadcrumb
        Constraint::Length(1),
        Constraint::Min(6),    // chart and legend
        Constraint::Length(1), // the selected name, in full
        Constraint::Length(1),
    ])
    .horizontal_margin(1)
    .split(area);

    let legend_width = LEGEND_WIDTH.min(chunks[2].width.saturating_sub(20));
    let body = Layout::horizontal([Constraint::Min(20), Constraint::Length(legend_width)])
        .split(chunks[2]);

    let rows = app.rows();
    let drawn = draw_chart(frame, body[0], app, &rows, chart);
    if legend_width > 0 {
        draw_legend(frame, body[1], app, &rows, tick);
    }
    draw_selection(frame, chunks[3], app, &rows);

    // The breadcrumb comes after the chart because what the chart could draw
    // decides what this line has to admit.
    let total_width = chunks[0].width as usize;
    let mut right_span = None;
    let mut right_len = 0;

    let crumb_floor = crate::tui::overview::MIN_PATH_WIDTH
        .min(crate::tui::text::width(&app.breadcrumb_parts().join(" › ")));
    let room = total_width.saturating_sub(2 + crumb_floor);
    if let Some(note) = crate::tui::overview::rescan_note(app, tick, room) {
        right_len = crate::tui::text::width(&note);
        right_span = Some(Span::styled(note, theme::warning()));
    }

    let mut left_note = None;
    let mut left_note_len = 0;
    if let Some(note) = note(app, &rows, drawn) {
        let note_str = format!("   {note}");
        left_note_len = crate::tui::text::width(&note_str);
        left_note = Some(Span::styled(note_str, theme::secondary()));
    }

    let gap = if right_span.is_some() { 2 } else { 0 };
    let note_len = if total_width.saturating_sub(right_len + gap + left_note_len) >= 12 {
        left_note_len
    } else {
        left_note = None;
        0
    };

    let crumb_budget = total_width.saturating_sub(right_len + gap + note_len);
    let crumb_text = crate::tui::text::shorten_breadcrumb(&app.breadcrumb_parts(), crumb_budget);
    let crumb_len = crate::tui::text::width(&crumb_text);

    let left_len = crumb_len + note_len;
    let padding = total_width.saturating_sub(left_len + right_len);

    let mut crumbs = vec![Span::styled(crumb_text, theme::heading())];
    if let Some(n) = left_note {
        crumbs.push(n);
    }
    if let Some(r) = right_span {
        if padding > 0 {
            crumbs.push(Span::raw(" ".repeat(padding)));
        }
        crumbs.push(r);
    }
    frame.render_widget(Paragraph::new(Line::from(crumbs)), chunks[0]);

    crate::tui::footer::draw(frame, chunks[4], app, tick);
}

/// What the chart could not say for itself.
///
/// Anything under about a percent of a directory is a fraction of a pixel of
/// arc, and a long tail of them shares one pixel. The highlight covers the
/// whole run in that case, so this says as much — a cursor that moves while the
/// chart does not otherwise just looks broken.
///
/// It goes on the breadcrumb line rather than in the footer: the keys are worth
/// more than a note, and the footer would drop two of them to fit it.
fn note(app: &App, rows: &[Row], drawn: Drawn) -> Option<String> {
    if !drawn.selection_is_a_run {
        return None;
    }
    let share = rows.get(app.selection)?.fraction * 100.0;
    Some(match share {
        share if share >= 0.1 => format!("{share:.1}% of this folder — too small to place"),
        _ => "under 0.1% of this folder — too small to place".to_string(),
    })
}

fn draw_chart(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    rows: &[Row],
    chart: &mut ChartCache,
) -> Drawn {
    if area.width < 8 || area.height < 4 {
        return Drawn::default();
    }

    let cache = app.radial_tree(RINGS);

    // The selected row and the highlighted wedge are the same thing seen two
    // ways, so they are looked up from one source: the selected path.
    let selected_id = rows
        .get(app.selection)
        .and_then(|row| row.path.as_deref())
        .and_then(|path| cache.id_of(path));

    let key = ChartKey {
        width: area.width,
        height: area.height,
        no_color: theme::colors_disabled(),
    };
    let cached = chart
        .frame
        .get_or_insert_with(|| ChartFrame::new(&cache, key, selected_id));
    if cached.key != key || !cached.source.ptr_eq(&Arc::downgrade(&cache)) {
        *cached = ChartFrame::new(&cache, key, selected_id);
    } else if cached.selected != selected_id {
        cached.select(&cache, selected_id);
    }
    let buffer = frame.buffer_mut();
    for (y, row) in cached
        .buffer
        .content
        .chunks_exact(area.width as usize)
        .enumerate()
    {
        let start = buffer.index_of(area.x, area.y + y as u16);
        buffer.content[start..start + row.len()].clone_from_slice(row);
    }
    draw_hole(frame, area, app);
    cached.drawn
}

/// The hole in the middle carries the total for the directory you are in —
/// the number the whole chart is a breakdown of.
fn draw_hole(frame: &mut Frame, area: Rect, app: &App) {
    let Some(entry) = app.current_entry() else {
        return;
    };

    let name = entry.name().to_string();
    let size = match (app.showing_growth(), &app.diff) {
        (true, Some(diff)) => signed(diff.total_delta(), app.settings.unit),
        _ => format(entry.size(app.settings.size_kind), app.settings.unit),
    };
    let box_width = (width(&name).max(width(&size)) as u16 + 2).min(area.width);
    if box_width == 0 || area.height < 4 {
        return;
    }

    let hole = Rect {
        x: area.x + area.width.saturating_sub(box_width) / 2,
        y: area.y + area.height / 2 - 1,
        width: box_width,
        height: 2,
    };

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(size, theme::heading())),
            Line::from(Span::styled(name, theme::muted())),
        ])
        .alignment(Alignment::Center),
        hole,
    );
}

/// The legend column is too narrow for most file names, so the selected one is
/// repeated here in full across the whole width — otherwise the only way to
/// read a name is to open the folder it is in.
fn draw_selection(frame: &mut Frame, area: Rect, app: &App, rows: &[Row]) {
    let Some(row) = rows.get(app.selection) else {
        return;
    };
    let total = area.width as usize;
    if total < 8 {
        return;
    }

    let growth = app.showing_growth();
    let unit = app.settings.unit;
    let size = amount(row, growth, unit);
    let (_, scale) = app.legend_measure();
    // The swatch ties this line to the wedge and the legend row it repeats.
    let name = truncate(&row.name, total.saturating_sub(width(&size) + 5));

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "■ ",
                Style::default().fg(theme::color(color_of(row, growth, scale))),
            ),
            Span::styled(name, theme::heading()),
            Span::raw("   "),
            Span::styled(size, theme::secondary()),
        ])),
        area,
    );
}

fn draw_legend(frame: &mut Frame, area: Rect, app: &App, rows: &[Row], tick: usize) {
    let unit = app.settings.unit;
    if area.height == 0 {
        return;
    }
    if rows.is_empty() {
        let message = truncate(
            &crate::tui::overview::no_rows_message(app, tick),
            area.width as usize,
        );
        frame.render_widget(Paragraph::new(Span::styled(message, theme::muted())), area);
        return;
    }

    // The bottom line says what the legend does not list yet, as in the
    // overview.
    let mut area = area;
    if area.height >= 2
        && let Some(line) = crate::tui::overview::unlisted_line(app, tick, area.width as usize)
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
    let capacity = area.height as usize;

    // Keep the selection on screen when the legend is shorter than the list.
    let offset = app.selection.saturating_sub(capacity.saturating_sub(1));
    let visible = rows.iter().skip(offset).take(capacity);

    let growth = app.showing_growth();
    let (size_width, scale) = app.legend_measure();
    let amount = |row: &Row| amount(row, growth, unit);
    let name_width = (area.width as usize).saturating_sub(size_width + 5);

    let lines: Vec<Line> = visible
        .enumerate()
        .map(|(index, row)| {
            let color = color_of(row, growth, scale);
            let is_selected = offset + index == app.selection;
            let marked = row
                .path
                .as_ref()
                .is_some_and(|path| app.marks.contains(path));

            let name_style = if is_selected {
                theme::selected()
            } else {
                Style::default()
            };

            Line::from(vec![
                Span::styled("■ ", Style::default().fg(theme::color(color))),
                Span::styled(fit(&row.name, name_width), name_style),
                Span::raw(" "),
                Span::styled(format!("{:>size_width$}", amount(row)), theme::secondary()),
                Span::styled(if marked { " ✓" } else { "" }, theme::accent()),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
}

/// What a row is being drawn by: its size, or its change when growth is on.
fn amount(row: &Row, growth: bool, unit: Unit) -> String {
    if growth {
        signed(row.delta.unwrap_or(0), unit)
    } else {
        format(row.size, unit)
    }
}

/// The legend's measure of every row: the widest amount, which the column is
/// lined up to, and the biggest change, which growth colours are relative to.
/// Both look at every row, so the app keeps them (`App::legend_measure`).
pub(crate) fn measure(rows: &[Row], growth: bool, unit: Unit) -> (usize, i64) {
    let size_width = rows
        .iter()
        .map(|row| width(&amount(row, growth, unit)))
        .max()
        .unwrap_or(8);
    (size_width, scale(rows))
}

fn scale(rows: &[Row]) -> i64 {
    rows.iter()
        .map(|row| row.delta.unwrap_or(0).abs())
        .max()
        .unwrap_or(1)
        .max(1)
}

fn color_of(row: &Row, growth: bool, scale: i64) -> Rgb {
    if growth {
        palette::growth_color(row.delta.unwrap_or(0), scale)
    } else {
        palette::categorical(row.color_index)
    }
}
