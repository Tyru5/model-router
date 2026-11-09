//! Search shared by the diff view and a run's output feed, and the feed's
//! follow/pause position.
//!
//! While following, the panel shows the newest output. Scrolling up pauses
//! it: the position is then kept as the log line at the top of the panel, so
//! new output (and old lines leaving the bounded log) never moves the view.
use super::{run_slot::RunSlot, theme::Palette};
use crossterm::event::{KeyCode as Key, KeyEvent, KeyModifiers as Mods};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::ops::Range;

/// Smart-case text search: case-insensitive (ASCII) unless the query has an
/// uppercase letter.
pub(super) fn find(haystack: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return vec![];
    }
    let found = |hay: &str, needle: &str| {
        hay.match_indices(needle)
            .map(|(at, text)| at..at + text.len())
            .collect()
    };
    if query.chars().any(char::is_uppercase) {
        found(haystack, query)
    } else {
        // ASCII folding keeps byte offsets, so ranges index the original.
        found(&haystack.to_ascii_lowercase(), &query.to_ascii_lowercase())
    }
}

/// `line` with the byte `ranges` of its text restyled; `current` (an index
/// into `ranges`) gets `current_style` instead of `style`.
pub(super) fn highlight(
    line: Line<'static>,
    ranges: &[Range<usize>],
    style: Style,
    current: Option<usize>,
    current_style: Style,
) -> Line<'static> {
    if ranges.is_empty() {
        return line;
    }
    let Line {
        spans,
        style: line_style,
        alignment,
    } = line;
    let mut out = Vec::with_capacity(spans.len() + ranges.len() * 2);
    let mut offset = 0;
    for span in spans {
        let text = span.content.as_ref();
        let end = offset + text.len();
        let mut cut = 0;
        for (index, range) in ranges.iter().enumerate() {
            let (start, stop) = (range.start.max(offset), range.end.min(end));
            if start >= stop {
                continue;
            }
            let (start, stop) = (start - offset, stop - offset);
            if start > cut {
                out.push(Span::styled(text[cut..start].to_owned(), span.style));
            }
            let mark = if current == Some(index) {
                current_style
            } else {
                style
            };
            out.push(Span::styled(
                text[start..stop].to_owned(),
                span.style.patch(mark),
            ));
            cut = stop;
        }
        if cut < text.len() {
            out.push(Span::styled(text[cut..].to_owned(), span.style));
        }
        offset = end;
    }
    Line {
        spans: out,
        style: line_style,
        alignment,
    }
}

/// The text of a line, spans joined.
pub(super) fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Where the output panel is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LogView {
    /// Show the newest output as it arrives.
    pub(super) follow: bool,
    /// While paused: the absolute log line at the top, and its wrapped row.
    pub(super) anchor: (u64, usize),
}
impl Default for LogView {
    fn default() -> Self {
        Self {
            follow: true,
            anchor: (0, 0),
        }
    }
}

/// A search typed into the output panel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LogSearch {
    pub(super) query: String,
    /// Still typing the query.
    pub(super) typing: bool,
    /// Index of the current match among `matches`.
    pub(super) current: usize,
}

/// The output log wrapped to the panel width.
pub(super) struct Wrapped {
    pub(super) lines: Vec<Line<'static>>,
    /// First wrapped row of each log line.
    pub(super) starts: Vec<usize>,
    /// Absolute number of the first log line kept.
    pub(super) first: u64,
}
impl Wrapped {
    /// The wrapped row an anchor names; lines that left the log map to 0.
    pub(super) fn row(&self, (line, within): (u64, usize)) -> usize {
        let Some(index) = line.checked_sub(self.first) else {
            return 0;
        };
        let index = usize::try_from(index).unwrap_or(usize::MAX);
        match self.starts.get(index) {
            Some(&start) => {
                let end = self
                    .starts
                    .get(index + 1)
                    .copied()
                    .unwrap_or(self.lines.len());
                (start + within).min(end.saturating_sub(1).max(start))
            }
            None => self.lines.len(),
        }
    }
    /// The anchor of a wrapped row.
    pub(super) fn anchor(&self, row: usize) -> (u64, usize) {
        let index = self.starts.partition_point(|&s| s <= row).saturating_sub(1);
        let start = self.starts.get(index).copied().unwrap_or(0);
        (self.first + index as u64, row.saturating_sub(start))
    }
    /// Top row shown for `view` in a panel `visible` rows tall.
    pub(super) fn top(&self, view: LogView, visible: usize) -> usize {
        let last = self.lines.len().saturating_sub(visible);
        if view.follow {
            last
        } else {
            self.row(view.anchor).min(last)
        }
    }
    /// Every match: (row, byte range in that row).
    pub(super) fn matches(&self, query: &str) -> Vec<(usize, Range<usize>)> {
        if query.is_empty() {
            return vec![];
        }
        self.lines
            .iter()
            .enumerate()
            .flat_map(|(row, line)| {
                find(&line_text(line), query)
                    .into_iter()
                    .map(move |range| (row, range))
            })
            .collect()
    }
}

/// Visible rows of the output panel: highlighted matches, the current one
/// distinct.
pub(super) fn visible_lines(
    wrapped: &Wrapped,
    top: usize,
    visible: usize,
    search: Option<&LogSearch>,
    palette: Palette,
) -> Vec<Line<'static>> {
    let end = (top + visible).min(wrapped.lines.len());
    let rows = &wrapped.lines[top.min(end)..end];
    let Some(search) = search.filter(|s| !s.query.is_empty()) else {
        return rows.to_vec();
    };
    let matches = wrapped.matches(&search.query);
    rows.iter()
        .enumerate()
        .map(|(offset, line)| {
            let row = top + offset;
            let first = matches.partition_point(|(r, _)| *r < row);
            let count = matches[first..].partition_point(|(r, _)| *r == row);
            let ranges: Vec<_> = matches[first..first + count]
                .iter()
                .map(|(_, range)| range.clone())
                .collect();
            let current = (!search.typing)
                .then(|| search.current.checked_sub(first))
                .flatten()
                .filter(|&i| i < count);
            highlight(
                line.clone(),
                &ranges,
                palette.search_match(),
                current,
                palette.search_current(),
            )
        })
        .collect()
}

impl RunSlot {
    /// The output as last drawn, and the panel height.
    fn drawn_output(&self) -> Option<(std::rc::Rc<Wrapped>, usize)> {
        let cache = self.cache.borrow();
        let (_, wrapped) = cache.output.as_ref()?;
        Some((wrapped.clone(), self.log_visible.get().max(1)))
    }

    /// Moves the output by `delta` rows; reaching the end follows again.
    pub(super) fn scroll_output(&mut self, delta: isize) {
        let Some((wrapped, visible)) = self.drawn_output() else {
            return;
        };
        let last = wrapped.lines.len().saturating_sub(visible);
        let top = wrapped.top(self.log_view, visible);
        let top = top.saturating_add_signed(delta).min(last);
        self.log_view = if top >= last {
            LogView::default()
        } else {
            LogView {
                follow: false,
                anchor: wrapped.anchor(top),
            }
        };
    }

    pub(super) fn output_home(&mut self) {
        let Some((wrapped, visible)) = self.drawn_output() else {
            return;
        };
        let last = wrapped.lines.len().saturating_sub(visible);
        if last == 0 {
            self.log_view = LogView::default();
        } else {
            self.log_view = LogView {
                follow: false,
                anchor: wrapped.anchor(0),
            };
        }
    }

    pub(super) fn output_end(&mut self) {
        self.log_view = LogView::default();
    }

    /// Pauses at the current view, or resumes following; returns the status line.
    pub(super) fn toggle_follow(&mut self) -> Option<String> {
        if !self.log_view.follow {
            self.log_view = LogView::default();
            return Some("Following new output.".into());
        }
        let (wrapped, visible) = self.drawn_output()?;
        let top = wrapped.top(self.log_view, visible);
        self.log_view = LogView {
            follow: false,
            anchor: wrapped.anchor(top),
        };
        Some("Output paused; [End] or [f] follows again.".into())
    }

    pub(super) fn start_output_search(&mut self) {
        self.show_output = true;
        self.log_search = Some(LogSearch {
            typing: true,
            ..LogSearch::default()
        });
    }

    /// Keys for a search in the output panel; None passes the key on,
    /// Some carries a status line when there is one.
    pub(super) fn output_search_key(&mut self, key: KeyEvent) -> Option<Option<String>> {
        let search = self.log_search.as_mut()?;
        let plain = key.modifiers - Mods::SHIFT == Mods::NONE;
        if search.typing {
            match key.code {
                Key::Esc => self.log_search = None,
                Key::Enter if search.query.is_empty() => self.log_search = None,
                Key::Enter => {
                    search.typing = false;
                    return Some(self.jump_to_match(None));
                }
                Key::Backspace => {
                    search.query.pop();
                }
                Key::Char(c) if plain => search.query.push(c),
                // Ctrl+C and other shortcuts keep working while typing.
                _ => return None,
            }
            return Some(None);
        }
        match key.code {
            Key::Esc => self.log_search = None,
            Key::Char('n') if plain => return Some(self.jump_to_match(Some(true))),
            Key::Char('N') if plain => return Some(self.jump_to_match(Some(false))),
            _ => return None,
        }
        Some(None)
    }

    /// Moves to the next or previous match, or (None) the nearest one at or
    /// above the bottom of the panel, and pauses the output there. Returns
    /// the status line when nothing matches.
    fn jump_to_match(&mut self, forward: Option<bool>) -> Option<String> {
        let (wrapped, visible) = self.drawn_output()?;
        let search = self.log_search.as_mut()?;
        let matches = wrapped.matches(&search.query);
        if matches.is_empty() {
            return Some(format!("No output matches “{}”.", search.query));
        }
        let top = wrapped.top(self.log_view, visible);
        let current = match forward {
            None => matches
                .iter()
                .rposition(|(row, _)| *row < top + visible)
                .unwrap_or(matches.len() - 1),
            Some(true) => (search.current + 1) % matches.len(),
            Some(false) => search.current.checked_sub(1).unwrap_or(matches.len() - 1),
        };
        search.current = current;
        let row = matches[current].0;
        let last = wrapped.lines.len().saturating_sub(visible);
        let top = if row < top || row >= top + visible {
            // A third of the way down, so what led to it shows too.
            row.saturating_sub(visible / 3).min(last)
        } else {
            top
        };
        // Paused even at the end, so new output does not carry the match away.
        self.log_view = LogView {
            follow: false,
            anchor: wrapped.anchor(top),
        };
        None
    }

    /// The search line under the output panel: the query being typed, or
    /// the match count.
    pub(super) fn output_search_status(&self) -> Option<String> {
        let search = self.log_search.as_ref()?;
        if search.typing {
            return Some(format!("/{}▏", search.query));
        }
        let (wrapped, _) = self.drawn_output()?;
        let count = wrapped.matches(&search.query).len();
        Some(if count == 0 {
            format!("/{} · no matches · [Esc] clears", search.query)
        } else {
            format!(
                "/{} · {}/{count} · [n]/[N] next/previous · [Esc] clears",
                search.query,
                search.current.min(count - 1) + 1
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_is_smart_case_and_keeps_byte_offsets() {
        assert_eq!(find("Error error ERROR", "error"), [0..5, 6..11, 12..17]);
        assert_eq!(find("Error error ERROR", "Error"), vec![0..5]);
        assert_eq!(find("雪 fail 雪 FAIL", "fail"), [4..8, 13..17]);
        assert!(find("anything", "").is_empty());
    }

    #[test]
    fn highlight_splits_spans_at_match_edges() {
        let line = Line::from(vec![Span::raw("ab"), Span::raw("cdab")]);
        let mark = Style::default().add_modifier(ratatui::style::Modifier::REVERSED);
        let cur = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
        let out = highlight(line, &[1..3, 4..6], mark, Some(1), cur);
        let parts: Vec<_> = out
            .spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style == cur))
            .collect();
        assert_eq!(
            parts,
            [
                ("a", false),
                ("b", false),
                ("c", false),
                ("d", false),
                ("ab", true)
            ]
        );
        assert_eq!(out.spans[1].style, mark);
    }

    #[test]
    fn anchors_survive_lines_leaving_the_log() {
        let wrapped = Wrapped {
            lines: (0..6).map(|i| Line::raw(i.to_string())).collect(),
            starts: vec![0, 2, 3],
            first: 10,
        };
        assert_eq!(wrapped.anchor(0), (10, 0));
        assert_eq!(wrapped.anchor(1), (10, 1));
        assert_eq!(wrapped.anchor(4), (12, 1));
        assert_eq!(wrapped.row((12, 1)), 4);
        assert_eq!(wrapped.row((9, 0)), 0, "dropped lines map to the top");
        assert_eq!(wrapped.row((10, 5)), 1, "rows stay within their line");
        let paused = LogView {
            follow: false,
            anchor: (11, 0),
        };
        assert_eq!(wrapped.top(paused, 2), 2);
        assert_eq!(wrapped.top(LogView::default(), 2), 4);
    }
}
