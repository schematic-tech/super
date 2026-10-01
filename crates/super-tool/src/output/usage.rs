use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use super_types::usage::{CheckStatus, Period, TokenQuota, UsageQuery, UsageReport};

use super::{
    Ui,
    style::{Ink, Line},
};

#[cfg(test)]
mod tests;

impl Ui {
    pub fn usage_report(report: &UsageReport, query: &UsageQuery, timezone: Tz) {
        Self::print_lines(&report_lines(report, query, timezone, Self::width().saturating_sub(4)));
    }
}

fn report_lines(report: &UsageReport, query: &UsageQuery, timezone: Tz, width: usize) -> Vec<Line> {
    let mut lines = vec![
        Line::default()
            .push("Token usage", Ink::Accent, true)
            .muted(" · ")
            .plain(timestamp(report.as_of, timezone))
            .muted(" · ")
            .plain(timezone.name().replace('_', " ")),
        Line::default(),
    ];
    lines.push(
        Line::default()
            .bold("Weekly quota")
            .muted(" · all repositories · rolling 7 days"),
    );
    quota_lines(&mut lines, report.quota.as_ref(), timezone);
    let period = match query.period {
        Period::Today => "Today",
        Period::Week => "Last 7 days",
        Period::Month => "Last 30 days",
    };
    let repository = query
        .repository_id
        .and_then(|id| report.repositories.iter().find(|repository| repository.id == id));
    let scope = repository.map_or_else(
        || {
            if query.repository_id.is_some() {
                "Selected repository"
            } else {
                "All repositories"
            }
        },
        |repository| repository.name.as_str(),
    );
    lines.extend([Line::default(), Line::default().bold(period).muted(" · ").bold(scope)]);
    let tokens = if report.usage_available {
        report.total_tokens
    } else {
        None
    };
    let total = tokens.map_or_else(
        || Line::default().muted("Unavailable"),
        |value| Line::default().bold(compact(value)),
    );
    lines.extend(table(
        &["TOKENS USED", "SUPERTESTS RUN"],
        vec![vec![
            total,
            Line::default().bold(compact(u64::from(report.total_checks))),
        ]],
        width,
        0,
        false,
    ));
    if !report.usage_available || report.total_tokens.is_none() {
        lines.push(Line::default().muted("Some usage measurements are unavailable. Totals may be incomplete."));
    }
    lines.extend([Line::default(), Line::default().bold("Recent checks"), Line::default()]);
    lines.extend(recent_checks(report, query.repository_id.is_none(), width));
    let visible_pending = report
        .checks
        .iter()
        .any(|check| check.usage_pending && check.period_tokens.is_some());
    if report.pending || visible_pending {
        let note = if visible_pending {
            Line::default().push("+ ", Ink::Accent, true)
        } else {
            Line::default()
        };
        lines.push(note.muted("Usage is still being reported"));
    }
    lines
}

fn recent_checks(report: &UsageReport, all_repositories: bool, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    if report.checks.is_empty() {
        lines.push(Line::default().muted("No checks in this period."));
    } else {
        let mut headers = vec!["SUPERTEST"];
        if all_repositories {
            headers.push("REPOSITORY");
        }
        headers.extend(["STATUS", "TOKENS"]);
        let rows = report
            .checks
            .iter()
            .map(|check| {
                let name = if report
                    .checks
                    .iter()
                    .any(|other| other.name == check.name && other.path != check.path)
                {
                    format!("{}::{}", check.path, check.name)
                } else {
                    check.name.clone()
                };
                let mut row = vec![Line::new(name)];
                if all_repositories {
                    row.push(
                        Line::default().muted(
                            report
                                .repositories
                                .iter()
                                .find(|repository| repository.id == check.repository_id)
                                .map_or("Unknown repository", |repository| repository.name.as_str()),
                        ),
                    );
                }
                let (label, ink) = status(check.status);
                row.push(Line::default().push(label, ink, false));
                let mut tokens = amount(check.period_tokens);
                tokens = tokens.push(
                    if check.usage_pending && check.period_tokens.is_some() {
                        "+"
                    } else {
                        " "
                    },
                    Ink::Accent,
                    false,
                );
                row.push(tokens);
                row
            })
            .collect();
        lines.extend(table(&headers, rows, width, headers.len() - 1, true));
        lines.push(Line::default());
        lines.push(Line::default().muted(format!(
            "Showing {} of {} checks",
            report.checks.len(),
            report.total_checks
        )));
    }
    lines
}

fn quota_lines(lines: &mut Vec<Line>, quota: Option<&TokenQuota>, timezone: Tz) {
    let Some(quota) = quota else {
        lines.push(Line::default().muted("Quota unavailable."));
        return;
    };
    let Some(limit) = quota.limit else {
        lines.push(Line::default().bold("Unlimited"));
        return;
    };
    let Some(remaining) = quota.remaining else {
        lines.push(Line::default().muted("Quota unavailable."));
        return;
    };
    let remaining = remaining.min(limit);
    let percent = if limit == 0 {
        0
    } else {
        u128::from(remaining) * 100 / u128::from(limit)
    };
    let ink = if quota.exhausted || u128::from(remaining) * 100 <= u128::from(limit) * 5 {
        Ink::Fail
    } else if u128::from(remaining) * 100 <= u128::from(limit) * 20 {
        Ink::Warning
    } else {
        Ink::Pass
    };
    let filled = if limit == 0 {
        0
    } else {
        (u128::from(remaining) * 20 / u128::from(limit)) as usize
    };
    let percent_label = if remaining > 0 && percent == 0 {
        "<1".into()
    } else {
        percent.to_string()
    };
    lines.push(
        Line::default()
            .push("━".repeat(filled), ink, false)
            .muted("─".repeat(20 - filled))
            .push(format!("  {percent_label}% remaining"), ink, true)
            .muted(" · ")
            .plain(format!("{} of {} tokens", compact(remaining), compact(limit))),
    );
    if quota.exhausted {
        lines.push(Line::default().push(
            "New checks are paused. Running checks will finish.",
            Ink::Warning,
            false,
        ));
    }
    if let Some(next) = quota.next_available_at {
        lines.push(Line::default().muted(format!(
            "{} {}",
            if quota.exhausted {
                "Checks available"
            } else {
                "More tokens available"
            },
            timestamp(next, timezone)
        )));
    } else if quota.exhausted {
        lines.push(Line::default().muted("Contact your account administrator to increase your quota."));
    }
}

fn timestamp(value: DateTime<Utc>, timezone: Tz) -> String {
    value.with_timezone(&timezone).format("%b %-d, %-I:%M %p").to_string()
}

fn amount(value: Option<u64>) -> Line {
    value.map_or_else(
        || Line::default().muted("Unavailable"),
        |value| Line::new(compact(value)),
    )
}

fn status(value: CheckStatus) -> (&'static str, Ink) {
    match value {
        CheckStatus::Pass => ("pass", Ink::Pass),
        CheckStatus::Fail => ("fail", Ink::Fail),
        CheckStatus::Conditional => ("conditional", Ink::Warning),
        CheckStatus::Checking => ("checking", Ink::Accent),
        CheckStatus::Queued => ("queued", Ink::Muted),
        CheckStatus::Blocked => ("blocked", Ink::Warning),
        CheckStatus::Canceled => ("canceled", Ink::Muted),
        CheckStatus::Error => ("error", Ink::Fail),
    }
}

fn compact(value: u64) -> String {
    const UNITS: [(u128, &str); 5] = [
        (1, ""),
        (1_000, "K"),
        (1_000_000, "M"),
        (1_000_000_000, "B"),
        (1_000_000_000_000, "T"),
    ];
    let value = u128::from(value);
    let mut unit = UNITS.iter().rposition(|(scale, _)| value >= *scale).unwrap_or(0);
    if unit == 0 {
        return value.to_string();
    }
    let mut tenths = (value * 10 + UNITS[unit].0 / 2) / UNITS[unit].0;
    if tenths >= 10_000 && unit + 1 < UNITS.len() {
        unit += 1;
        tenths = (value * 10 + UNITS[unit].0 / 2) / UNITS[unit].0;
    }
    let suffix = UNITS[unit].1;
    if tenths.is_multiple_of(10) {
        format!("{}{suffix}", tenths / 10)
    } else {
        format!("{}.{}{suffix}", tenths / 10, tenths % 10)
    }
}

fn table(headers: &[&str], rows: Vec<Vec<Line>>, width: usize, token_column: usize, pending_suffix: bool) -> Vec<Line> {
    // A separate suffix cell keeps pending '+' markers outside the aligned amounts.
    let headings: Vec<_> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            let heading = Line::default().muted(header);
            if index == token_column && pending_suffix {
                heading.plain(" ")
            } else {
                heading
            }
        })
        .collect();
    let mut widths: Vec<_> = headers
        .iter()
        .enumerate()
        .map(|(index, _)| {
            rows.iter()
                .map(|row| row[index].width())
                .max()
                .unwrap_or(0)
                .min(width)
                .max(headings[index].width())
        })
        .collect();
    let minimum_widths: Vec<_> = headings
        .iter()
        .enumerate()
        .map(|(index, header)| {
            if index == token_column {
                widths[index]
            } else {
                header.width()
            }
        })
        .collect();
    let minimum = minimum_widths.iter().sum::<usize>() + 3 * (headers.len() - 1);
    if width < minimum {
        return rows
            .into_iter()
            .flat_map(|row| {
                let mut lines: Vec<_> = headers
                    .iter()
                    .zip(row)
                    .map(|(header, value)| Line::default().muted(header.to_lowercase()).muted(" · ").append(value))
                    .collect();
                lines.push(Line::default());
                lines
            })
            .collect();
    }
    while widths.iter().sum::<usize>() + 3 * (headers.len() - 1) > width {
        let index = (0..widths.len())
            .filter(|&index| widths[index] > minimum_widths[index])
            .max_by_key(|&index| widths[index])
            .expect("table can fit its headings");
        widths[index] -= 1;
    }
    let mut lines = vec![table_row(&headings, &widths, token_column)];
    for row in rows {
        let cells: Vec<_> = row
            .iter()
            .zip(&widths)
            .map(|(cell, &width)| cell.wrapped(width))
            .collect();
        for index in 0..cells.iter().map(Vec::len).max().unwrap_or(0) {
            lines.push(table_row(
                &cells
                    .iter()
                    .map(|cell| cell.get(index).cloned().unwrap_or_default())
                    .collect::<Vec<_>>(),
                &widths,
                token_column,
            ));
        }
    }
    lines
}

fn table_row(cells: &[Line], widths: &[usize], token_column: usize) -> Line {
    let mut line = Line::default();
    for (index, (cell, &width)) in cells.iter().zip(widths).enumerate() {
        if index != 0 {
            line = line.muted(" · ");
        }
        let cell = if index == token_column {
            Line::new(" ".repeat(width.saturating_sub(cell.width()))).append(cell.clone())
        } else {
            cell.clone()
        };
        line = line.append(cell.cell(width));
    }
    line
}
