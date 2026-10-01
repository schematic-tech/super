//! Compatibility reader for the API's existing, server-authored presentation format.
//!
//! Match the complete record grammar, including emphasis, separators, sequential problem
//! numbers, and all four evidence fields. Never filter arbitrary prose by a heading/prefix.
//! Unknown layouts fall back losslessly to the full presentation. Keep values as borrowed
//! strings: a multiline witness must not be mistaken for another presentation record.

use super_types::{Check, CheckAssurance, CheckOutcome, PresentationLine};

use super::{
    presentation,
    style::{Ink, Line},
};

struct Evidence<'a> {
    values: [&'a str; 4],
}

struct Problem<'a> {
    location: &'a str,
    evidence: Vec<Evidence<'a>>,
}

struct Parsed<'a> {
    conclusion: Option<Conclusion<'a>>,
    problems: Vec<Problem<'a>>,
}

struct Conclusion<'a> {
    headline: &'a str,
    explanation: Option<&'a str>,
}

impl Conclusion<'_> {
    fn lines(&self, include_explanation: bool) -> Vec<Line> {
        let mut lines = vec![Line::default().push(
            self.headline,
            Ink::Plain,
            include_explanation && self.explanation.is_some(),
        )];
        if include_explanation && let Some(explanation) = self.explanation {
            lines.push(Line::default());
            lines.push(Line::new(format!("  {explanation}")));
        }
        lines
    }
}

fn plain(line: &PresentationLine) -> Option<&str> {
    (!line.emphasized).then_some(line.text.as_str())
}

fn heading(line: &PresentationLine, text: &str) -> bool {
    line.emphasized && line.text == text
}

fn parse(mut lines: &[PresentationLine]) -> Option<Parsed<'_>> {
    let mut parsed = Parsed {
        conclusion: None,
        problems: Vec::new(),
    };
    if lines.first().is_some_and(|line| plain(line) == Some("")) {
        lines = &lines[1..];
    }
    if lines.first().is_some_and(|line| heading(line, "Conclusion")) {
        lines = &lines[1..];
        let mut body = Vec::new();
        while let Some(line) = lines.first().filter(|line| plain(line) != Some("")) {
            body.push(plain(line)?.strip_prefix("  ")?);
            lines = &lines[1..];
        }
        let headline = *body.first()?;
        // The current producer emits a single-line headline, then one explanation record.
        // Never split prose to guess those boundaries.
        if body.len() > 2 || headline.trim().is_empty() || headline.contains('\n') {
            return None;
        }
        parsed.conclusion = Some(Conclusion {
            headline,
            explanation: body.get(1).copied(),
        });
    }
    while !lines.is_empty() {
        if plain(&lines[0]) == Some("") {
            lines = &lines[1..];
        }
        if !heading(lines.first()?, &format!("Problem {}", parsed.problems.len() + 1)) {
            return None;
        }
        let location = plain(lines.get(1)?)?.strip_prefix("  Location: ")?;
        lines = &lines[2..];
        let mut evidence = Vec::new();
        while lines
            .get(1)
            .is_some_and(|line| heading(line, "Counterexample") || heading(line, "Certified counterexample"))
        {
            if plain(&lines[0]) != Some("") {
                return None;
            }
            let mut values = [""; 4];
            for (index, name) in ["witness", "expected", "observed", "reproduce"].iter().enumerate() {
                values[index] = plain(lines.get(index + 2)?)?.strip_prefix(&format!("  {name:9} = "))?;
            }
            evidence.push(Evidence { values });
            lines = &lines[6..];
        }
        parsed.problems.push(Problem { location, evidence });
    }
    Some(parsed)
}

/// Validated record positions for consistent narrative styling in the expanded view.
pub(super) struct NarrativeLayout {
    pub headline: usize,
    pub explanation: Option<usize>,
}

pub(super) fn narrative_layout(check: &Check) -> Option<NarrativeLayout> {
    let conclusion = parse(&check.presentation.details)?.conclusion?;
    let headline = usize::from(check.presentation.details[0].text.is_empty()) + 1;
    Some(NarrativeLayout {
        headline,
        explanation: conclusion.explanation.map(|_| headline + 1),
    })
}

pub(super) fn details(check: &Check) -> Vec<Line> {
    let Some(parsed) = parse(&check.presentation.details) else {
        return presentation::full_details(check);
    };
    // Only successful checks without problems or errors collapse the explanation;
    // full details retain every narrative record.
    let headline_only = presentation::is_pass(check)
        && check.terminal
        && !check.problematic
        && check.operational_error.is_none()
        && parsed.problems.is_empty();
    let mut lines = Vec::new();
    let has_problem = !parsed.problems.is_empty() || check.problematic;
    if has_problem && parsed.conclusion.is_none() && parsed.problems.is_empty() {
        lines.push(Line::default().bold("Problem"));
        lines.push(Line::new(presentation::explanation(check, false)));
    }
    if let Some(conclusion) = &parsed.conclusion {
        lines.extend(conclusion.lines(!headline_only));
    }
    for (index, problem) in parsed.problems.iter().enumerate() {
        if parsed.conclusion.is_some() || index > 0 {
            lines.push(Line::default());
        }
        if parsed.problems.len() > 1 {
            lines.push(Line::default().bold(format!("Problem {}", index + 1)));
        }
        if problem.location != presentation::location(&check.supertest) {
            lines.push(Line::default().muted(format!("  Location: {}", problem.location)));
        }
        for (index, evidence) in problem.evidence.iter().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            let certified = check.result.as_ref().is_some_and(|result| {
                result.outcome == CheckOutcome::Fail && result.assurance == CheckAssurance::Certified
            });
            lines.push(Line::default().bold(if certified {
                "Certified counterexample"
            } else {
                "Counterexample"
            }));
            for (name, value) in ["witness", "expected", "observed"].into_iter().zip(evidence.values) {
                lines.push(Line::new(format!("  {name:8} = {value}")));
            }
        }
    }
    if lines.is_empty() && !presentation::is_pass(check) && check.terminal {
        lines.push(Line::new(presentation::explanation(check, false)));
    }
    lines
}
