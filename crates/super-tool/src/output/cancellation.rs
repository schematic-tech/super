use super::{
    Results, presentation,
    style::{Ink, Line},
};
use crate::cancel::{Entry, Outcome};

fn label(outcome: Outcome) -> (&'static str, Ink) {
    match outcome {
        Outcome::Canceled => ("canceled", Ink::Muted),
        Outcome::AlreadyCanceled => ("already canceled", Ink::Muted),
        Outcome::AlreadyFinished => ("already finished", Ink::Muted),
        Outcome::BackgroundStopped => ("background work stopped", Ink::Muted),
        Outcome::Finished => ("finished before cancellation", Ink::Muted),
        Outcome::Requested => ("cancellation requested", Ink::Accent),
        Outcome::Unconfirmed => ("cancellation not confirmed", Ink::Warning),
    }
}

fn row_status(entry: &Entry, check: &super_types::Check) -> Line {
    match entry.outcome {
        Outcome::Requested | Outcome::Unconfirmed => {
            let (text, ink) = label(entry.outcome);
            Line::default().push(text, ink, false)
        }
        _ => presentation::status(check).line(),
    }
}

pub(super) fn render(results: &Results, entries: &[Entry], width: usize) -> Vec<Line> {
    let checks: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            results
                .rows
                .iter()
                .filter_map(|row| row.current.as_ref())
                .find(|check| check.number == entry.check_number)
                .map(|check| (entry, check))
        })
        .collect();
    let mut lines = match checks.as_slice() {
        [] => vec![Line::default().push(
            "! Nothing to cancel · no checks in this selection.",
            Ink::Warning,
            false,
        )],
        [(entry, check)] => single(results, entry, check),
        _ => multiple(results, entries, &checks, width),
    };
    for (entry, _) in &checks {
        if let Some(error) = &entry.error {
            lines.extend([
                Line::default(),
                Line::default().push(format!("! Check #{}: {error}", entry.check_number), Ink::Warning, false),
            ]);
        }
    }
    if entries
        .iter()
        .any(|entry| matches!(entry.outcome, Outcome::Requested | Outcome::Unconfirmed))
    {
        lines.push(Line::default());
        lines.push(
            Line::default()
                .muted("Watch: ")
                .push(format!("{} --watch", results.status_command()), Ink::Accent, true)
                .soft_wrap(),
        );
    }
    lines
}

fn single(results: &Results, entry: &Entry, check: &super_types::Check) -> Vec<Line> {
    let mut lines = Vec::new();
    let (label, ink) = label(entry.outcome);
    match entry.outcome {
        Outcome::AlreadyFinished | Outcome::AlreadyCanceled => lines.push(
            Line::default()
                .push("! ", Ink::Warning, false)
                .plain(format!("Nothing to cancel · check #{} is {label}.", check.number)),
        ),
        Outcome::Canceled => lines.push(presentation::canceled_line(check)),
        _ => {
            let mut chars = label.chars();
            let title = format!("{}{}", chars.next().unwrap().to_uppercase(), chars.as_str());
            lines.push(
                Line::default()
                    .push(title, ink, false)
                    .plain(" · ")
                    .bold(&check.supertest.name)
                    .muted(format!(" · #{}", check.number)),
            );
            if entry.outcome == Outcome::BackgroundStopped {
                lines.push(Line::default().muted("Result retained · ").push(
                    presentation::status(check).label,
                    presentation::status(check).ink,
                    false,
                ));
            } else if entry.outcome == Outcome::Finished {
                lines.push(presentation::result_line(check, None));
            }
        }
    }
    if entry.outcome == Outcome::Canceled && presentation::simple_cancellation(check) {
        lines.extend([Line::default(), results.recheck(check)]);
    } else if matches!(entry.outcome, Outcome::Canceled | Outcome::BackgroundStopped) {
        lines.extend([
            Line::default(),
            Line::default().muted("Details: ").push(
                format!("super status --check {}", check.number),
                Ink::Accent,
                true,
            ),
        ]);
    }
    lines
}

fn multiple(results: &Results, entries: &[Entry], checks: &[(&Entry, &super_types::Check)], width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    lines.push(Line::default().bold(&results.repository));
    let nothing = entries
        .iter()
        .all(|entry| matches!(entry.outcome, Outcome::AlreadyCanceled | Outcome::AlreadyFinished));
    if nothing {
        lines.push(Line::default().push("! Nothing to cancel.", Ink::Warning, false));
    }
    let mut summary = Line::default();
    for outcome in [
        Outcome::Canceled,
        Outcome::BackgroundStopped,
        Outcome::Requested,
        Outcome::Unconfirmed,
        Outcome::Finished,
        Outcome::AlreadyFinished,
        Outcome::AlreadyCanceled,
    ] {
        let count = entries.iter().filter(|entry| entry.outcome == outcome).count();
        if count > 0 {
            if !summary.parts.is_empty() {
                summary = summary.muted(" · ");
            }
            let (text, ink) = label(outcome);
            summary = summary.push(format!("{count} {text}"), ink, false);
        }
    }
    lines.extend([summary, Line::default()]);
    let status_width = checks
        .iter()
        .map(|(entry, check)| row_status(entry, check).width())
        .max()
        .unwrap_or(6)
        .max(6);
    let number_width = checks
        .iter()
        .map(|(_, check)| format!("#{}", check.number).len())
        .max()
        .unwrap_or(5)
        .max(5);
    let available = width.saturating_sub(4);
    let name_width = checks
        .iter()
        .map(|(_, check)| Line::new(&check.supertest.name).width())
        .max()
        .unwrap_or(9)
        .max(9)
        .min(available.saturating_sub(status_width + number_width + 6))
        .max(9);
    let table = name_width + status_width + number_width + 6 <= available;
    if table {
        lines.push(
            Line::default()
                .muted("SUPERTEST")
                .cell(name_width)
                .muted(" · ")
                .append(Line::default().muted("STATUS").cell(status_width))
                .muted(" · CHECK"),
        );
    }
    let mut last_path = None;
    let multiple_files = checks.first().is_some_and(|(_, first)| {
        checks
            .iter()
            .any(|(_, check)| check.supertest.path != first.supertest.path)
    });
    for (entry, check) in checks {
        if multiple_files && last_path != Some(&check.supertest.path) {
            lines.push(Line::default().muted(&check.supertest.path));
            last_path = Some(&check.supertest.path);
        }
        let status = row_status(entry, check);
        if table {
            lines.push(
                Line::new(&check.supertest.name)
                    .cell(name_width)
                    .muted(" · ")
                    .append(status.cell(status_width))
                    .muted(format!(" · #{}", check.number)),
            );
        } else {
            lines.push(
                Line::default()
                    .bold(&check.supertest.name)
                    .muted(format!(" · #{}", check.number)),
            );
            lines.push(status);
        }
    }
    lines
}
