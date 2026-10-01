use super::{
    Check, Results, age, elapsed,
    style::{Ink, Line},
};
use chrono::Utc;
use super_types::{CheckAssurance, CheckOperationalError, CheckOutcome};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Pass,
    Fail,
    Conditional,
    Pending,
    Queued,
    Checking,
    Blocked,
    Canceled,
    Error,
    Unavailable,
}

fn kind(check: &Check) -> Kind {
    use super_types::usage::CheckStatus;
    if check.terminal && check.result.is_none() && check.operational_error.is_none() {
        return Kind::Unavailable;
    }
    match CheckStatus::from_check(check) {
        CheckStatus::Pass => Kind::Pass,
        CheckStatus::Fail => Kind::Fail,
        CheckStatus::Conditional => Kind::Conditional,
        CheckStatus::Blocked => Kind::Blocked,
        CheckStatus::Canceled => Kind::Canceled,
        CheckStatus::Error => Kind::Error,
        CheckStatus::Queued if check.presentation.status.label == "pending" => Kind::Pending,
        CheckStatus::Queued => Kind::Queued,
        CheckStatus::Checking => Kind::Checking,
    }
}

fn is_certified(check: &Check) -> bool {
    check
        .result
        .as_ref()
        .is_some_and(|result| result.assurance == CheckAssurance::Certified)
}

pub(super) struct Status {
    pub label: String,
    pub marker: &'static str,
    pub ink: Ink,
    pub interruption: Option<&'static str>,
}

impl Status {
    pub fn line(&self) -> Line {
        let mut line = Line::default().push(&self.label, self.ink, false);
        if let Some(interruption) = self.interruption {
            line = line.muted(" · ").push(interruption, Ink::Warning, false);
        }
        line
    }
}

pub(super) fn status(check: &Check) -> Status {
    let (label, marker, ink) = match kind(check) {
        Kind::Pass if is_certified(check) => ("pass (certified)", "✓", Ink::Pass),
        Kind::Pass => ("pass", "✓", Ink::Pass),
        Kind::Fail if is_certified(check) => ("fail (certified)", "×", Ink::Fail),
        Kind::Fail => ("fail", "×", Ink::Fail),
        Kind::Conditional => ("conditional", "?", Ink::Warning),
        Kind::Pending => ("pending", "○", Ink::Muted),
        Kind::Queued => ("queued", "○", Ink::Muted),
        Kind::Checking => ("checking", "●", Ink::Accent),
        Kind::Blocked => ("blocked", "!", Ink::Warning),
        Kind::Canceled => ("canceled", "○", Ink::Muted),
        Kind::Error => ("error", "!", Ink::Fail),
        Kind::Unavailable => ("result unavailable", "?", Ink::Muted),
    };
    Status {
        label: label.into(),
        marker,
        ink,
        interruption: check
            .result
            .as_ref()
            .and(check.operational_error.as_ref())
            .map(|error| match error {
                CheckOperationalError::Blocked => "blocked",
                CheckOperationalError::Canceled => "canceled",
                CheckOperationalError::Error | CheckOperationalError::MissingConclusion => "error",
            }),
    }
}

pub(super) fn activity(check: &Check) -> String {
    let status = status(check);
    if kind(check) == Kind::Checking {
        if let Some(problem) = &check.presentation.active_problem_label {
            return format!("checking · {problem}");
        }
        if let Some(label) = check.presentation.activity_label.as_deref()
            && !matches!(label, "queued" | "checking" | "")
        {
            return format!("checking · {label}");
        }
    }
    status.line().text()
}

pub(super) fn activity_line(check: &Check) -> Line {
    let status = status(check);
    if kind(check) == Kind::Checking {
        Line::default().push(activity(check), status.ink, false)
    } else {
        status.line()
    }
}

pub(super) fn activity_sentence(text: &str, tick: Option<usize>) -> Line {
    let (prefix, rest) = text.split_at(text.find(char::is_whitespace).unwrap_or(text.len()));
    let verb = prefix.strip_suffix("...").or_else(|| prefix.strip_suffix('…'));
    if let Some(verb) = verb.filter(|word| !word.is_empty() && word.chars().all(char::is_alphabetic)) {
        // Animate only the display prefix. Three columns keep the supplied sentence still,
        // including when the API uses a single Unicode ellipsis instead of three dots.
        let prefix = tick.map_or_else(
            || prefix.to_owned(),
            |tick| format!("{verb}{:<3}", ".".repeat((tick / 5) % 3 + 1)),
        );
        Line::default()
            .push(prefix, Ink::Activity, true)
            .push(rest, Ink::Activity, false)
    } else {
        Line::default().push(text, Ink::Activity, false)
    }
}

pub(super) fn is_pass(check: &Check) -> bool {
    check
        .result
        .as_ref()
        .is_some_and(|result| result.outcome == CheckOutcome::Pass)
}

fn certified_pass_suffix(results: &Results) -> String {
    let count = results
        .rows
        .iter()
        .filter_map(|row| row.current.as_ref())
        .filter(|check| is_pass(check) && is_certified(check))
        .count();
    if count == 0 {
        String::new()
    } else {
        format!(" ({count} certified)")
    }
}

pub(super) fn summary(results: &Results) -> Line {
    let mut checking = 0;
    let mut queued = 0;
    let mut passed = 0;
    let mut failed = 0;
    let mut other = std::collections::BTreeMap::new();
    for row in &results.rows {
        if let Some(check) = &row.current {
            match kind(check) {
                Kind::Fail => failed += 1,
                Kind::Pass => passed += 1,
                Kind::Checking => checking += 1,
                Kind::Queued | Kind::Pending => queued += 1,
                Kind::Conditional => *other.entry("conditional").or_insert(0) += 1,
                Kind::Blocked => *other.entry("blocked").or_insert(0) += 1,
                Kind::Canceled => *other.entry("canceled").or_insert(0) += 1,
                Kind::Error => *other.entry("error").or_insert(0) += 1,
                Kind::Unavailable => *other.entry("result unavailable").or_insert(0) += 1,
            }
        } else {
            *other
                .entry(if results.history_incomplete() {
                    "not loaded"
                } else {
                    "not checked"
                })
                .or_insert(0) += 1;
        }
    }
    let count = results.rows.len();
    let mut line = Line::default().muted(format!("{count} supertest{}", if count == 1 { "" } else { "s" }));
    for (count, name, ink) in [
        (checking, "in progress", Ink::Muted),
        (queued, "queued", Ink::Muted),
        (passed, "passed", Ink::Pass),
        (failed, "failed", Ink::Fail),
    ] {
        if count > 0 {
            line = line.muted(" · ").push(format!("{count} {name}"), ink, false);
            if name == "passed" {
                line = line.push(certified_pass_suffix(results), ink, false);
            }
        }
    }
    for (name, count) in other {
        line = line.muted(format!(" · {count} {name}"));
    }
    let interrupted = results
        .rows
        .iter()
        .filter_map(|row| row.current.as_ref())
        .filter(|check| check.result.is_some() && check.operational_error.is_some())
        .count();
    if interrupted > 0 {
        line = line.push(format!(" · {interrupted} with operational errors"), Ink::Warning, false);
    }
    let found = results
        .rows
        .iter()
        .filter(|row| {
            row.current
                .as_ref()
                .is_some_and(|check| check.problematic && kind(check) != Kind::Fail)
        })
        .count();
    if found > 0 {
        line = line.push(format!(" · {found} with reported problems"), Ink::Fail, false);
    }
    line
}

pub(super) fn explanation(check: &Check, disconnected: bool) -> String {
    let conclusion = || {
        check
            .presentation
            .details
            .iter()
            .find(|line| !line.emphasized && !line.text.trim().is_empty())
            .map(|line| line.text.trim().to_owned())
    };
    if (check.terminal || check.problematic)
        && let Some(conclusion) = conclusion()
    {
        return conclusion;
    }
    if !check.terminal
        && let Some(line) = &check.presentation.live_line
    {
        return if disconnected {
            format!("Last reported: {line}")
        } else {
            line.clone()
        };
    }
    conclusion().unwrap_or_else(|| match kind(check) {
        Kind::Queued | Kind::Pending => "Waiting to start. No analysis has been reported yet.".into(),
        Kind::Checking => "No detailed activity has been reported yet.".into(),
        Kind::Pass if is_certified(check) => "A certificate was accepted for this result.".into(),
        Kind::Pass => "No problems found within this supertest.".into(),
        Kind::Blocked => "The check could not finish. No further reason was reported.".into(),
        Kind::Error => "The check ended with an operational error.".into(),
        Kind::Canceled => "This check was canceled.".into(),
        Kind::Fail | Kind::Conditional | Kind::Unavailable => "No further evidence was reported.".into(),
    })
}

pub(super) fn timing(check: &Check) -> String {
    let duration = elapsed(
        check.created_at,
        if check.terminal { check.updated_at } else { Utc::now() },
    );
    if check.terminal {
        return format!("Requested {}", age(check.created_at));
    }
    format!(
        "Elapsed {duration} · {}",
        check.presentation.activity_updated_at.map_or_else(
            || "no activity detail available".into(),
            |updated| format!("last update {}", age(updated))
        )
    )
}

pub(super) fn fix_command(check: &Check) -> Line {
    Line::default()
        .muted("Review and apply: ")
        .push(format!("super fix --check {}", check.number), Ink::Accent, true)
        .soft_wrap()
}

pub(super) fn full_details(check: &Check) -> Vec<Line> {
    let mut lines = Vec::new();
    let narrative = super::compact::narrative_layout(check);
    for (index, detail) in check.presentation.details.iter().enumerate() {
        if let Some(narrative) = &narrative {
            if index + 1 == narrative.headline {
                // The saved headline is the heading; a generic Conclusion label adds nothing.
                continue;
            }
            if index == narrative.headline {
                lines.push(Line::default().push(
                    detail.text.strip_prefix("  ").unwrap_or(&detail.text),
                    Ink::Plain,
                    narrative.explanation.is_some(),
                ));
                continue;
            }
            if narrative.explanation == Some(index) {
                lines.push(Line::default());
            }
        }
        // split(), not lines(): preserve embedded empty paragraphs and trailing newlines.
        for text in detail.text.split('\n') {
            lines.push(Line::default().push(text, Ink::Plain, detail.emphasized));
        }
    }
    trim_padding(&mut lines);
    lines
}

pub(super) fn trim_padding(lines: &mut Vec<Line>) {
    let start = lines
        .iter()
        .position(|line| !line.text().trim().is_empty())
        .unwrap_or(lines.len());
    lines.drain(..start);
    while lines.last().is_some_and(|line| line.text().trim().is_empty()) {
        lines.pop();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EvidenceView {
    Summary,
    Selected,
    Full,
}

impl EvidenceView {
    pub fn snapshot(details: bool) -> Self {
        if details { Self::Full } else { Self::Summary }
    }
}

pub(super) fn fix_pending(check: &Check) -> bool {
    check.problematic
        && check.fix.is_none()
        && check.fix_pending
        && check.updates_pending
        && check.operational_error.is_none()
}

pub(super) fn fix_notice(check: &Check, disconnected: bool) -> Option<Line> {
    if check.fix.is_some() {
        Some(fix_command(check))
    } else if !disconnected && fix_pending(check) {
        Some(Line::default().muted("A fix proposal may still arrive."))
    } else if !disconnected && check.problematic && check.terminal && (!check.fix_pending || !check.updates_pending) {
        Some(Line::default().muted("No fix proposal available."))
    } else {
        None
    }
}

pub(super) fn evidence(check: &Check, view: EvidenceView, disconnected: bool) -> Vec<Line> {
    evidence_with_tick(check, view, disconnected, None)
}

pub(super) fn evidence_with_tick(
    check: &Check,
    view: EvidenceView,
    disconnected: bool,
    tick: Option<usize>,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut result = match view {
        EvidenceView::Full => full_details(check),
        // Selected passes retain their headline; run summaries omit it.
        EvidenceView::Summary if is_pass(check) && !check.problematic => Vec::new(),
        EvidenceView::Summary | EvidenceView::Selected => super::compact::details(check),
    };
    if !check.terminal {
        // Live activity and saved findings are separate API fields. Do not reuse a
        // finding headline as activity when the evidence below already contains it.
        if let Some(live) = &check.presentation.live_line {
            let prefix = if disconnected {
                Line::default().push("Last reported: ", Ink::Activity, false)
            } else {
                Line::default()
            };
            lines.push(prefix.append(activity_sentence(live, tick.filter(|_| !disconnected))));
        } else if result.is_empty() {
            lines.push(Line::default().push(explanation(check, disconnected), Ink::Activity, false));
        }
        lines.push(Line::default().muted(timing(check)));
    }
    if check.terminal && view != EvidenceView::Summary && result.iter().all(|line| line.text().trim().is_empty()) {
        result.push(Line::new(explanation(check, false)));
    }
    if !result.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(result);
    }
    if check.result.is_some()
        && let Some(error) = &check.operational_error
    {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::default().push(
            match error {
                super_types::CheckOperationalError::Blocked => {
                    "The check was blocked; the reported finding remains available."
                }
                super_types::CheckOperationalError::Canceled => {
                    "The check was canceled; the reported result remains available."
                }
                super_types::CheckOperationalError::Error | super_types::CheckOperationalError::MissingConclusion => {
                    "The check ended with an operational error; the reported result remains available."
                }
            },
            Ink::Warning,
            false,
        ));
    }
    if let Some(notice) = fix_notice(check, disconnected) {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        if view == EvidenceView::Full
            && let Some(fix) = &check.fix
        {
            lines.push(Line::default().bold("Proposed fix"));
            lines.push(Line::new(&fix.summary));
        }
        lines.push(notice);
    }
    lines
}

pub(super) fn result_line(check: &Check, tick: Option<usize>) -> Line {
    let status = status(check);
    Line::default()
        .push(
            format!(
                "{} ",
                tick.filter(|_| !check.terminal)
                    .map_or(status.marker, |tick| super::TICKS[tick % super::TICKS.len()])
            ),
            status.ink,
            false,
        )
        .append(status.line())
        .plain(" · ")
        .bold(&check.supertest.name)
}

pub(super) fn simple_cancellation(check: &Check) -> bool {
    crate::cancel::canceled(check)
        && !crate::cancel::active(check)
        && check.result.is_none()
        && !check.problematic
        && check.fix.is_none()
        && check
            .presentation
            .details
            .iter()
            .all(|line| line.text.trim().is_empty())
}

pub(super) fn canceled_line(check: &Check) -> Line {
    result_line(check, None).muted(format!(" · #{}", check.number))
}

pub(super) fn location(supertest: &super_types::Supertest) -> String {
    supertest
        .line
        .map_or_else(|| supertest.path.clone(), |line| format!("{}:{line}", supertest.path))
}

pub(super) fn source(check: &Check, results: &Results) -> String {
    if let Some(head) = &results.head
        && head.temporary
        && check.revision.reported_git_commit.as_deref() == Some(&head.oid)
    {
        let parent = head.parent_oid.as_deref().unwrap_or("unknown");
        return format!("uncommitted changes · based on {}", &parent[..parent.len().min(7)]);
    }
    if let Some(parent) = check
        .revision
        .reported_git_commit
        .as_ref()
        .and_then(|oid| results.temporary_commit_parents.get(oid))
    {
        return format!("uncommitted changes · based on {}", &parent[..parent.len().min(7)]);
    }
    check.revision.short_identity().into()
}

pub(super) fn context(results: &Results) -> Line {
    let sources: std::collections::BTreeSet<_> = results
        .rows
        .iter()
        .filter_map(|row| row.current.as_ref())
        .map(|check| source(check, results))
        .collect();
    let mut line = Line::default().bold(&results.repository);
    if !sources.is_empty() {
        line = line.muted(format!(" · {}", sources.into_iter().collect::<Vec<_>>().join(", ")));
    }
    line
}

pub(super) fn check_identity(check: &Check, results: &Results, include_source: bool) -> Line {
    let mut line = Line::default().muted(location(&check.supertest));
    if include_source {
        line = line.muted(format!(" · {}", source(check, results)));
    }
    line.muted(format!(" · #{}", check.number))
}

pub(super) fn divider(width: usize) -> Vec<Line> {
    vec![
        Line::default(),
        Line::default().muted("─".repeat(width.saturating_sub(4))),
        Line::default(),
    ]
}

fn check_block(check: &Check, results: &Results, details: bool, include_source: bool) -> Vec<Line> {
    if !details && simple_cancellation(check) {
        return vec![canceled_line(check)];
    }
    let mut lines = vec![result_line(check, None), check_identity(check, results, include_source)];
    let body = evidence(check, EvidenceView::snapshot(details), false);
    if !body.is_empty() {
        lines.push(Line::default());
        lines.extend(body);
    }
    lines
}

fn needs_attention(check: &Check) -> bool {
    !check.terminal || !is_pass(check) || check.problematic || check.operational_error.is_some() || check.fix.is_some()
}

pub(super) fn snapshot(results: &Results, include_history: bool, details: bool, width: usize) -> Vec<Line> {
    if results.rows.is_empty() {
        return vec![Line::new("No checks yet. Run super check to start.")];
    }
    if include_history {
        return history_snapshot(results, details, width);
    }
    let single = results.rows.len() == 1;
    if !details
        && single
        && let Some(check) = &results.rows[0].current
        && simple_cancellation(check)
    {
        return vec![canceled_line(check), Line::default(), results.recheck(check)];
    }
    let mut lines = if single {
        Vec::new()
    } else {
        vec![context(results), summary(results)]
    };
    if !single
        && !details
        && results
            .rows
            .iter()
            .all(|row| row.current.as_ref().is_some_and(|check| !needs_attention(check)))
    {
        return vec![
            Line::default()
                .push(
                    format!(
                        "✓ {} supertests passed{}",
                        results.rows.len(),
                        certified_pass_suffix(results)
                    ),
                    Ink::Pass,
                    true,
                )
                .muted(" · ")
                .append(context(results)),
        ];
    }
    let mut blocks = 0;
    for row in &results.rows {
        if !single && !details && row.current.as_ref().is_some_and(|check| !needs_attention(check)) {
            continue;
        }
        if blocks > 0 {
            lines.extend(divider(width));
        } else if !lines.is_empty() {
            lines.push(Line::default());
        }
        if let Some(check) = &row.current {
            lines.extend(check_block(check, results, details, single));
        } else {
            lines.push(Line::default().bold(&row.supertest.name));
            lines.push(Line::default().muted(location(&row.supertest)));
            lines.push(Line::new(if results.history_incomplete() {
                "No check loaded at the current commit."
            } else {
                "Not checked at the current commit."
            }));
        }
        blocks += 1;
    }
    if results.fixes_pending() {
        lines.push(Line::default());
        lines.push(
            Line::default()
                .muted("Watch: ")
                .push(
                    format!(
                        "{} --watch{}",
                        results.status_command(),
                        if details { " --details" } else { "" }
                    ),
                    Ink::Accent,
                    true,
                )
                .soft_wrap(),
        );
    } else if !details
        && results
            .rows
            .iter()
            .filter_map(|row| row.current.as_ref())
            .any(|check| check.terminal && check.fix.is_none() && needs_attention(check) && !simple_cancellation(check))
    {
        lines.push(Line::default());
        lines.push(
            Line::default()
                .muted("Details: ")
                .push(format!("{} --details", results.status_command()), Ink::Accent, true)
                .soft_wrap(),
        );
    }
    if let Some(before) = results.next_before {
        lines.push(Line::default());
        lines.push(more_history(results, before));
    }
    lines
}

pub(super) struct AttemptColumns {
    status: usize,
    number: usize,
    requested: usize,
}

impl AttemptColumns {
    pub fn new(attempts: &[&Check], width: usize) -> Self {
        let number = attempts
            .iter()
            .map(|check| format!("#{}", check.number).len())
            .max()
            .unwrap_or(0)
            .max(5);
        let requested = attempts
            .iter()
            .map(|check| age(check.created_at).len())
            .max()
            .unwrap_or(0)
            .max("REQUESTED".len());
        let status = attempts
            .iter()
            .map(|check| 2 + status(check).line().width())
            .max()
            .unwrap_or(0)
            .max(6);
        Self {
            // Reserve commit, check number, request time, and the three separators before
            // sizing status. Use all loaded attempts so selection and paging stay aligned.
            status: status.min(width.saturating_sub(7 + number + requested + 9)).max(1),
            number,
            requested,
        }
    }

    pub fn header(&self) -> Line {
        self.line(
            Line::default().muted("STATUS"),
            Line::default().muted("COMMIT"),
            Line::default().muted("CHECK"),
            Line::default().muted("REQUESTED"),
        )
    }

    pub fn row(&self, check: &Check) -> Line {
        let status = status(check);
        self.line(
            Line::default()
                .push(format!("{} ", status.marker), status.ink, false)
                .append(status.line()),
            Line::default().muted(
                check
                    .revision
                    .reported_git_commit
                    .as_ref()
                    .filter(|commit| !commit.is_empty())
                    .map_or("—", |_| check.revision.short_identity()),
            ),
            Line::default().muted(format!("#{}", check.number)),
            Line::default().muted(age(check.created_at)),
        )
    }

    fn line(&self, status: Line, commit: Line, number: Line, requested: Line) -> Line {
        status
            .cell(self.status)
            .muted(" · ")
            .append(commit.cell(7))
            .muted(" · ")
            .append(number.cell(self.number))
            .muted(" · ")
            .append(requested.cell(self.requested))
    }
}

fn more_history(results: &Results, before: u64) -> Line {
    Line::default()
        .muted("More history: ")
        .push(
            format!("{} --history --before {before}", results.status_command()),
            Ink::Accent,
            true,
        )
        .soft_wrap()
}

fn history_snapshot(results: &Results, details: bool, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    for (index, row) in results.rows.iter().enumerate() {
        if index > 0 {
            lines.extend(divider(width));
        }
        lines.push(Line::default().bold(&row.supertest.name).muted(" · history"));
        lines.push(Line::default().muted(location(&row.supertest)));
        let attempts = results.attempts(index);
        let current = row.current.as_ref().map(|check| check.number);
        let columns = AttemptColumns::new(&attempts, width.saturating_sub(4));
        if !attempts.is_empty() {
            lines.push(Line::default());
            lines.push(columns.header());
        }
        for check in &attempts {
            lines.push(Line::default());
            lines.push(columns.row(check));
            let source = source(check, results);
            if source != check.revision.short_identity() {
                lines.push(Line::default().muted(source));
            }
            if details {
                lines.extend(evidence(check, EvidenceView::Full, false));
            }
        }
        if !attempts.iter().any(|check| Some(check.number) != current) {
            lines.push(Line::default().muted(if results.next_before.is_some() {
                "No earlier checks in this page."
            } else {
                "No earlier checks."
            }));
        }
    }
    if let Some(before) = results.next_before {
        lines.push(Line::default());
        lines.push(more_history(results, before));
    }
    lines
}
