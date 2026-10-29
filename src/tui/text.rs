//! Width-aware text helpers. Column alignment has to count display cells, not
//! chars, or a single CJK filename knocks the whole table crooked.

use unicode_width::UnicodeWidthStr;

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Trim to `max` display cells, marking the cut with an ellipsis.
pub fn truncate(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_string();
    }

    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cell = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + cell > max - 1 {
            break;
        }
        out.push(ch);
        used += cell;
    }
    out.push('…');
    out
}

/// Pad on the right to `width` display cells.
pub fn pad(text: &str, target: usize) -> String {
    let current = width(text);
    if current >= target {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(target - current))
}

/// Fit exactly `width` cells: truncate if long, pad if short.
pub fn fit(text: &str, target: usize) -> String {
    pad(&truncate(text, target), target)
}

/// Long paths lose their middle rather than their tail — the last components
/// are the ones that identify the thing.
pub fn shorten_path(path: &str, max: usize) -> String {
    if width(path) <= max || max < 6 {
        return truncate(path, max);
    }
    let available = max - 1;
    let keep_end = available * 2 / 3;
    let keep_start = available - keep_end;

    let mut start = String::new();
    let mut used = 0;
    for ch in path.chars() {
        let cells = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + cells > keep_start {
            break;
        }
        start.push(ch);
        used += cells;
    }

    let mut end = Vec::new();
    used = 0;
    for ch in path.chars().rev() {
        let cells = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + cells > keep_end {
            break;
        }
        end.push(ch);
        used += cells;
    }
    let end: String = end.into_iter().rev().collect();
    format!("{start}…{end}")
}

/// Shorten a breadcrumb chain (e.g. `["Macintosh HD", "Users", "alice", "code", "files"]`)
/// to fit `max` cells without wrapping or overflowing.
pub fn shorten_breadcrumb<T: AsRef<str>>(parts: &[T], max: usize) -> String {
    if parts.is_empty() {
        return String::new();
    }
    let sep = " › ";
    let sep_width = width(sep);
    let ellipsis_width = width("…");
    let join = |parts: &[T]| {
        parts
            .iter()
            .map(|p| p.as_ref())
            .collect::<Vec<_>>()
            .join(sep)
    };
    let full = join(parts);
    if width(&full) <= max {
        return full;
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_string();
    }
    if parts.len() == 1 {
        return shorten_path(parts[0].as_ref(), max);
    }

    // Try keeping the root volume (first part), replacing intermediate parts with "…",
    // and keeping as many tail components as will fit.
    let first = parts[0].as_ref();
    let first_len = width(first);

    // Try: first › … › [tail components]
    for tail_count in (1..parts.len() - 1).rev() {
        let tail_str = join(&parts[parts.len() - tail_count..]);
        let candidate_width = first_len + sep_width + ellipsis_width + sep_width + width(&tail_str);
        if candidate_width <= max {
            return format!("{first}{sep}…{sep}{tail_str}");
        }
    }

    // If first › … › tail doesn't fit, try dropping first: "… › [tail components]"
    for tail_count in (1..parts.len()).rev() {
        let tail_str = join(&parts[parts.len() - tail_count..]);
        let candidate_width = ellipsis_width + sep_width + width(&tail_str);
        if candidate_width <= max {
            return format!("…{sep}{tail_str}");
        }
    }

    // If even "… › last" doesn't fit, truncate/shorten the last component.
    let last = parts.last().unwrap().as_ref();
    if width(last) <= max {
        last.to_string()
    } else {
        shorten_path(last, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_characters_count_double() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本"), 4);
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate("Downloads", 20), "Downloads");
        assert_eq!(truncate("Downloads", 5), "Down…");
        assert_eq!(truncate("Downloads", 1), "…");
    }

    #[test]
    fn zero_cell_budgets_render_nothing() {
        assert_eq!(truncate("Downloads", 0), "");
        assert_eq!(shorten_path("/home/Downloads", 0), "");
        assert_eq!(shorten_breadcrumb(&["home", "Downloads"], 0), "");
    }

    #[test]
    fn fitting_produces_exactly_the_requested_width() {
        for target in 1..12 {
            assert_eq!(width(&fit("Applications", target)), target);
            assert_eq!(width(&fit("ab", target)), target);
        }
    }

    #[test]
    fn wide_text_never_overflows_its_column() {
        assert!(width(&fit("日本語のフォルダ", 7)) <= 7);
    }

    #[test]
    fn shortened_paths_keep_their_tail() {
        let shortened = shorten_path("/home/alice/code/files/crates/core/src", 20);
        assert!(width(&shortened) <= 20);
        assert!(shortened.ends_with("src"));
        assert!(shortened.contains('…'));
    }

    #[test]
    fn shortened_breadcrumbs_fit_budget() {
        let parts = vec![
            "Macintosh HD".to_string(),
            "Users".to_string(),
            "alice".to_string(),
            "code".to_string(),
            "files".to_string(),
        ];
        // Full is 43 chars
        assert_eq!(
            shorten_breadcrumb(&parts, 50),
            "Macintosh HD › Users › alice › code › files"
        );
        let b40 = shorten_breadcrumb(&parts, 40);
        assert!(width(&b40) <= 40);
        assert_eq!(b40, "Macintosh HD › … › alice › code › files");

        let b30 = shorten_breadcrumb(&parts, 30);
        assert!(width(&b30) <= 30);
        assert_eq!(b30, "Macintosh HD › … › files");

        let b20 = shorten_breadcrumb(&parts, 20);
        assert!(width(&b20) <= 20);
        assert_eq!(b20, "… › code › files");

        let b10 = shorten_breadcrumb(&parts, 10);
        assert!(width(&b10) <= 10);
        assert_eq!(b10, "… › files");

        let b5 = shorten_breadcrumb(&parts, 5);
        assert!(width(&b5) <= 5);
        assert_eq!(b5, "files");
    }

    #[test]
    fn shortened_wide_breadcrumb_fits_cell_budget() {
        let parts = ["root", "日本日本日本"];
        let shortened = shorten_breadcrumb(&parts, 6);

        assert!(
            width(&shortened) <= 6,
            "{shortened:?} is {} cells",
            width(&shortened)
        );
    }
}
