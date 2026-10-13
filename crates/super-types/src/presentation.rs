//! Display compatibility for saved results and clients connected to older servers.

use crate::{Check, CheckOutcome, FixProposal};

/// Whether editorial prose describes certification instead of the code's behavior.
#[must_use]
pub fn certification_wording(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphabetic()).any(|word| {
        [
            "certified",
            "uncertified",
            "certify",
            "certifies",
            "certifying",
            "certification",
            "certifications",
            "certificate",
            "certificates",
        ]
        .iter()
        .any(|blocked| word.eq_ignore_ascii_case(blocked))
    })
}

impl Check {
    /// Normalize saved display fields while preserving the outcome, evidence, source, and patch.
    pub fn normalize_presentation(&mut self) {
        if let Some(result) = &mut self.result
            && matches!(result.assurance.as_str(), "certified" | "uncertified")
        {
            result.assurance = "standard".into();
        }
        let outcome = self.result.as_ref().map(|result| result.outcome);
        let (label, sentence) = match outcome {
            Some(CheckOutcome::Pass) => ("pass", "No problems found within this supertest."),
            Some(CheckOutcome::Fail) => ("fail", "This supertest found a problem."),
            Some(CheckOutcome::Conditional) => (
                "conditional",
                "This result depends on conditions that have not all been established.",
            ),
            None if self.terminal => ("result unavailable", "No result was reported."),
            None => ("checking", "The check is in progress."),
        };
        let presentation = &mut self.presentation;
        if certification_wording(&presentation.status.label) {
            presentation.status.label = label.into();
        }
        if presentation
            .history
            .outcome_group
            .as_deref()
            .is_some_and(certification_wording)
        {
            presentation.history.outcome_group = Some(label.into());
        }
        for text in [
            &mut presentation.live_line,
            &mut presentation.activity_label,
            &mut presentation.active_problem_label,
            &mut presentation.history.outcome_group,
            &mut presentation.history.collapse_group,
        ] {
            if text.as_deref().is_some_and(certification_wording) {
                *text = None;
            }
        }

        // Only the producer's Conclusion records are editorial. Never scan evidence values
        // or source locations as prose, even when they contain newlines or heading text.
        let details = &mut presentation.details;
        let start = usize::from(
            details
                .first()
                .is_some_and(|line| !line.emphasized && line.text.is_empty()),
        );
        if details
            .get(start)
            .is_some_and(|line| line.emphasized && line.text == "Conclusion")
        {
            let end = details[start + 1..]
                .iter()
                .position(|line| line.emphasized || line.text.is_empty())
                .map_or(details.len(), |offset| start + 1 + offset);
            if details[start + 1..end]
                .iter()
                .any(|line| certification_wording(&line.text))
            {
                details.splice(
                    start + 1..end,
                    [crate::PresentationLine {
                        text: format!("  {sentence}"),
                        emphasized: false,
                    }],
                );
            }
        }
        for line in details {
            if line.emphasized && line.text == "Certified counterexample" {
                line.text = "Counterexample".into();
            }
        }
        if let Some(fix) = &mut self.fix {
            fix.normalize_presentation();
        }
    }

    /// Return a display-compatible copy at a public response boundary.
    #[must_use]
    pub fn into_public_view(mut self) -> Self {
        self.normalize_presentation();
        self
    }
}

impl FixProposal {
    /// Normalize editorial fields while preserving executable validation and patch data.
    pub fn normalize_presentation(&mut self) {
        if certification_wording(&self.summary) {
            self.summary = "Proposed fix for this supertest.".into();
        }
        if certification_wording(&self.instructions) {
            self.instructions = "Review the patch, apply it to the checked version, and rerun the supertest.".into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn saved_check() -> Check {
        let id = "00000000-0000-0000-0000-000000000001";
        serde_json::from_value(json!({
            "repository_id": id, "number": 1,
            "supertest": {"path": "certified.rs", "name": "uncertified", "language": "rust"},
            "revision": {"id": id, "tree_sha256": "a".repeat(64)},
            "terminal": true, "problematic": true,
            "result": {"outcome": "fail", "assurance": "certified"},
            "operational_error": "canceled",
            "presentation": {
                "status": {"marker": "×", "label": "fail (certified)", "tone": "error"},
                "live_line": "Certifying the result.", "activity_label": "certifying",
                "history": {"outcome_group": "fail.certified", "collapse_group": "fail.certified"},
                "details": [
                    {"text": "Conclusion", "emphasized": true},
                    {"text": "  The result is uncertified.", "emphasized": false},
                    {"text": "  No certificate was accepted.", "emphasized": false},
                    {"text": "", "emphasized": false},
                    {"text": "Problem 1", "emphasized": true},
                    {"text": "  Location: certified.rs:1", "emphasized": false},
                    {"text": "", "emphasized": false},
                    {"text": "Certified counterexample", "emphasized": true},
                    {"text": "  witness   = uncertified\nConclusion\nCertified counterexample", "emphasized": false}
                ]
            },
            "fix": {
                "id": id, "state": "proposed", "base_revision_id": id, "base_tree_sha256": "a".repeat(64),
                "summary": "The certified fix.", "instructions": "Apply the uncertified patch.",
                "diff": "-uncertified\n+certified\n", "files": [],
                "validation": ["test --certified"], "created_at": "2026-01-01T00:00:00Z"
            },
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:01:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn saved_display_is_neutral_without_rewriting_authority_or_exact_data() {
        let before = saved_check();
        let mut after = before.clone().into_public_view();
        assert_eq!(
            after.result.as_ref().unwrap().outcome,
            before.result.as_ref().unwrap().outcome
        );
        assert_eq!(after.result.as_ref().unwrap().assurance, "standard");
        assert_eq!(after.operational_error, before.operational_error);
        assert_eq!(after.supertest, before.supertest);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.presentation.status.label, "fail");
        assert_eq!(after.presentation.history.outcome_group.as_deref(), Some("fail"));
        assert_eq!(after.presentation.history.collapse_group, None);
        assert_eq!(after.presentation.live_line, None);
        assert_eq!(after.presentation.activity_label, None);
        assert_eq!(after.presentation.details[1].text, "  This supertest found a problem.");
        assert_eq!(after.presentation.details[4], before.presentation.details[5]);
        assert_eq!(after.presentation.details[6].text, "Counterexample");
        assert_eq!(after.presentation.details[7], before.presentation.details[8]);
        let fix = after.fix.as_ref().unwrap();
        let original = before.fix.as_ref().unwrap();
        assert!(!certification_wording(&fix.summary));
        assert!(!certification_wording(&fix.instructions));
        assert_eq!(fix.diff, original.diff);
        assert_eq!(fix.files, original.files);
        assert_eq!(fix.validation, original.validation);
        let normalized = after.clone();
        after.normalize_presentation();
        assert_eq!(after, normalized);
    }

    #[test]
    fn wording_matches_words_case_insensitively_without_matching_fragments() {
        for text in [
            "UNCERTIFIED",
            "non-certified",
            "certification.",
            "certificates",
            "certifying…",
        ] {
            assert!(certification_wording(text), "{text}");
        }
        assert!(!certification_wording("The certifier returns a value."));
    }

    #[test]
    fn assurance_is_an_open_string_and_only_legacy_values_are_normalized() {
        for value in ["certified", "uncertified", "standard", "custom"] {
            let mut check = saved_check();
            check.result.as_mut().unwrap().assurance = value.into();
            let normalized = check.into_public_view();
            let encoded = serde_json::to_value(normalized.result).unwrap();
            assert_eq!(
                encoded["assurance"],
                if value == "custom" { "custom" } else { "standard" }
            );
        }
    }

    #[test]
    fn normalization_keeps_unrelated_presentation_and_does_not_invent_a_verdict() {
        let mut check = saved_check().into_public_view();
        check.result = None;
        check.presentation.status.label = "uncertified".into();
        check.normalize_presentation();
        assert_eq!(check.result, None);
        assert_eq!(check.presentation.status.label, "result unavailable");
        check.presentation.status.label = "canceled".into();
        check.presentation.history.collapse_group = Some("custom".into());
        let before = check.clone();
        check.normalize_presentation();
        assert_eq!(check, before);
    }
}
