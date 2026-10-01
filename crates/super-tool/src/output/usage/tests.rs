use super::*;

fn report() -> UsageReport {
    serde_json::from_str(include_str!("../../../tests/fixtures/usage.json")).unwrap()
}

fn query() -> UsageQuery {
    UsageQuery {
        period: Period::Month,
        timezone: "UTC".into(),
        repository_id: None,
        sort: super_types::usage::UsageSort::StartedAt,
        direction: super_types::usage::Direction::Descending,
        page: 1,
        page_size: None,
        snapshot: None,
    }
}

fn text(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| line.text().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn compact_numbers_round_without_overflow_or_spurious_precision() {
    for (value, expected) in [
        (0, "0"),
        (999, "999"),
        (1_000, "1K"),
        (1_049, "1K"),
        (1_050, "1.1K"),
        (999_950, "1M"),
        (1_234_567, "1.2M"),
        (999_950_000, "1B"),
        (2_300_000_000, "2.3B"),
        (u64::MAX, "18446744.1T"),
    ] {
        assert_eq!(compact(value), expected);
    }
}

#[test]
fn report_has_compact_tables_and_separates_bold_context_from_muted_headings() {
    let lines = report_lines(&report(), &query(), chrono_tz::UTC, 76);
    assert_eq!(
        text(&lines),
        "Super usage · Sep 25, 12:42 AM · UTC\n\nWeekly quota · all repositories · rolling 7 days\n━━━━━━━━━━━━━━━─────  75% remaining · 750M of 1B tokens\nMore tokens available Sep 26, 12:30 AM\n\nLast 30 days · All repositories\nTOKENS USED · SUPERTESTS RUN\n     228.7K · 3\n\nRecent checks\n\nSUPERTEST             · REPOSITORY · STATUS   · TOKENS\namounts_stay_positive · payments   · pass     · 128.4K\nrefunds_are_bounded   · payments   · fail     ·  92.1K\nroundtrip             · parser     · checking ·   8.2K+\n\nShowing 3 of 3 checks\n+ Usage is still being reported"
    );
    assert!(
        lines[0]
            .parts
            .iter()
            .any(|(text, ink, bold)| text == "Super usage" && *ink == Ink::Accent && *bold)
    );
    assert!(!lines.last().unwrap().text().is_empty());
    let checking = lines.iter().find(|line| line.text().starts_with("roundtrip")).unwrap();
    assert!(
        checking
            .parts
            .iter()
            .any(|(value, ink, _)| value == "checking" && *ink == Ink::Accent)
    );
    let token_end = lines
        .iter()
        .find(|line| line.text().starts_with("amounts_stay_positive"))
        .unwrap()
        .text()
        .trim_end()
        .len();
    assert_eq!(checking.text().find('+'), Some(token_end));
    let footer = lines.iter().find(|line| line.text().starts_with("Showing")).unwrap();
    assert!(footer.parts.iter().all(|(_, ink, bold)| *ink == Ink::Muted && !bold));
    let context = lines
        .iter()
        .position(|line| line.text().starts_with("Last 30 days"))
        .unwrap();
    assert!(
        lines[context]
            .parts
            .iter()
            .filter(|(value, _, _)| value != " · ")
            .all(|(_, _, bold)| *bold)
    );
    assert!(
        lines[context + 1]
            .parts
            .iter()
            .filter(|(value, _, _)| !value.trim().is_empty())
            .all(|(_, ink, bold)| *ink == Ink::Muted && !bold)
    );
}

#[test]
fn quota_states_preserve_unknowns_and_use_recovery_times_in_the_selected_timezone() {
    let mut data = report();
    let quota = data.quota.as_mut().unwrap();
    quota.limit = None;
    quota.remaining = None;
    let mut lines = Vec::new();
    quota_lines(&mut lines, Some(quota), chrono_tz::UTC);
    assert_eq!(text(&lines), "Unlimited");
    quota.limit = Some(1_000);
    lines.clear();
    quota_lines(&mut lines, Some(quota), chrono_tz::UTC);
    assert_eq!(text(&lines), "Quota unavailable.");
    for (remaining, ink, label) in [
        (201, Ink::Pass, "20%"),
        (51, Ink::Warning, "5%"),
        (50, Ink::Fail, "5%"),
        (1, Ink::Fail, "<1%"),
    ] {
        quota.remaining = Some(remaining);
        lines.clear();
        quota_lines(&mut lines, Some(quota), chrono_tz::UTC);
        assert!(
            lines[0]
                .parts
                .iter()
                .any(|(value, tone, bold)| value.contains(label) && *tone == ink && *bold)
        );
    }
    quota.limit = Some(0);
    quota.remaining = Some(0);
    quota.exhausted = true;
    lines.clear();
    quota_lines(&mut lines, Some(quota), chrono_tz::Australia::Sydney);
    assert!(text(&lines).contains("0% remaining · 0 of 0 tokens"));
    assert!(text(&lines).contains("Checks available Sep 26, 10:30 AM"));
    quota.next_available_at = None;
    lines.clear();
    quota_lines(&mut lines, Some(quota), chrono_tz::UTC);
    assert!(text(&lines).contains("Contact your account administrator"));
}

#[test]
fn missing_measurements_never_look_like_zero_or_hide_checks() {
    let mut data = report();
    data.quota = None;
    data.usage_available = false;
    data.total_tokens = None;
    data.checks[0].period_tokens = None;
    let output = text(&report_lines(&data, &query(), chrono_tz::UTC, 76));
    assert!(output.contains("Quota unavailable."));
    assert!(output.contains("Unavailable · 3"));
    assert!(output.contains("Some usage measurements are unavailable"));
    assert!(output.contains("amounts_stay_positive"));
    assert!(!output.contains("128.4K"));
    for check in &mut data.checks {
        check.usage_pending = false;
    }
    // Older checks outside the displayed page can still make the overall totals pending.
    data.total_checks = 13;
    let output = text(&report_lines(&data, &query(), chrono_tz::UTC, 76));
    assert!(output.ends_with("\nUsage is still being reported"));
    assert!(!output.contains("+ Usage"));
    data.pending = false;
    let output = text(&report_lines(&data, &query(), chrono_tz::UTC, 76));
    assert!(!output.contains("Usage is still being reported"));
    data.checks.clear();
    data.total_checks = 0;
    let output = text(&report_lines(&data, &query(), chrono_tz::UTC, 76));
    assert!(output.contains("No checks in this period."));
    assert!(!output.contains("SUPERTEST             ·"));
}

#[test]
fn narrow_tables_wrap_long_unicode_names_and_sanitize_terminal_controls() {
    let name = "\x1b[31m支付_very_long_supertest_with_details\x1b[0m";
    for width in [36, 50, 76] {
        let lines = table(
            &["SUPERTEST", "REPOSITORY", "STATUS", "TOKENS"],
            vec![vec![
                Line::new(name),
                Line::new("payments"),
                Line::new("conditional"),
                Line::new("1.2B+"),
            ]],
            width,
            3,
            true,
        );
        let wrapped: Vec<_> = lines.iter().flat_map(|line| line.wrapped(width)).collect();
        assert!(wrapped.iter().all(|line| line.width() <= width));
        let output = text(&wrapped);
        assert!(!output.contains('\x1b'));
        assert!(!output.contains('…'));
        let first_column = if width >= 42 {
            wrapped
                .iter()
                .skip(1)
                .map(|line| line.text().split(" · ").next().unwrap().trim_end().to_owned())
                .collect::<String>()
        } else {
            output
                .lines()
                .take_while(|line| !line.starts_with("repository"))
                .collect::<String>()
                .replace("supertest · ", "")
        };
        assert_eq!(first_column, "支付_very_long_supertest_with_details");
    }
}
