use super::{
    Check, FixProposal,
    style::{ADDED, Ink, Line, REMOVED, clean},
};

pub(super) fn preview(repository: &str, check: &Check, fix: &FixProposal, width: usize, details: bool) -> Vec<Line> {
    let mut rows = vec![
        Line::default()
            .bold(repository)
            .muted(format!(" · {}", check.revision.short_identity())),
        Line::default()
            .plain("Fix proposal · ")
            .bold(&check.supertest.name)
            .muted(format!(" · #{}", check.number)),
    ];
    // This legacy API placeholder adds no information. Preserve it in --details and
    // JSON; do not try to classify or rewrite any other proposal summary.
    let summary = fix.summary.trim();
    if !summary.is_empty() && (details || summary != "Review the proposed correction.") {
        rows.push(Line::new(summary));
    }
    rows.push(Line::default());
    rows.extend(patch(&fix.diff, width));
    if details && !fix.instructions.trim().is_empty() {
        rows.extend([
            Line::default(),
            Line::default().bold("Instructions"),
            Line::new(fix.instructions.trim()),
        ]);
    }
    if details && !fix.validation.is_empty() {
        rows.extend([Line::default(), Line::default().bold("Suggested validation")]);
        rows.extend(fix.validation.iter().map(|line| Line::new(format!("  {line}"))));
    }
    rows.push(Line::default());
    rows
}

pub(super) fn patch(diff: &str, width: usize) -> Vec<Line> {
    let source: Vec<_> = diff.lines().collect();
    let digits = line_number_width(&source);
    let mut rows = Vec::new();
    let mut index = 0;
    let mut old = 0_u64;
    let mut new = 0_u64;
    let mut hunk = false;
    while index < source.len() {
        let text = source[index];
        if let Some(header) = text.strip_prefix("diff --git a/") {
            let path = header.split_once(" b/").map_or(header, |(_, path)| path);
            let next = source[index + 1..]
                .iter()
                .position(|line| line.starts_with("diff --git "))
                .map_or(source.len(), |offset| index + 1 + offset);
            let added = source[index + 1..next]
                .iter()
                .filter(|line| line.starts_with('+') && !line.starts_with("+++ "))
                .count();
            let removed = source[index + 1..next]
                .iter()
                .filter(|line| line.starts_with('-') && !line.starts_with("--- "))
                .count();
            if !rows.is_empty() {
                rows.push(Line::default());
            }
            let heading = Line::default()
                .bold(path)
                .push(format!("  +{added}"), Ink::Pass, false)
                .push(format!(" -{removed}"), Ink::Fail, false);
            if heading.width() > width.saturating_sub(4) {
                rows.extend(Line::default().bold(path).wrapped(width.saturating_sub(4)));
                rows.push(Line::default().push(format!("+{added}"), Ink::Pass, false).push(
                    format!(" -{removed}"),
                    Ink::Fail,
                    false,
                ));
            } else {
                rows.push(heading);
            }
            hunk = false;
        } else if text.starts_with("@@ ") {
            if let Some((left, right)) = hunk_start(text) {
                old = left;
                new = right;
                hunk = true;
            }
            rows.push(Line::default().muted(format!("old new   {text}")));
        } else if hunk && (text.starts_with('-') || text.starts_with('+')) {
            let start = index;
            while index < source.len() && (source[index].starts_with('-') || source[index].starts_with('+')) {
                index += 1;
            }
            let block = &source[start..index];
            for (raw, content) in block.iter().zip(emphasized_block(block)) {
                let added = raw.starts_with('+');
                rows.extend(code_rows(
                    if added { None } else { Some(old) },
                    if added { Some(new) } else { None },
                    if added { '+' } else { '-' },
                    &content,
                    width,
                    digits,
                ));
                if added {
                    new += 1;
                } else {
                    old += 1;
                }
            }
            continue;
        } else if hunk && let Some(code) = text.strip_prefix(' ') {
            rows.extend(code_rows(Some(old), Some(new), ' ', &Line::new(code), width, digits));
            old += 1;
            new += 1;
        } else if !text.starts_with("--- ") && !text.starts_with("+++ ") && !text.starts_with("index ") {
            // Retain mode changes, no-newline notices, and any other patch metadata.
            rows.push(Line::default().muted(text));
        }
        index += 1;
    }
    rows
}

fn hunk_start(text: &str) -> Option<(u64, u64)> {
    let mut parts = text.strip_prefix("@@ ")?.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?.split(',').next()?.parse().ok()?;
    let new = parts.next()?.strip_prefix('+')?.split(',').next()?.parse().ok()?;
    Some((old, new))
}

fn matching_edges(text: &str, other: &str) -> (usize, usize, usize) {
    let a: Vec<_> = text.chars().collect();
    let b: Vec<_> = other.chars().collect();
    let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let score = a[..prefix]
        .iter()
        .chain(a[a.len() - suffix..].iter())
        .filter(|c| !c.is_whitespace())
        .count();
    (prefix, a.len() - suffix, score)
}

fn emphasize(text: &str, other: Option<&str>, ink: Ink) -> Line {
    let chars: Vec<_> = text.chars().collect();
    let (start, end, _) = other.map_or((0, chars.len(), 0), |other| matching_edges(text, other));
    Line::default()
        .push(chars[..start].iter().collect::<String>(), ink, false)
        .push(chars[start..end].iter().collect::<String>(), ink, true)
        .push(chars[end..].iter().collect::<String>(), ink, false)
}

fn emphasized_block(block: &[&str]) -> Vec<Line> {
    let lines: Vec<_> = block
        .iter()
        .map(|line| (line.starts_with('+'), clean(&line[1..])))
        .collect();
    let peers: Vec<_> = lines
        .iter()
        .map(|(added, code)| {
            lines
                .iter()
                .enumerate()
                .filter(|(_, (candidate_added, _))| candidate_added != added)
                .take(100)
                .max_by_key(|(_, (_, candidate))| matching_edges(code, candidate).2)
                .map(|(index, _)| index)
        })
        .collect();
    lines
        .iter()
        .enumerate()
        .map(|(index, (added, code))| {
            // Only mutually matching lines share fragment emphasis. Reusing one old
            // line for several additions can make a wholly new line look unchanged.
            let counterpart = peers[index]
                .filter(|peer| peers[*peer] == Some(index))
                .map(|peer| lines[peer].1.as_str());
            emphasize(
                code,
                counterpart,
                if *added { Ink::AddedText } else { Ink::RemovedText },
            )
        })
        .collect()
}

fn code_rows(old: Option<u64>, new: Option<u64>, kind: char, content: &Line, width: usize, digits: usize) -> Vec<Line> {
    let gutter = digits * 2 + 4;
    let ink = match kind {
        '+' => Ink::Pass,
        '-' => Ink::Fail,
        _ => Ink::Muted,
    };
    content
        .wrapped(width.saturating_sub(4 + gutter))
        .into_iter()
        .enumerate()
        .map(|(index, content)| {
            let number = |value: Option<u64>| {
                if index == 0 {
                    value.map_or_else(String::new, |value| value.to_string())
                } else {
                    String::new()
                }
            };
            let line = Line::default()
                .muted(format!("{:>digits$} {:>digits$} ", number(old), number(new)))
                .push(format!("{kind} "), ink, false)
                .append(content);
            match kind {
                '+' => line.background(ADDED),
                '-' => line.background(REMOVED),
                _ => line,
            }
        })
        .collect()
}

fn line_number_width(source: &[&str]) -> usize {
    source
        .iter()
        .filter(|line| line.starts_with("@@ "))
        .flat_map(|line| {
            line.split_whitespace().skip(1).take(2).filter_map(|range| {
                let mut parts = range.get(1..)?.split(',');
                let start = parts.next()?.parse::<u64>().ok()?;
                let length = parts.next().unwrap_or("1").parse::<u64>().ok()?;
                Some(start.saturating_add(length.saturating_sub(1)))
            })
        })
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(3)
}
