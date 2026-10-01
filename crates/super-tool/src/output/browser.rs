use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{
    Check, Results, TICKS, presentation,
    style::{Ink, Line, panel},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    List,
    Inspect,
}

pub(super) struct Browser {
    pub mode: Mode,
    pub tick: usize,
    pub release_alert: Option<String>,
    details: bool,
    can_go_back: bool,
    row: usize,
    attempt: Option<u64>,
    list_start: usize,
    history_start: usize,
    scroll: usize,
    page: usize,
    content_rows: usize,
    header_clipped: bool,
}

impl Browser {
    pub fn new(results: &Results) -> Self {
        Self {
            mode: Mode::List,
            tick: 0,
            release_alert: None,
            details: false,
            can_go_back: false,
            row: 0,
            attempt: results
                .rows
                .first()
                .and_then(|row| row.current.as_ref())
                .map(|check| check.number),
            list_start: 0,
            history_start: 0,
            scroll: 0,
            page: 1,
            content_rows: 0,
            header_clipped: false,
        }
    }

    pub fn configure(&mut self, details: bool, inspect: bool) {
        self.details = details;
        self.mode = if inspect { Mode::Inspect } else { Mode::List };
        self.can_go_back = false;
    }

    pub fn closes(&self, key: KeyEvent) -> bool {
        (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Esc && !self.can_go_back)
    }

    pub fn selected<'a>(&self, results: &'a Results) -> Option<&'a Check> {
        if !self.focused(results) {
            return results.rows.get(self.row)?.current.as_ref();
        }
        let attempts = results.attempts(self.row);
        attempts
            .iter()
            .copied()
            .find(|check| Some(check.number) == self.attempt)
            .or_else(|| results.rows.get(self.row)?.current.as_ref())
            .or_else(|| attempts.first().copied())
    }

    fn focused(&self, results: &Results) -> bool {
        self.mode == Mode::Inspect || results.rows.len() == 1
    }

    fn can_inspect(&self, results: &Results) -> bool {
        self.mode == Mode::List
            && (results.rows.len() > 1
                || results.attempts(self.row).len() > 1
                || self
                    .selected(results)
                    .is_some_and(|check| check.terminal || !check.presentation.details.is_empty()))
    }

    pub fn history_action(&self, results: &Results) -> Option<&'static str> {
        if !self.focused(results) || results.history_loading {
            None
        } else if results.history_error.is_some() {
            Some("n retry history")
        } else if results.next_before.is_some() {
            Some("n older")
        } else if !results.history_loaded {
            Some("n load history")
        } else {
            None
        }
    }

    pub fn key(&mut self, code: KeyCode, results: &Results, height: usize, width: usize) {
        // Use the current wrapped layout, never a page size cached before evidence grew.
        self.frame(results, width, height, false);
        let before = (self.mode, self.row, self.attempt);
        match code {
            KeyCode::Enter if self.can_inspect(results) => {
                self.attempt = self
                    .selected(results)
                    .or_else(|| results.attempts(self.row).first().copied())
                    .map(|check| check.number);
                self.mode = Mode::Inspect;
                self.can_go_back = true;
                self.history_start = 0;
            }
            KeyCode::Esc if self.can_go_back => {
                self.mode = Mode::List;
                self.can_go_back = false;
            }
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(self.page.saturating_sub(1).max(1)),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page.saturating_sub(1).max(1)),
            KeyCode::Up | KeyCode::Down => {
                let up = code == KeyCode::Up;
                if self.focused(results) && results.attempts(self.row).len() > 1 {
                    let attempts = results.attempts(self.row);
                    let index = attempts
                        .iter()
                        .position(|check| Some(check.number) == self.attempt)
                        .unwrap_or(0);
                    let next = if up {
                        index.saturating_sub(1)
                    } else {
                        (index + 1).min(attempts.len().saturating_sub(1))
                    };
                    self.attempt = attempts.get(next).map(|check| check.number);
                } else if self.mode == Mode::Inspect || results.rows.len() == 1 {
                    self.scroll = if up {
                        self.scroll.saturating_sub(1)
                    } else {
                        self.scroll.saturating_add(1)
                    };
                } else {
                    self.row = if up {
                        self.row.saturating_sub(1)
                    } else {
                        (self.row + 1).min(results.rows.len().saturating_sub(1))
                    };
                }
            }
            _ => {}
        }
        if before != (self.mode, self.row, self.attempt) {
            self.scroll = 0;
        }
        self.scroll = self.scroll.min(self.content_rows.saturating_sub(self.page));
    }

    pub fn frame(&mut self, results: &Results, width: usize, height: usize, reconnecting: bool) -> Vec<String> {
        self.row = self.row.min(results.rows.len().saturating_sub(1));
        if self.focused(results) && self.attempt.is_none() {
            self.attempt = self.selected(results).map(|check| check.number);
        }
        let content_width = width.saturating_sub(4).max(1);
        let mut lines = Vec::new();
        if let Some(message) = &self.release_alert {
            lines = Line::default()
                .push("Super CLI notice: ", Ink::Warning, true)
                .plain(message)
                .wrapped(content_width);
            // The complete notice remains in scrollback after leaving the alternate screen.
            let limit = (height / 4).max(1);
            if lines.len() > limit {
                lines.truncate(limit);
                lines[limit - 1] = lines[limit - 1]
                    .clone()
                    .cell(content_width.saturating_sub(1))
                    .plain("…");
            }
            lines.push(Line::default());
        }
        if results.rows.is_empty() {
            lines.push(Line::new("No checks yet. Run super check to start."));
            lines.push(self.close_hint(results));
        } else if width < 60 || height < 20 {
            lines.push(Line::default().muted("Resize to at least 60×20 to browse."));
            if let Some(check) = self.selected(results) {
                lines.push(presentation::result_line(check, Some(self.tick)));
                lines.push(presentation::check_identity(check, results, true));
                lines.push(Line::default().muted(presentation::timing(check)));
            }
            lines.push(self.close_hint(results));
        } else {
            self.header(results, reconnecting, &mut lines);
            let wrapped = wrap(lines.clone(), content_width);
            self.header_clipped = wrapped.len() > height / 3;
            // An exceptionally long name/path must not consume the controls or result
            // viewport. Its complete identity remains scrollable inside the panel.
            lines = if self.header_clipped {
                lines.into_iter().map(|line| line.cell(content_width)).collect()
            } else {
                wrapped
            };
            if self.focused(results) {
                self.attempts(
                    results,
                    height.saturating_sub(lines.len() + 8).max(1) / 2,
                    width,
                    &mut lines,
                );
            } else if results.rows.len() > 1 {
                self.checks(
                    results,
                    height.saturating_sub(lines.len() + 8).max(2) / 2,
                    width,
                    &mut lines,
                );
            }
            self.result_panel(results, width, height, reconnecting, &mut lines);
        }
        // Include every row so the renderer can erase space vacated by a shrinking panel.
        lines.resize(height, Line::default());
        lines.iter().map(|line| line.render(width, 2)).collect()
    }

    fn header(&self, results: &Results, reconnecting: bool, lines: &mut Vec<Line>) {
        if self.focused(results) && results.attempts(self.row).len() > 1 {
            if let Some(row) = results.rows.get(self.row) {
                lines.push(Line::default().bold(&row.supertest.name));
            }
        } else if self.mode == Mode::Inspect || results.rows.len() == 1 {
            if let Some(check) = self.selected(results) {
                lines.push(presentation::result_line(check, Some(self.tick)));
                lines.push(presentation::check_identity(check, results, true));
            } else if let Some(row) = results.rows.get(self.row) {
                lines.push(Line::default().bold(&row.supertest.name));
                lines.push(Line::default().muted(presentation::location(&row.supertest)));
            }
        } else {
            lines.push(presentation::context(results));
            lines.push(presentation::summary(results));
        }
        if reconnecting {
            lines.push(Line::default().push("Reconnecting · results may be out of date", Ink::Warning, false));
        }
        if self.mode == Mode::Inspect || results.rows.len() == 1 {
            if results.history_loading {
                lines.push(Line::default().muted("Loading history…"));
            } else if results.history_error.is_some() {
                lines.push(Line::default().push("History unavailable", Ink::Warning, false));
            }
        }
        lines.push(Line::default());
    }

    fn checks(&mut self, results: &Results, capacity: usize, width: usize, lines: &mut Vec<Line>) {
        let columns = Columns::new(results, width);
        lines.push(columns.line(
            Line::default(),
            Line::default().muted("SUPERTEST"),
            Line::default().muted("STATUS"),
            Line::default().muted("PREVIOUS"),
        ));
        let range = window(&mut self.list_start, self.row, results.rows.len(), capacity.max(2));
        for index in range.clone() {
            let row = &results.rows[index];
            let status = row.current.as_ref().map(presentation::status);
            let marker = if index == self.row && row.current.as_ref().is_some_and(|check| !check.terminal) {
                TICKS[self.tick % TICKS.len()]
            } else {
                status.as_ref().map_or("·", |status| status.marker)
            };
            let ink = status.as_ref().map_or(Ink::Muted, |status| status.ink);
            let previous = row.previous.as_ref().map_or_else(
                || {
                    Line::default().muted(if results.history_incomplete() {
                        "not loaded"
                    } else {
                        "none"
                    })
                },
                |check| {
                    let status = presentation::status(check);
                    status.line().muted(format!(" {}", check.revision.short_identity()))
                },
            );
            let mut line = columns.line(
                Line::default()
                    .push(if index == self.row { "› " } else { "  " }, Ink::Accent, false)
                    .push(marker, ink, false),
                Line::new(&row.supertest.name),
                row.current.as_ref().map_or_else(
                    || {
                        Line::default().muted(if results.history_incomplete() {
                            "not loaded"
                        } else {
                            "not checked"
                        })
                    },
                    presentation::activity_line,
                ),
                previous,
            );
            if index == self.row {
                line = line.selected();
            }
            lines.push(line);
        }
        if range.len() < results.rows.len() {
            lines.push(Line::default().muted(format!("Check {} of {}", self.row + 1, results.rows.len())));
        }
        lines.push(Line::default());
    }

    fn attempts(&mut self, results: &Results, capacity: usize, width: usize, lines: &mut Vec<Line>) {
        let attempts = results.attempts(self.row);
        if attempts.len() <= 1 {
            return;
        }
        let selected = attempts
            .iter()
            .position(|check| Some(check.number) == self.attempt)
            .unwrap_or(0);
        let columns = presentation::AttemptColumns::new(&attempts, width.saturating_sub(6));
        lines.push(Line::default().bold("Check history"));
        lines.push(Line::new("  ").append(columns.header()));
        let range = window(
            &mut self.history_start,
            selected,
            attempts.len(),
            capacity.saturating_sub(2).max(1),
        );
        for index in range.clone() {
            let check = attempts[index];
            let mut line = Line::default()
                .push(if index == selected { "› " } else { "  " }, Ink::Accent, false)
                .append(columns.row(check))
                .cell(width.saturating_sub(4));
            if index == selected {
                line = line.selected();
            }
            lines.push(line);
        }
        if range.len() < attempts.len() {
            lines.push(Line::default().muted(format!("Attempt {} of {}", selected + 1, attempts.len())));
        }
        lines.push(Line::default());
    }

    fn result_panel(
        &mut self,
        results: &Results,
        width: usize,
        height: usize,
        reconnecting: bool,
        lines: &mut Vec<Line>,
    ) {
        let mut body = Vec::new();
        if let Some(check) = self.selected(results) {
            if self.header_clipped {
                body.push(presentation::result_line(check, None));
                body.push(presentation::check_identity(check, results, true));
                body.push(Line::default());
            } else if self.mode == Mode::List && results.rows.len() > 1 {
                // Full name is accessible even when its table cell has to truncate.
                let table_name_width = Columns::new(results, width).name;
                if console::measure_text_width(&check.supertest.name) > table_name_width {
                    body.push(Line::default().bold(&check.supertest.name));
                }
                let mixed_sources = results
                    .rows
                    .iter()
                    .filter_map(|row| row.current.as_ref())
                    .any(|other| presentation::source(other, results) != presentation::source(check, results));
                body.push(presentation::check_identity(check, results, mixed_sources));
                body.push(Line::default());
            } else if self.focused(results) && results.attempts(self.row).len() > 1 {
                let mut identity = Line::default().muted(presentation::location(&check.supertest));
                let source = presentation::source(check, results);
                if source != check.revision.short_identity() {
                    identity = identity.muted(format!(" · {source}"));
                }
                body.push(identity);
                body.push(Line::default());
            }
            body.extend(presentation::evidence_with_tick(
                check,
                if self.details || self.mode == Mode::Inspect {
                    presentation::EvidenceView::Full
                } else {
                    presentation::EvidenceView::Selected
                },
                reconnecting,
                Some(self.tick),
            ));
        } else {
            body.push(Line::new(if results.history_incomplete() {
                "No check loaded at the current commit."
            } else {
                "Not checked at the current commit."
            }));
        }
        presentation::trim_padding(&mut body);
        // The outer panel provides indentation; retain internal/multiline evidence whitespace.
        for line in &mut body {
            if let Some((text, _, _)) = line.parts.first_mut()
                && let Some(stripped) = text.strip_prefix("  ")
            {
                *text = stripped.to_owned();
            }
        }
        let body = wrap(body, width.saturating_sub(6).max(1));
        self.content_rows = body.len();
        let mut footer = self.controls(results, false, width.saturating_sub(4));
        if body.is_empty() {
            self.scroll = 0;
            self.page = 1;
            lines.extend(footer);
            return;
        }
        let mut capacity = height.saturating_sub(lines.len() + footer.len() + 3).max(1);
        if body.len() > capacity {
            footer = self.controls(results, true, width.saturating_sub(4));
            capacity = height.saturating_sub(lines.len() + footer.len() + 3).max(1);
        }
        self.page = body.len().min(capacity).max(1);
        self.scroll = self.scroll.min(body.len().saturating_sub(self.page));
        lines.extend(panel(
            body.into_iter().skip(self.scroll).take(self.page).collect(),
            self.page + 2,
            width,
        ));
        lines.push(Line::default());
        lines.extend(footer);
    }

    fn controls(&self, results: &Results, scrolling: bool, width: usize) -> Vec<Line> {
        let mut hints = Vec::new();
        if self.focused(results) && results.attempts(self.row).len() > 1 {
            hints.push("↑/↓ attempts");
        } else if self.mode == Mode::List && results.rows.len() > 1 {
            hints.push("↑/↓ select");
        } else if scrolling {
            hints.push("↑/↓ scroll");
        }
        if self.can_inspect(results) {
            hints.push("Enter for more details");
        }
        if scrolling {
            hints.push("PgUp/PgDn scroll");
        }
        if self.can_go_back {
            hints.push("Esc back");
        }
        if let Some(action) = self.history_action(results) {
            hints.push(action);
        }
        hints.push(self.close_action(results));
        // Wrap between shortcuts so a key and its action stay together.
        let mut lines = Vec::new();
        let mut line = Line::default();
        for hint in hints {
            if line.width() > 0 {
                if line.width() + 3 + console::measure_text_width(hint) > width {
                    lines.push(line);
                    line = Line::default();
                } else {
                    line = line.push(" · ", Ink::Accent, false);
                }
            }
            line = line.push(hint, Ink::Accent, false);
        }
        lines.push(line);
        lines
    }

    fn close_action(&self, results: &Results) -> &'static str {
        match (self.can_go_back, results.finished()) {
            (true, true) => "Ctrl+C close",
            (true, false) => "Ctrl+C detach",
            (false, true) => "Esc/Ctrl+C close",
            (false, false) => "Esc/Ctrl+C detach",
        }
    }

    fn close_hint(&self, results: &Results) -> Line {
        let mut line = Line::default();
        if self.can_go_back {
            line = line.muted("Esc back · ");
        }
        line = line.muted(self.close_action(results));
        if !results.finished() {
            line = line.muted(" · checks continue remotely");
        }
        line
    }
}

fn wrap(lines: Vec<Line>, width: usize) -> Vec<Line> {
    lines.into_iter().flat_map(|line| line.wrapped(width.max(1))).collect()
}

fn window(start: &mut usize, selected: usize, count: usize, capacity: usize) -> std::ops::Range<usize> {
    let capacity = capacity.min(count);
    *start = (*start).min(selected).min(count.saturating_sub(capacity));
    if selected >= *start + capacity {
        *start = selected.saturating_sub(capacity.saturating_sub(1));
    }
    *start..(*start + capacity).min(count)
}

struct Columns {
    name: usize,
    status: usize,
    previous: usize,
}

impl Columns {
    fn new(results: &Results, width: usize) -> Self {
        // Live activity may truncate, but reserve enough room for a reported result
        // and its operational error. Measure all rows so selection cannot shift columns.
        let status = results
            .rows
            .iter()
            .filter_map(|row| row.current.as_ref())
            .map(|check| presentation::status(check).line().width())
            .max()
            .unwrap_or(0)
            .max(20);
        let previous = if width >= 100 && results.rows.iter().any(|row| row.previous.is_some()) {
            results
                .rows
                .iter()
                .filter_map(|row| row.previous.as_ref())
                .map(|check| presentation::status(check).line().width() + 8)
                .max()
                .unwrap_or(0)
                .max(26)
        } else {
            0
        };
        let available = width.saturating_sub(4 + 4 + 3 + status + if previous > 0 { 3 + previous } else { 0 });
        let desired = results
            .rows
            .iter()
            .map(|row| console::measure_text_width(&row.supertest.name))
            .max()
            .unwrap_or(0);
        Self {
            name: desired.max(9).min(available).max(1),
            status,
            previous,
        }
    }

    fn line(&self, prefix: Line, name: Line, status: Line, previous: Line) -> Line {
        let mut line = prefix
            .cell(4)
            .append(name.cell(self.name))
            .muted(" · ")
            .append(status.cell(self.status));
        if self.previous > 0 {
            line = line.muted(" · ").append(previous.cell(self.previous));
        }
        line
    }
}
