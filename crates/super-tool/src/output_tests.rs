use super::browser::Mode;
use super::presentation::{EvidenceView, evidence};
use super::style::clean;
use super::*;
use console::measure_text_width;
use crossterm::event::{KeyEvent, KeyModifiers};

#[test]
fn terminal_repaints_preserve_stable_text_and_clear_stale_rows() {
    let mut frame = TerminalFrame::default();
    let mut output = Vec::new();
    let initial = vec!["⠋ checking".into(), "copy this ".into(), "old detail".into()];
    frame.render(&mut output, initial.clone(), (10, 3)).unwrap();
    assert!(String::from_utf8_lossy(&output).contains("copy this "));

    output.clear();
    frame.render(&mut output, initial.clone(), (10, 3)).unwrap();
    assert!(output.is_empty(), "idle ticks must leave selections untouched");

    let mut updated = initial;
    updated[0] = "⠙ checking".into();
    updated[2] = "          ".into();
    frame.render(&mut output, updated.clone(), (10, 3)).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output),
        "\x1b[1;1H⠙ checking\x1b[3;1H          ",
        "spinner changes and vacated rows must not rewrite selectable text"
    );

    output.clear();
    updated.push("          ".into());
    frame.render(&mut output, updated, (10, 4)).unwrap();
    assert!(
        String::from_utf8_lossy(&output).contains("\x1b[2;1Hcopy this "),
        "resize must repaint even rows whose content has not changed"
    );
}

fn check(index: u64) -> Check {
    serde_json::from_value(serde_json::json!({
        "number": index,
        "repository_id": Uuid::nil(),
        "supertest": {"path": "supertests/sequences.py", "name": format!("law_{index}"), "language": "python", "line": 3},
        "revision": {"id": Uuid::nil(), "tree_sha256": "a".repeat(64), "reported_git_commit": "b".repeat(40)},
        "terminal": false, "problematic": false, "operational_error": null,
        "presentation": {
            "status": {"marker": "●", "label": "checking", "tone": "active"},
            "live_line": "Reviewing the proposed result and supporting evidence.",
            "activity_label": "reviewing",
            "activity_updated_at": "2026-01-01T00:00:00Z", "history": {}
        },
        "created_at": format!("2026-01-01T00:00:{:02}Z", index % 60),
        "updated_at": "2026-01-01T00:01:00Z", "event_sequence": 10
    })).unwrap()
}

#[test]
fn usage_and_cli_status_follow_typed_outcomes_instead_of_display_wording() {
    use super_types::{CheckAssurance, CheckOperationalError, CheckOutcome, usage::CheckStatus};

    let mut active = check(1);
    active.presentation.status.label = "pass".into();
    assert_eq!(CheckStatus::from_check(&active), CheckStatus::Checking);
    assert_eq!(status(&active).label, "checking");
    for (error, expected, label) in [
        (CheckOperationalError::Blocked, CheckStatus::Blocked, "blocked"),
        (CheckOperationalError::Canceled, CheckStatus::Canceled, "canceled"),
        (CheckOperationalError::MissingConclusion, CheckStatus::Error, "error"),
    ] {
        active.operational_error = Some(error);
        assert_eq!(CheckStatus::from_check(&active), expected);
        assert_eq!(status(&active).label, label);
    }
    let mut reported = result_check(2, CheckOutcome::Pass, CheckAssurance::Uncertified);
    reported.operational_error = Some(CheckOperationalError::Canceled);
    reported.presentation.status.label = "checking".into();
    assert_eq!(CheckStatus::from_check(&reported), CheckStatus::Pass);
    assert_eq!(status(&reported).label, "pass");
}

fn results(count: usize) -> Results {
    Results {
        repository: "supertest-examples-bad".into(),
        repository_root: std::env::current_dir().unwrap(),
        workspace_id: Uuid::nil(),
        head: None,
        temporary_commit_parents: HashMap::new(),
        run: None,
        selector: ".".into(),
        history: Vec::new(),
        next_before: None,
        history_loaded: true,
        history_loading: false,
        history_error: None,
        rows: (1..=count)
            .map(|index| {
                let check = check(index as u64);
                ScopeStatusRow {
                    supertest: check.supertest.clone(),
                    current: Some(check),
                    previous: None,
                }
            })
            .collect(),
    }
}

#[test]
fn human_follow_ups_use_readable_targets_while_json_keeps_the_exact_run() {
    let mut results = results(2);
    let run_id = Uuid::from_u128(17);
    results.run = Some(CheckSubmission {
        id: run_id,
        repository_id: results.workspace_id,
        selector: ".".into(),
        check_numbers: vec![1, 2],
        created_at: Utc::now(),
    });
    results.next_before = Some(1);
    let ui = Ui {
        json: false,
        interactive: false,
        details: true,
        release_alert: None,
    };
    assert_eq!(ui.follow_up(&results, false).text(), "Details: super status --details");
    assert_eq!(
        ui.follow_up(&results, true).text(),
        "Watch: super status --watch --details"
    );
    assert_eq!(results.json_target(), format!("--run {run_id}"));

    results.selector = "supertests/a file.py".into();
    assert_eq!(
        ui.follow_up(&results, true).text(),
        "Watch: super status 'supertests/a file.py' --watch --details"
    );
    for history in [false, true] {
        let lines = presentation::snapshot(&results, history, true, 80);
        let hint = lines
            .iter()
            .find(|line| line.text().starts_with("More history:"))
            .unwrap();
        assert_eq!(
            hint.text(),
            "More history: super status 'supertests/a file.py' --history --before 1"
        );
        assert!(hint.soft_wrap, "copying long commands must preserve shell arguments");
        assert!(!lines_text(&lines).contains(&run_id.to_string()));
    }
    results.rows.truncate(1);
    assert_eq!(
        ui.follow_up(&results, false).text(),
        "Details: super status --check 1 --details"
    );
    assert_eq!(results.json_target(), format!("--run {run_id}"));
}

#[test]
fn human_selector_hints_resolve_from_subdirectories_and_quote_shell_characters() {
    let mut results = results(2);
    results.repository_root = PathBuf::from("/repo");
    for (selector, current, expected) in [
        ("supertests/test.py", "/repo", "supertests/test.py"),
        ("supertests/test.py", "/repo/supertests", "test.py"),
        ("supertests/test.py", "/repo/src/nested", "../../supertests/test.py"),
        (".", "/repo/src", ".."),
        ("-tests/test.py", "/repo", "./-tests/test.py"),
        ("supertests/a b's.py::law", "/repo/supertests", "a b's.py::law"),
    ] {
        let current = Path::new(current);
        results.selector = selector.into();
        let hint = results.command_selector(current);
        assert_eq!(hint, expected);
        let (hint_path, _) = hint.split_once("::").unwrap_or((&hint, ""));
        assert!(!hint_path.starts_with('-'));
    }
    assert_eq!(shell_quote("a b's.py::law"), "'a b'\\''s.py::law'");
}

#[test]
fn repeated_run_membership_does_not_duplicate_check_history() {
    let mut results = results(1);
    let check = results.rows[0].current.as_ref().unwrap().clone();
    for _ in 0..3 {
        results
            .append_history(super_types::CheckHistoryPage {
                checks: vec![check.clone()],
                next_before: None,
            })
            .unwrap();
    }
    assert_eq!(results.history.len(), 1);
    assert_eq!(results.attempts(0).len(), 1);
    assert!(results.rows[0].previous.is_none());
    assert_eq!(Browser::new(&results).mode, Mode::List);
}

#[test]
fn previous_column_is_hidden_when_every_previous_result_is_unloaded() {
    let mut results = results(2);
    results.history_loaded = false;
    let frame = Browser::new(&results).frame(&results, 120, 36, false);
    assert!(frame.iter().take(5).all(|line| !line.contains("PREVIOUS")));
}

#[test]
fn release_alert_stays_visible_while_browsing_without_hiding_controls() {
    let results = results(48);
    for message in [
        "Installation is changing.\nSee https://example.test/install.".to_owned(),
        "A long notice. ".repeat(500),
    ] {
        for (width, height) in [(60, 20), (80, 24), (120, 36)] {
            let mut browser = Browser::new(&results);
            browser.release_alert = Some(message.clone());
            for _ in 0..2 {
                let lines = plain_frame(&mut browser, &results, width, height);
                assert_eq!(lines.len(), height);
                assert!(lines.iter().all(|line| measure_text_width(line) <= width));
                let text = lines.join("\n");
                assert_eq!(text.matches("Super CLI notice:").count(), 1);
                assert!(text.contains("Esc/Ctrl+C detach"), "{text}");
                if message.starts_with("Installation") {
                    assert!(text.contains("https://example.test/install."));
                }
                browser.key(KeyCode::Down, &results, height, width);
            }
        }
    }
}

#[test]
fn table_is_name_first_with_activity_and_only_shows_previous_results_when_useful() {
    let mut results = results(2);
    let mut browser = Browser::new(&results);
    let empty_history = plain_frame(&mut browser, &results, 120, 36).join("\n");
    assert!(
        !empty_history.contains("PREVIOUS"),
        "loaded but empty history needs no column"
    );
    let mut previous = result_check(
        9,
        super_types::CheckOutcome::Fail,
        super_types::CheckAssurance::Uncertified,
    );
    previous.revision.reported_git_commit = Some("a".repeat(40));
    results.rows[0].previous = Some(previous);
    for (width, height) in [(60, 24), (80, 24), (99, 36), (100, 36), (120, 36)] {
        let frame = plain_frame(&mut browser, &results, width, height);
        assert!(frame.iter().all(|line| measure_text_width(line) <= width));
        let heading = frame.iter().find(|line| line.contains("SUPERTEST")).unwrap();
        let row = frame.iter().find(|line| line.contains("law_1")).unwrap();
        assert!(heading.contains("SUPERTEST · STATUS"));
        assert!(row.contains("law_1     · checking · reviewing"));
        let column = |line: &str, text: &str| measure_text_width(line.split_once(text).unwrap().0);
        assert_eq!(column(heading, "STATUS"), column(row, "checking"));
        assert!(!row.contains("#1"), "the check number belongs in its pane");
        assert_eq!(heading.contains("PREVIOUS"), width >= 100);
        assert_eq!(row.contains("fail aaaaaaa"), width >= 100);
        if width >= 100 {
            assert_eq!(column(heading, "PREVIOUS"), column(row, "fail aaaaaaa"));
        }
        let text = frame.join("\n");
        assert_eq!(text.matches("#1").count(), 1);
        assert_eq!(text.matches("supertests/sequences.py:3").count(), 1);
    }
}

#[test]
fn escape_goes_back_only_after_entering_details_ctrl_c_closes_and_q_is_ignored() {
    let results = results(2);
    let mut browser = Browser::new(&results);
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!browser.closes(q));
    assert!(browser.closes(ctrl_c));
    assert!(browser.closes(escape));
    browser.key(KeyCode::Down, &results, 36, 120);
    browser.key(KeyCode::Enter, &results, 36, 120);
    assert!(!browser.closes(escape));
    assert!(browser.closes(ctrl_c));
    assert!(!browser.closes(q));
    for (width, height) in [(40, 15), (80, 24), (120, 36)] {
        let frame = plain_frame(&mut browser, &results, width, height).join("\n");
        assert!(frame.contains("Esc back"));
        assert!(frame.contains("Ctrl+C detach"));
        assert!(!frame.contains("Esc/Ctrl+C detach"));
    }
    browser.key(KeyCode::Esc, &results, 36, 120);
    assert_eq!(browser.mode, Mode::List);
    assert_eq!(browser.selected(&results).unwrap().number, 2);
    assert!(browser.closes(escape));
    browser.configure(false, true);
    assert!(browser.closes(escape), "a directly opened result is the outermost view");
    assert!(browser.closes(ctrl_c));
    assert!(!browser.closes(q));
    let frame = plain_frame(&mut browser, &results, 80, 24).join("\n");
    assert!(frame.contains("Esc/Ctrl+C detach"));
}

#[test]
fn focused_check_without_history_has_one_identity_and_no_empty_history_controls() {
    let mut results = results(1);
    results.rows[0].current = Some(normalization_check(42));
    let mut browser = Browser::new(&results);
    browser.configure(false, true);
    let frame = plain_frame(&mut browser, &results, 120, 36).join("\n");
    assert_eq!(frame.matches("normalizing_twice_changes_nothing").count(), 1);
    assert_eq!(frame.matches("#42").count(), 1);
    assert!(frame.contains("reproduce"));
    for absent in [
        "SUPERTEST",
        "Check history",
        "No earlier checks",
        "↑/↓ attempts",
        "Esc back",
        "PgUp/PgDn",
    ] {
        assert!(!frame.contains(absent), "unexpected {absent}: {frame}");
    }
}

#[test]
fn selected_result_keeps_explanation_and_evidence_and_expands_reproduction() {
    let mut results = results(2);
    for assurance in [
        super_types::CheckAssurance::Uncertified,
        super_types::CheckAssurance::Certified,
    ] {
        let mut passed = result_check(1, super_types::CheckOutcome::Pass, assurance);
        passed.presentation.details.clear();
        results.rows[0].current = Some(passed);
        let frame = plain_frame(&mut Browser::new(&results), &results, 120, 36).join("\n");
        assert!(frame.contains(if assurance == super_types::CheckAssurance::Certified {
            "A certificate was accepted for this result."
        } else {
            "No problems found within this supertest."
        }));
    }
    let mut blocked = check(1);
    blocked.terminal = true;
    blocked.operational_error = Some(super_types::CheckOperationalError::Blocked);
    blocked.presentation.details.clear();
    results.rows[0].current = Some(blocked);
    let frame = plain_frame(&mut Browser::new(&results), &results, 120, 36).join("\n");
    assert!(frame.contains("The check could not finish. No further reason was reported."));

    results.rows[0].current = Some(normalization_check(42));
    let mut browser = Browser::new(&results);
    let frame = plain_frame(&mut browser, &results, 120, 36).join("\n");
    assert!(frame.contains("text = \"a   b\""));
    assert!(!frame.contains("reproduce"));
    browser.key(KeyCode::Enter, &results, 36, 120);
    let expanded = plain_frame(&mut browser, &results, 120, 36).join("\n");
    assert!(expanded.contains("reproduce"));
}

#[test]
fn history_pages_do_not_replace_the_selected_check() {
    let mut results = results(1);
    let current = results.rows[0].current.clone().unwrap();
    results.head = Some(super_types::CommitRef {
        oid: current.revision.reported_git_commit.clone().unwrap(),
        parent_oid: None,
        branch: None,
        temporary: false,
    });
    results
        .append_history(super_types::CheckHistoryPage {
            checks: vec![current.clone()],
            next_before: None,
        })
        .unwrap();
    assert_eq!(results.rows[0].current.as_ref().unwrap().number, current.number);
    let mut newer = current.clone();
    newer.number = 200;
    newer.created_at += chrono::Duration::days(1);
    results
        .append_history(super_types::CheckHistoryPage {
            checks: vec![newer],
            next_before: None,
        })
        .unwrap();
    assert_eq!(results.rows[0].current.as_ref().unwrap().number, current.number);
}

#[test]
fn live_evidence_can_grow_without_moving_source_identity() {
    let mut results = results(1);
    let mut browser = Browser::new(&results);
    let before = plain_frame(&mut browser, &results, 80, 24);
    results.rows[0]
        .current
        .as_mut()
        .unwrap()
        .presentation
        .details
        .push(super_types::PresentationLine {
            text: "More evidence. ".repeat(500),
            emphasized: false,
        });
    let after = plain_frame(&mut browser, &results, 80, 24);
    assert_eq!(before.len(), after.len());
    page_to_end(&mut browser, &results, 80, 24);
    let scrolled = plain_frame(&mut browser, &results, 80, 24);
    assert!(scrolled[1].contains("bbbbbbb"));
    assert!(scrolled[1].contains("#1"));
    assert_eq!(&before[..2], &scrolled[..2]);
}

#[test]
fn an_unchecked_current_commit_still_opens_its_prior_evidence() {
    let mut results = results(2);
    let previous = results.rows[0].current.take().unwrap();
    results.history.push(previous.clone());
    let mut browser = Browser::new(&results);
    browser.key(KeyCode::Enter, &results, 24, 80);
    assert_eq!(browser.selected(&results).unwrap().number, previous.number);
    results.rows.truncate(1);
    results.head = Some(super_types::CommitRef {
        oid: "c".repeat(40),
        parent_oid: None,
        branch: None,
        temporary: false,
    });
    let mut browser = Browser::new(&results);
    assert_eq!(browser.selected(&results).unwrap().number, previous.number);
    let frame = plain_frame(&mut browser, &results, 80, 24).join("\n");
    assert!(frame.contains("bbbbbbb"), "identify the checked commit");
    assert!(!frame.contains("different commit"));
}

#[test]
fn inspecting_an_unchecked_row_keeps_its_own_name_and_location() {
    let mut results = results(2);
    results.rows[1].current = None;
    results.rows[1].supertest.path = "supertests/other.py".into();
    let mut browser = Browser::new(&results);
    browser.key(KeyCode::Down, &results, 24, 80);
    browser.key(KeyCode::Enter, &results, 24, 80);
    let frame = plain_frame(&mut browser, &results, 80, 24).join("\n");
    assert!(frame.contains("law_2") && frame.contains("supertests/other.py:3"));
    assert!(frame.contains("Not checked at the current commit."));
    assert!(!frame.contains("law_1") && !frame.contains("No earlier checks."));
}

fn plain_frame(browser: &mut Browser, results: &Results, width: usize, height: usize) -> Vec<String> {
    browser
        .frame(results, width, height, false)
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect()
}

#[test]
fn both_terminal_sizes_bound_the_table_and_keep_details_above_controls() {
    let results = results(48);
    for (width, height) in [(80, 24), (120, 36)] {
        let mut browser = Browser::new(&results);
        let lines = plain_frame(&mut browser, &results, width, height);
        assert_eq!(lines.len(), height);
        assert!(lines.iter().all(|line| measure_text_width(line) <= width));
        assert!(lines[0].contains("supertest-examples-bad"));
        let activity = lines
            .iter()
            .position(|line| line.contains("Reviewing the proposed"))
            .unwrap();
        let controls = lines
            .iter()
            .position(|line| line.contains("Enter for more details"))
            .unwrap();
        assert!(activity < controls);
        assert!(lines[controls..].iter().any(|line| line.contains("Esc/Ctrl+C detach")));
        assert!(!lines.join("\n").contains("00000000"));
        assert_eq!(lines.join("\n").matches("Reviewing the proposed").count(), 1);
        for _ in 1..results.rows.len() {
            browser.key(KeyCode::Down, &results, height, width);
        }
        let last = plain_frame(&mut browser, &results, width, height).join("\n");
        assert!(last.contains("law_48"));
        assert!(last.contains("of 48"));
        assert!(last.contains("Elapsed"));
    }
}

#[test]
fn single_check_shows_history_immediately_and_expands_the_selected_attempt() {
    let mut results = results(1);
    results.rows[0].current = Some(normalization_check(1));
    results.rows[0].supertest = results.rows[0].current.as_ref().unwrap().supertest.clone();
    let mut earlier = normalization_check(2);
    earlier.supertest = results.rows[0].supertest.clone();
    earlier.created_at = chrono::DateTime::from_timestamp(0, 0).unwrap();
    results.history.push(earlier.clone());
    let mut browser = Browser::new(&results);
    assert_eq!(browser.mode, Mode::List);
    let frame = plain_frame(&mut browser, &results, 120, 36).join("\n");
    assert!(frame.contains("#1") && frame.contains("#2"));
    assert!(frame.contains("↑/↓ attempts") && frame.contains("Enter for more details"));
    assert!(!frame.contains("reproduce"));
    browser.key(KeyCode::Down, &results, 36, 120);
    assert_eq!(browser.selected(&results).unwrap().number, earlier.number);
    browser.key(KeyCode::Enter, &results, 24, 80);
    assert_eq!(browser.mode, Mode::Inspect);
    assert_eq!(browser.selected(&results).unwrap().number, earlier.number);
    let frame = plain_frame(&mut browser, &results, 120, 36).join("\n");
    assert!(frame.contains("reproduce"));
    assert!(frame.contains("↑/↓ attempts"));
    browser.key(KeyCode::Esc, &results, 24, 80);
    assert_eq!(browser.mode, Mode::List);
    assert_eq!(browser.selected(&results).unwrap().number, earlier.number);
    browser.key(KeyCode::Up, &results, 36, 120);
    assert_eq!(browser.selected(&results).unwrap().number, 1);
}

#[test]
fn history_columns_align_mixed_results_and_date_every_attempt() {
    console::set_colors_enabled(true);
    let mut results = results(1);
    let mut current = result_check(
        10000,
        super_types::CheckOutcome::Fail,
        super_types::CheckAssurance::Uncertified,
    );
    current.created_at = Utc::now() - chrono::Duration::minutes(5) - chrono::Duration::seconds(15);
    let mut passed = current.clone();
    passed.number = 99;
    passed.created_at -= chrono::Duration::minutes(10);
    passed.result.as_mut().unwrap().outcome = super_types::CheckOutcome::Pass;
    passed.result.as_mut().unwrap().assurance = super_types::CheckAssurance::Certified;
    let mut blocked = current.clone();
    blocked.number = 1;
    blocked.created_at -= chrono::Duration::minutes(20);
    blocked.result = None;
    blocked.operational_error = Some(super_types::CheckOperationalError::Blocked);
    blocked.revision.reported_git_commit = None;
    results.rows[0].supertest = current.supertest.clone();
    results.rows[0].current = Some(current);
    results.history = vec![passed, blocked];
    results.head = Some(super_types::CommitRef {
        oid: "c".repeat(40),
        parent_oid: None,
        branch: None,
        temporary: false,
    });
    for (width, height) in [(60, 20), (80, 24), (120, 36)] {
        let mut browser = Browser::new(&results);
        let frame = plain_frame(&mut browser, &results, width, height);
        assert!(frame.iter().all(|line| measure_text_width(line) <= width));
        let header = frame.iter().position(|line| line.contains("REQUESTED")).unwrap();
        assert!(frame[header].contains("COMMIT"));
        assert!(frame[header - 1].contains("Check history"));
        assert!(!frame[header + 3].contains("aaaaaaa"), "a tree hash is not a commit");
        assert!(frame[header + 3].contains('—'));
        let separators = |line: &str| {
            line.match_indices('·')
                .map(|(index, _)| measure_text_width(&line[..index]))
                .collect::<Vec<_>>()
        };
        let positions = separators(&frame[header]);
        assert_eq!(positions.len(), 3);
        for (line, number, age) in [
            (&frame[header + 1], "#10000", "5m ago"),
            (&frame[header + 2], "#99", "15m ago"),
            (&frame[header + 3], "#1", "25m ago"),
        ] {
            assert_eq!(separators(line), positions, "{line}");
            assert!(line.contains(number) && line.contains(age), "{line}");
            assert!(!line.contains("current") && !line.contains("different commit"));
        }
        let snapshot = lines_text(&presentation::snapshot(&results, true, false, width));
        assert!(snapshot.contains("#10000 · 5m ago"));
        assert!(!snapshot.contains("current") && !snapshot.contains("different commit"));
        for (key, selected, unselected) in [(None, "#10000", "#99"), (Some(KeyCode::Down), "#99", "#10000")] {
            if let Some(key) = key {
                browser.key(key, &results, height, width);
            }
            let frame = browser.frame(&results, width, height, false);
            let row = frame.iter().find(|line| line.contains(selected)).unwrap();
            for metadata in [selected, "bbbbbbb", "ago", "·"] {
                assert_eq!(
                    text_style(row, metadata).foreground,
                    None,
                    "selected metadata: {metadata}"
                );
            }
            let (label, color) = if selected == "#10000" {
                ("fail", (243, 161, 161))
            } else {
                ("pass", (149, 212, 173))
            };
            assert_eq!(text_style(row, label).foreground, Some(color));
            assert!(
                ansi_cells(row)
                    .iter()
                    .filter(|(c, _)| !c.is_whitespace())
                    .all(|(_, style)| style.bold)
            );
            let other = frame.iter().find(|line| line.contains(unselected)).unwrap();
            assert!(ansi_cells(other).iter().all(|(_, style)| !style.bold));
            assert_eq!(text_style(other, unselected).foreground, Some((176, 182, 197)));
        }
    }
}

#[test]
fn history_uses_creation_time_not_number_order_and_matches_declarations_not_line_numbers() {
    let mut results = results(1);
    let current = results.rows[0].current.as_ref().unwrap().clone();
    let mut earlier = check(999);
    earlier.supertest = current.supertest.clone();
    earlier.supertest.line = Some(999);
    earlier.created_at = current.created_at - chrono::Duration::days(1);
    let mut newer = check(3);
    newer.supertest = current.supertest.clone();
    newer.created_at = current.created_at + chrono::Duration::days(1);
    results.history = vec![earlier.clone(), newer.clone(), current.clone()];
    results.refresh_previous();
    assert_eq!(results.rows[0].previous.as_ref().unwrap().number, earlier.number);
    assert_eq!(
        results.attempts(0).iter().map(|c| c.number).collect::<Vec<_>>(),
        [newer.number, current.number, earlier.number]
    );
}

#[test]
fn events_do_not_move_selection_or_accept_stale_cross_workspace_updates() {
    let mut results = results(3);
    let mut browser = Browser::new(&results);
    browser.key(KeyCode::Down, &results, 24, 80);
    let selected = browser.selected(&results).unwrap().number;
    let mut event = results.rows[1].current.clone().unwrap();
    event.event_sequence = 11;
    event.presentation.activity_label = Some("verifying".into());
    assert!(results.update(&event));
    assert_eq!(browser.selected(&results).unwrap().number, selected);
    event.event_sequence = 1;
    event.presentation.activity_label = Some("starting".into());
    assert!(!results.update(&event));
    event.event_sequence = 12;
    event.repository_id = Uuid::from_u128(123);
    assert!(!results.update(&event));
    assert_eq!(
        browser
            .selected(&results)
            .unwrap()
            .presentation
            .activity_label
            .as_deref(),
        Some("verifying")
    );
}

#[test]
fn full_evidence_remains_accessible_and_ansi_does_not_break_assertions() {
    let mut results = results(1);
    let check = results.rows[0].current.as_mut().unwrap();
    let witness = "界".repeat(300);
    check.presentation.details.push(super_types::PresentationLine {
        text: format!("Witness: {witness}\nExpected: 6\nObserved: 0\nReproduce: test(-42)"),
        emphasized: false,
    });
    let all = evidence(check, EvidenceView::Full, false)
        .iter()
        .map(Line::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains(&witness));
    assert!(all.contains("Reproduce: test(-42)"));
    let mut browser = Browser::new(&results);
    page_to_end(&mut browser, &results, 80, 24);
    let lines = plain_frame(&mut browser, &results, 80, 24);
    assert!(lines.join("\n").contains("Reproduce: test(-42)"));
    assert!(lines.iter().all(|line| measure_text_width(line) <= 80));
    assert_eq!(console::strip_ansi_codes("\x1b[31mExpected: 6\x1b[0m"), "Expected: 6");
    assert!(!clean("\x1b]52;c;secret\x07\rINJECT").contains(['\x1b', '\x07', '\r']));
}

#[test]
fn check_exit_codes_separate_findings_from_operational_failure() {
    let mut results = results(1);
    assert_eq!(results.exit_code(), 0);
    results.rows[0].current.as_mut().unwrap().problematic = true;
    assert_eq!(results.exit_code(), 1);
    results.rows[0].current.as_mut().unwrap().operational_error = Some(super_types::CheckOperationalError::Error);
    assert_eq!(results.exit_code(), 2);
}

#[test]
fn small_empty_and_short_views_clear_unused_rows_without_moving_content() {
    for count in [0, 1, 2] {
        let results = results(count);
        let mut browser = Browser::new(&results);
        for (width, height) in [(30, 8), (80, 24), (120, 36)] {
            let frame = plain_frame(&mut browser, &results, width, height);
            assert_eq!(frame.len(), height);
            assert!(frame.iter().all(|line| measure_text_width(line) <= width));
            if count == 0 && width >= 60 {
                let footer = frame.iter().position(|line| line.contains("close")).unwrap();
                assert!(frame[footer + 1..].iter().all(|line| line.trim().is_empty()));
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CellStyle {
    foreground: Option<(u8, u8, u8)>,
    background: Option<(u8, u8, u8)>,
    bold: bool,
    italic: bool,
}

// Read emitted SGR rather than stripping it: the design depends on independently styled cells.
fn ansi_cells(line: &str) -> Vec<(char, CellStyle)> {
    let expression = regex::Regex::new("\u{1b}\\[([0-9;]*)m").unwrap();
    let mut style = CellStyle::default();
    let mut cells = Vec::new();
    let mut offset = 0;
    for escape in expression.captures_iter(line) {
        let full = escape.get(0).unwrap();
        cells.extend(line[offset..full.start()].chars().map(|c| (c, style.clone())));
        let values: Vec<u16> = escape[1].split(';').map(|part| part.parse().unwrap_or(0)).collect();
        let mut index = 0;
        while index < values.len() {
            match values[index] {
                0 => style = CellStyle::default(),
                1 => style.bold = true,
                3 => style.italic = true,
                22 => style.bold = false,
                23 => style.italic = false,
                39 => style.foreground = None,
                49 => style.background = None,
                38 | 48 if values.get(index + 1) == Some(&2) => {
                    let color = Some((
                        u8::try_from(values[index + 2]).unwrap(),
                        u8::try_from(values[index + 3]).unwrap(),
                        u8::try_from(values[index + 4]).unwrap(),
                    ));
                    if values[index] == 38 {
                        style.foreground = color;
                    } else {
                        style.background = color;
                    }
                    index += 4;
                }
                _ => {}
            }
            index += 1;
        }
        offset = full.end();
    }
    cells.extend(line[offset..].chars().map(|c| (c, style.clone())));
    cells
}

fn text_style(line: &str, text: &str) -> CellStyle {
    let cells = ansi_cells(line);
    let needle: Vec<_> = text.chars().collect();
    let start = cells
        .windows(needle.len())
        .position(|window| window.iter().map(|(c, _)| c).eq(needle.iter()))
        .unwrap();
    cells[start].1.clone()
}

fn result_check(number: u64, outcome: super_types::CheckOutcome, assurance: super_types::CheckAssurance) -> Check {
    let mut check = check(number);
    check.terminal = true;
    check.result = Some(super_types::CheckResult { outcome, assurance });
    check.problematic = outcome == super_types::CheckOutcome::Fail;
    // Deliberately stale server presentation: clients must still color actual failures correctly.
    check.presentation.status.label = "fail (certifying…)".into();
    check.presentation.status.tone = super_types::PresentationTone::Warning;
    check
}

#[tokio::test]
async fn noninteractive_observation_finishes_at_verdict_with_optional_updates_pending() {
    let mut results = results(1);
    let mut verdict = result_check(
        1,
        super_types::CheckOutcome::Fail,
        super_types::CheckAssurance::Uncertified,
    );
    verdict.updates_pending = true;
    verdict.fix_pending = true;
    results.rows[0].current = Some(verdict);
    let client = PupClient::new("http://127.0.0.1:1", None).unwrap();
    let ui = Ui {
        json: false,
        interactive: false,
        details: false,
        release_alert: None,
    };
    assert!(results.finished());
    assert!(
        !observe(&client, &mut results, Observation::Attached, &ui)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn result_watch_reconnects_after_verdict_and_receives_later_explanation() {
    use std::io::{BufRead, BufReader};

    let mut results = results(1);
    let mut verdict = result_check(
        1,
        super_types::CheckOutcome::Fail,
        super_types::CheckAssurance::Uncertified,
    );
    verdict.updates_pending = true;
    results.rows[0].current = Some(verdict.clone());
    let mut explained = verdict.clone();
    explained.updates_pending = false;
    explained.event_sequence += 1;
    explained.presentation.details.push(super_types::PresentationLine {
        text: "The zero branch returns one.".into(),
        emphasized: false,
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = PupClient::new(&format!("http://{}", listener.local_addr().unwrap()), None).unwrap();
    let snapshots = [verdict.clone(), explained.clone()];
    let server = std::thread::spawn(move || {
        for check in snapshots {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            let body = format!(
                "event: check\ndata: {}\n\n",
                serde_json::to_string(&super_types::CheckEvent {
                    sequence: check.event_sequence,
                    check,
                })
                .unwrap()
            );
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let (sender, mut receiver) = mpsc::channel(8);
    let mut tasks = JoinSet::new();
    spawn_watches(&client, &results, &sender, &mut tasks, &mut HashSet::new(), &mut 0).unwrap();
    drop(sender);
    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        let mut observed = Vec::new();
        while let Some(message) = receiver.recv().await {
            match message {
                StreamMessage::Check(event) => observed.push(event.check),
                StreamMessage::Error(error) => panic!("watch failed: {error}"),
                StreamMessage::Connection(_, _) => {}
            }
        }
        observed
    })
    .await
    .unwrap();
    assert_eq!(observed, [verdict, explained]);
    while let Some(task) = tasks.join_next().await {
        task.unwrap();
    }
    server.join().unwrap();
}

#[test]
fn emitted_colors_bold_panel_padding_and_gutters_match_the_mock_palette() {
    console::set_colors_enabled(true);
    let mut results = results(2);
    results.rows[0].current = Some(result_check(
        1,
        super_types::CheckOutcome::Pass,
        super_types::CheckAssurance::Uncertified,
    ));
    results.rows[1].current = Some(result_check(
        2,
        super_types::CheckOutcome::Fail,
        super_types::CheckAssurance::Uncertified,
    ));
    for (width, height) in [(80, 24), (120, 36)] {
        let mut browser = Browser::new(&results);
        let frame = browser.frame(&results, width, height, false);
        for line in &frame {
            let cells = ansi_cells(line);
            assert_eq!(cells.len(), width);
            assert_eq!(cells[0].1.background, None);
            assert_eq!(cells[width - 1].1.background, None);
            if cells.iter().all(|(c, _)| *c == ' ') && !cells.iter().any(|(_, style)| style.background.is_some()) {
                assert!(cells.iter().all(|(_, style)| *style == CellStyle::default()));
            }
        }
        assert!(text_style(&frame[0], "supertest-examples-bad").bold);
        let row = frame.iter().find(|line| line.contains("law_1")).unwrap();
        assert_eq!(
            text_style(row, "law_1"),
            CellStyle {
                foreground: None,
                background: Some(style::SELECTION),
                bold: true,
                italic: false,
            }
        );
        assert_eq!(text_style(row, "pass").foreground, Some((149, 212, 173)));
        assert!(
            ansi_cells(row)
                .iter()
                .filter(|(c, _)| !c.is_whitespace())
                .all(|(_, style)| style.bold)
        );
        let row = frame.iter().find(|line| line.contains("law_2")).unwrap();
        assert_eq!(text_style(row, "law_2").foreground, None);
        assert_eq!(text_style(row, "fail").foreground, Some((243, 161, 161)));
        assert!(ansi_cells(row).iter().all(|(_, style)| !style.bold));
        assert_panel_padding(&frame, width);
        let rows: Vec<_> = presentation::snapshot(&results, false, false, width)
            .iter()
            .map(|line| line.render(width, 2))
            .collect();
        assert!(
            rows.iter()
                .flat_map(|line| ansi_cells(line))
                .all(|(_, style)| style.background.is_none())
        );
        assert!(
            rows.iter()
                .any(|line| line.contains("pass") && line.contains("38;2;149;212;173"))
        );
        assert!(
            rows.iter()
                .any(|line| line.contains("fail") && line.contains("38;2;243;161;161"))
        );
    }
}

fn assert_panel_padding(frame: &[String], width: usize) {
    let panel: Vec<_> = frame
        .iter()
        .map(|line| ansi_cells(line))
        .filter(|cells| cells[1].1.background == Some(style::PANEL))
        .collect();
    assert!(panel.len() >= 3);
    for cells in &panel {
        for x in [1, 2, width - 3, width - 2] {
            assert_eq!(cells[x].0, ' ', "two shaded spaces inside each panel edge");
            assert_eq!(cells[x].1.background, Some(style::PANEL));
        }
    }
    for cells in [panel.first().unwrap(), panel.last().unwrap()] {
        assert!(cells.iter().all(|(c, _)| *c == ' '));
        assert!(
            cells[1..width - 1]
                .iter()
                .all(|(_, style)| style.background == Some(style::PANEL))
        );
    }
}

#[test]
fn one_queue_state_and_consistent_semantics_cover_every_result() {
    let mut active = check(1);
    active.presentation.activity_label = Some("queued".into());
    assert_eq!(activity(&active), "checking");
    active.presentation.status.label = "queued".into();
    assert_eq!(activity(&active), "queued");
    for assurance in [
        super_types::CheckAssurance::Uncertified,
        super_types::CheckAssurance::Certified,
    ] {
        let failed = result_check(1, super_types::CheckOutcome::Fail, assurance);
        assert_eq!(status(&failed).ink, Ink::Fail);
        assert_eq!(status(&failed).marker, "×");
        assert!(!status(&failed).label.contains("certifying"));
    }
    for (label, ink) in [
        ("checking", Ink::Accent),
        ("queued", Ink::Muted),
        ("pending", Ink::Muted),
    ] {
        active.presentation.status.label = label.into();
        active.operational_error = match label {
            "canceled" => Some(super_types::CheckOperationalError::Canceled),
            "blocked" => Some(super_types::CheckOperationalError::Blocked),
            "error" => Some(super_types::CheckOperationalError::Error),
            _ => None,
        };
        active.result = (label == "conditional").then_some(super_types::CheckResult {
            outcome: super_types::CheckOutcome::Conditional,
            assurance: super_types::CheckAssurance::Uncertified,
        });
        assert_eq!(status(&active).ink, ink, "{label}");
    }
    for (error, ink) in [
        (super_types::CheckOperationalError::Canceled, Ink::Muted),
        (super_types::CheckOperationalError::Blocked, Ink::Warning),
        (super_types::CheckOperationalError::Error, Ink::Fail),
        (super_types::CheckOperationalError::MissingConclusion, Ink::Fail),
    ] {
        active.operational_error = Some(error);
        assert_eq!(status(&active).ink, ink);
    }
}

#[test]
fn result_semantics_do_not_depend_on_labels_tones_or_explanations() {
    use super_types::{CheckAssurance, CheckOutcome, PresentationTone};

    for outcome in [CheckOutcome::Pass, CheckOutcome::Fail, CheckOutcome::Conditional] {
        for assurance in [CheckAssurance::Uncertified, CheckAssurance::Certified] {
            let mut results = results(1);
            let current = result_check(1, outcome, assurance);
            let expected = status(&current);
            results.rows[0].current = Some(current);
            let expected_summary = presentation::summary(&results);
            let expected_exit = results.exit_code();
            for label in [
                "pass (certified)",
                "fail (certified)",
                "conditional",
                "blocked",
                "queued",
                "arbitrary",
            ] {
                let check = results.rows[0].current.as_mut().unwrap();
                check.presentation.status.label = label.into();
                check.presentation.status.marker = "incorrect marker".into();
                check.presentation.status.tone = PresentationTone::Success;
                check.presentation.details = vec![super_types::PresentationLine {
                    text: format!("Conclusion: {label}. This prose must not decide the result."),
                    emphasized: false,
                }];
                let actual = status(check);
                assert_eq!(
                    (actual.label, actual.marker, actual.ink),
                    (expected.label.clone(), expected.marker, expected.ink)
                );
                assert_eq!(presentation::is_pass(check), outcome == CheckOutcome::Pass);
                assert_eq!(presentation::summary(&results), expected_summary);
                assert_eq!(results.exit_code(), expected_exit);
                assert!(results.finished());
            }
        }
    }
}

#[test]
fn labels_cannot_supply_a_missing_verdict_or_certification() {
    let mut check = check(1);
    for label in ["pass", "pass (certified)", "fail (certified)", "conditional", "blocked"] {
        check.presentation.status.label = label.into();
        check.presentation.status.tone = super_types::PresentationTone::Success;
        check.terminal = false;
        assert_eq!(status(&check).label, "checking");
        check.terminal = true;
        assert_eq!(status(&check).label, "result unavailable");
        assert!(!presentation::is_pass(&check));
    }
}

#[test]
fn counterexample_certification_comes_from_the_result_not_its_heading() {
    let mut check = normalization_check(42);
    check.presentation.details[8].text = "Certified counterexample".into();
    assert!(!lines_text(&evidence(&check, EvidenceView::Summary, false)).contains("Certified counterexample"));
    check.presentation.details[8].text = "Counterexample".into();
    check.result.as_mut().unwrap().assurance = super_types::CheckAssurance::Certified;
    assert!(lines_text(&evidence(&check, EvidenceView::Summary, false)).contains("Certified counterexample"));
}

#[test]
fn single_check_has_a_live_marker_timing_and_symmetric_panel() {
    console::set_colors_enabled(true);
    let mut results = results(1);
    results.rows[0].current.as_mut().unwrap().presentation.status.label = "queued".into();
    results.rows[0].current.as_mut().unwrap().presentation.live_line = None;
    for (width, height) in [(80, 24), (120, 36)] {
        let mut browser = Browser::new(&results);
        let before = browser.frame(&results, width, height, false);
        browser.tick = 1;
        let after = browser.frame(&results, width, height, false);
        assert!(before[0].contains('⠋') && after[0].contains('⠙'));
        assert!(before.iter().any(|line| line.contains("Elapsed")));
        assert!(before.iter().any(|line| line.contains("last update")));
        assert!(!before.iter().any(|line| line.contains("ACTIVITY")));
        assert_panel_padding(&before, width);
    }
}

#[test]
fn evidence_preserves_headings_paragraphs_and_continuation_indentation() {
    let mut check = result_check(
        1,
        super_types::CheckOutcome::Fail,
        super_types::CheckAssurance::Uncertified,
    );
    check.presentation.details = vec![
        super_types::PresentationLine {
            text: "Conclusion".into(),
            emphasized: true,
        },
        super_types::PresentationLine {
            text: "  a long explanation that wraps across several lines\n\n  expected = 6\n  observed = 0".into(),
            emphasized: false,
        },
    ];
    let lines: Vec<_> = evidence(&check, EvidenceView::Full, false)
        .iter()
        .flat_map(|line| line.wrapped(24))
        .collect();
    assert!(lines[0].parts.iter().any(|(_, _, bold)| *bold));
    assert!(lines[2].text().starts_with("  "));
    assert!(lines.iter().any(|line| line.text().is_empty()));
    assert_eq!(
        clean("error\n  Run super link\tthen retry"),
        "error\n  Run super link        then retry"
    );
    assert!(!clean("safe\x1b[31mred\x1b[0m\x1b]52;c;data\x07").contains('\x1b'));
}

#[test]
fn diff_colors_whole_changed_lines_but_bolds_only_changed_fragments() {
    let patch = "diff --git a/source.py b/source.py\n@@ -1,2 +1,2 @@\n # unchanged\n-    counts[item] = +1\n+    counts[item] = counts.get(item, 0) + 1\n";
    for width in [40, 80, 120] {
        let rows = diff::patch(patch, width);
        for (background, ink, changed) in [
            (style::REMOVED, Ink::RemovedText, "+"),
            (style::ADDED, Ink::AddedText, "counts.get(item, 0) + "),
        ] {
            let spans: Vec<_> = rows
                .iter()
                .filter(|line| line.background == Some(background))
                .flat_map(|line| line.parts.iter().filter(|(_, color, _)| *color == ink))
                .collect();
            let code: String = spans.iter().map(|(text, _, _)| text.as_str()).collect();
            let bold: String = spans
                .iter()
                .filter(|(_, _, bold)| *bold)
                .map(|(text, _, _)| text.as_str())
                .collect();
            assert!(code.starts_with("    counts[item] = "), "{code:?}");
            assert!(code.ends_with('1'), "{code:?}");
            assert_eq!(bold, changed);
        }
        let context = rows.iter().find(|row| row.text().contains("# unchanged")).unwrap();
        assert_eq!(context.background, None);
        assert!(context.parts.iter().all(|(_, _, bold)| !bold));
    }
}

#[test]
fn diff_does_not_reuse_a_removed_line_to_hide_new_lines() {
    let patch = "diff --git a/text.py b/text.py\n@@ -1 +1,3 @@\n-    return text.replace(\"  \", \" \")\n+    while \"  \" in text:\n+        text = text.replace(\"  \", \" \")\n+    return text\n";
    let rows = diff::patch(patch, 100);
    for added in [true, false] {
        // The same rule applies to both insertions and deletions in a changed block.
        let reversed = patch
            .lines()
            .map(|line| {
                if let Some(code) = line.strip_prefix('-') {
                    format!("+{code}")
                } else if let Some(code) = line.strip_prefix('+') {
                    format!("-{code}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let reverse_rows = diff::patch(&reversed, 100);
        let lines = if added { &rows } else { &reverse_rows };
        let ink = if added { Ink::AddedText } else { Ink::RemovedText };
        let returned = lines
            .iter()
            .find(|line| line.text().ends_with("    return text"))
            .unwrap();
        let emphasized: String = returned
            .parts
            .iter()
            .filter(|(_, color, bold)| *color == ink && *bold)
            .map(|(text, _, _)| text.as_str())
            .collect();
        assert_eq!(emphasized.trim(), "return text");
        let assignment = lines
            .iter()
            .find(|line| line.text().contains("text = text.replace"))
            .unwrap();
        assert!(
            assignment
                .parts
                .iter()
                .any(|(text, color, bold)| *color == ink && !*bold && text.contains(".replace"))
        );
    }
}

#[test]
fn diff_has_old_new_line_numbers_and_preserves_code_indentation() {
    console::set_colors_enabled(true);
    let patch = "diff --git a/source.py b/source.py\n--- a/source.py\n+++ b/source.py\n@@ -42,2 +42,3 @@\n \ttotal = 0\n-\tvalue //= 10\n+\tmagnitude = abs(value)\n+\tmagnitude //= 10\n";
    for width in [80, 120] {
        let rows = diff::patch(patch, width);
        assert_eq!(rows[0].text(), "source.py  +2 -1");
        let removed = rows.iter().find(|line| line.text().contains("value //=")).unwrap();
        let added = rows.iter().find(|line| line.text().contains("magnitude //=")).unwrap();
        assert_eq!(removed.text(), " 43     -         value //= 10");
        assert_eq!(added.text(), "     44 +         magnitude //= 10");
        for (line, bg) in [(removed, (72, 42, 43)), (added, (36, 60, 45))] {
            let cells = ansi_cells(&line.render(width, 2));
            assert_eq!(cells[0].1.background, None);
            assert_eq!(cells[width - 1].1.background, None);
            assert!(
                cells[1..width - 1]
                    .iter()
                    .all(|(_, style)| style.background == Some(bg))
            );
        }
        assert!(text_style(&added.render(width, 2), "magnitude").bold);
    }
    let patch = "diff --git a/a.py b/a.py\n@@ -999,2 +999,2 @@\n before\n-after\n+replacement\n";
    let rows = diff::patch(patch, 80);
    let before = rows.iter().find(|line| line.text().contains("before")).unwrap().text();
    let after = rows.iter().find(|line| line.text().contains("after")).unwrap().text();
    assert_eq!(
        before.find("before"),
        after.find("after"),
        "number width changed in the middle of a hunk"
    );
}

#[test]
fn upload_progress_is_byte_weighted_left_aligned_and_stage_specific() {
    let rows = progress::frame(
        &crate::api::SourceProgress::Uploading {
            completed: 1_800_000,
            total: 2_600_000,
        },
        Duration::from_secs(12),
        0,
    );
    assert_eq!(rows[2].text(), "━━━━━━━━━━━━━━──────  69%");
    assert!(rows[0].text().contains("Syncing changes"));
    assert_eq!(rows[3].text(), "1.8 of 2.6 MB synced");
    assert!(rows[2].render(80, 2).starts_with("  "));
    for state in [
        crate::api::SourceProgress::Preparing,
        crate::api::SourceProgress::Finalizing,
        crate::api::SourceProgress::Uploading { completed: 0, total: 0 },
    ] {
        assert!(
            progress::frame(&state, Duration::ZERO, 0)
                .iter()
                .all(|line| !line.text().contains('%'))
        );
    }
}

#[test]
fn a_single_selector_selects_the_attempt_when_background_history_arrives() {
    let mut results = results(1);
    let current = results.rows[0].current.take().unwrap();
    results.history_loaded = false;
    results.history_loading = true;
    results.head = Some(super_types::CommitRef {
        oid: current.revision.reported_git_commit.clone().unwrap(),
        parent_oid: None,
        branch: None,
        temporary: false,
    });
    let mut browser = Browser::new(&results);
    let loading = plain_frame(&mut browser, &results, 80, 24).join("\n");
    assert!(loading.contains("No check loaded"));
    assert!(!loading.contains("Not checked"));
    results
        .append_history(super_types::CheckHistoryPage {
            checks: vec![current],
            next_before: None,
        })
        .unwrap();
    results.history_loading = false;
    let frame = plain_frame(&mut browser, &results, 80, 24).join("\n");
    assert!(frame.contains("#1"));
    assert!(!frame.contains("No attempt"));
    assert_eq!(browser.selected(&results).unwrap().number, 1);
}

fn normalization_check(number: u64) -> Check {
    let mut check = result_check(
        number,
        super_types::CheckOutcome::Fail,
        super_types::CheckAssurance::Uncertified,
    );
    check.supertest.path = "supertests/normalize_spaces.py".into();
    check.supertest.name = "normalizing_twice_changes_nothing".into();
    check.supertest.line = Some(5);
    check.revision.reported_git_commit = Some(format!("8e44bea{}", "0".repeat(33)));
    check.presentation.details = [
        ("", false),
        ("Conclusion", true),
        ("  Normalizing twice changes the text.", false),
        ("  A single replacement can leave adjacent spaces.", false),
        ("", false),
        ("Problem 1", true),
        ("  Location: supertests/normalize_spaces.py:5", false),
        ("", false),
        ("Counterexample", true),
        ("  witness   = text = \"a   b\"", false),
        (
            "  expected  = the second call returns the same text as the first",
            false,
        ),
        ("  observed  = first call: \"a  b\"; second call: \"a b\"", false),
        (
            "  reproduce = Call collapse_spaces(\"a   b\"), then call it on that result.",
            false,
        ),
    ]
    .map(|(text, emphasized)| super_types::PresentationLine {
        text: text.into(),
        emphasized,
    })
    .to_vec();
    check
}

fn lines_text(lines: &[Line]) -> String {
    lines.iter().map(Line::text).collect::<Vec<_>>().join("\n")
}

fn page_to_end(browser: &mut Browser, results: &Results, width: usize, height: usize) {
    for _ in 0..1000 {
        let before = browser.frame(results, width, height, false);
        browser.key(KeyCode::PageDown, results, height, width);
        if before == browser.frame(results, width, height, false) {
            return;
        }
    }
    panic!("paging did not reach the end of the result");
}

#[test]
fn reported_results_keep_their_verdict_and_surface_operational_errors_in_overviews() {
    use super_types::{CheckAssurance, CheckOperationalError, CheckOutcome};

    for outcome in [CheckOutcome::Pass, CheckOutcome::Fail, CheckOutcome::Conditional] {
        for (error, label) in [
            (CheckOperationalError::Blocked, "blocked"),
            (CheckOperationalError::Canceled, "canceled"),
            (CheckOperationalError::Error, "error"),
            (CheckOperationalError::MissingConclusion, "error"),
        ] {
            let mut results = results(2);
            let mut reported = result_check(1, outcome, CheckAssurance::Uncertified);
            reported.operational_error = Some(error);
            let verdict = status(&reported).label;
            let combined = format!("{verdict} · {label}");
            results.rows[0].current = Some(reported.clone());
            results.rows[1].current = Some(result_check(2, CheckOutcome::Pass, CheckAssurance::Uncertified));
            for (width, height) in [(60, 20), (80, 24), (120, 36)] {
                let mut browser = Browser::new(&results);
                let frame = plain_frame(&mut browser, &results, width, height);
                let row = frame.iter().find(|line| line.contains("law_1")).unwrap();
                assert!(row.contains(&combined), "{row}");
                assert!(frame.iter().all(|line| measure_text_width(line) <= width));
                assert!(lines_text(&presentation::snapshot(&results, false, false, width)).contains(&combined));
                let columns = presentation::AttemptColumns::new(&[&reported], width - 6);
                assert!(columns.row(&reported).text().contains(&combined));
            }
            assert!(
                presentation::summary(&results)
                    .text()
                    .contains("1 with operational errors")
            );
            if outcome == CheckOutcome::Pass {
                assert!(presentation::summary(&results).text().contains("2 passed"));
            }
            assert_eq!(results.exit_code(), 2);
            let line = status(&reported).line();
            assert_eq!(line.parts.first().unwrap().1, status(&reported).ink);
            assert_eq!(line.parts.last().unwrap().1, Ink::Warning);

            results.rows[0].current.as_mut().unwrap().result = None;
            assert!(
                !presentation::summary(&results)
                    .text()
                    .contains("with operational errors")
            );
        }
    }
}

#[test]
fn activity_dots_keep_the_sentence_aligned_and_stop_with_the_live_view() {
    let mut results = results(1);
    for live in ["Roving... Analyzing the code...", "Roving… Analyzing the code..."] {
        results.rows[0].current.as_mut().unwrap().presentation.live_line = Some(live.into());
        let check = results.rows[0].current.as_ref().unwrap();
        for (tick, prefix) in [(0, "Roving.  "), (5, "Roving.. "), (10, "Roving..."), (15, "Roving.  ")] {
            let expected = format!("{prefix} Analyzing the code...");
            let lines = presentation::evidence_with_tick(check, EvidenceView::Selected, false, Some(tick));
            assert_eq!(lines[0].text(), expected);
            for (width, height) in [(60, 20), (80, 24), (120, 36)] {
                let mut browser = Browser::new(&results);
                browser.tick = tick;
                let frame = plain_frame(&mut browser, &results, width, height);
                assert!(frame.iter().all(|line| measure_text_width(line) <= width));
                assert!(frame.iter().any(|line| line.contains(&expected)));
            }
            let disconnected = presentation::evidence_with_tick(check, EvidenceView::Selected, true, Some(tick));
            assert_eq!(disconnected[0].text(), format!("Last reported: {live}"));
        }
        let static_lines = evidence(check, EvidenceView::Selected, false);
        assert_eq!(static_lines[0].text(), live);
        assert_eq!(check.presentation.live_line.as_deref(), Some(live));
    }
    for tick in [0, 5, 10, 15] {
        for plain in [
            "Checking the source without a phase prefix.",
            "Roving through the code.",
            "An update: Roving... through the code.",
            "...",
            "…",
            "",
        ] {
            assert_eq!(presentation::activity_sentence(plain, Some(tick)).text(), plain);
        }
    }
    let check = results.rows[0].current.as_mut().unwrap();
    check.terminal = true;
    let finished = presentation::evidence_with_tick(check, EvidenceView::Selected, false, Some(10));
    assert!(!lines_text(&finished).contains("Roving"));
}

#[test]
fn complete_activity_sentences_stay_together_and_wrap_without_losing_words() {
    let mut results = results(1);
    for (live, bold) in [
        ("Roving... Checking how the parser handles empty input.", "Roving..."),
        (
            "Roving... Checking how the parser handles empty input, Unicode whitespace, and a trailing separator without losing any characters.",
            "Roving...",
        ),
        ("Sniffing… through claim", "Sniffing…"),
        ("Sketching...", "Sketching..."),
        ("Checking the source without a phase prefix.", ""),
    ] {
        let check = results.rows[0].current.as_mut().unwrap();
        check.presentation.live_line = Some(live.into());
        check.presentation.activity_label = Some("analyzing".into());
        for disconnected in [false, true] {
            let expected = if disconnected {
                format!("Last reported: {live}")
            } else {
                live.to_owned()
            };
            let lines = evidence(check, EvidenceView::Selected, disconnected);
            assert_eq!(lines[0].text(), expected);
            assert_eq!(lines_text(&lines).matches(live).count(), 1);
            let emphasized: String = lines[0]
                .parts
                .iter()
                .filter(|(_, _, bold)| *bold)
                .map(|(text, _, _)| text.as_str())
                .collect();
            assert_eq!(emphasized, bold, "only the phase word and ellipsis should be bold");
            assert!(lines[0].parts.iter().all(|(_, ink, _)| *ink == Ink::Activity));
        }
        for width in [80, 120] {
            let mut browser = Browser::new(&results);
            browser.tick = 10; // The fully visible three-dot frame.
            let frame = plain_frame(&mut browser, &results, width, 24);
            assert!(frame.iter().all(|line| measure_text_width(line) <= width));
            let displayed = live.replacen('…', "...", 1);
            let text = frame.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(text.contains(&displayed), "{frame:#?}");
            if live.len() < 70 {
                assert!(frame.iter().any(|line| line.contains(&displayed)), "{frame:#?}");
            }
        }
    }
}

#[test]
fn live_findings_appear_once_and_remain_separate_from_activity_and_timing() {
    let mut check = normalization_check(42);
    check.terminal = false;
    for live in [Some("Reviewing supporting evidence.".to_owned()), None] {
        check.presentation.live_line = live.clone();
        for view in [EvidenceView::Summary, EvidenceView::Selected, EvidenceView::Full] {
            for disconnected in [false, true] {
                let text = lines_text(&evidence(&check, view, disconnected));
                assert_eq!(text.matches("Normalizing twice changes the text.").count(), 1, "{text}");
                assert!(text.contains("Elapsed") && text.contains("last update"));
                if let Some(live) = &live {
                    assert_eq!(text.matches(live).count(), 1);
                    assert_eq!(text.contains("Last reported:"), disconnected);
                }
                assert!(text.find("Elapsed").unwrap() < text.find("Normalizing twice").unwrap());
                assert_eq!(text.contains("reproduce"), view == EvidenceView::Full);
                assert!(text.contains("text = \"a   b\""));
            }
        }
    }
}

#[test]
fn printed_results_share_one_scoped_details_hint() {
    let mut results = results(3);
    results.selector = "supertests/a file.py".into();
    for (index, row) in results.rows.iter_mut().enumerate() {
        row.current = Some(normalization_check(index as u64 + 1));
    }
    let text = lines_text(&presentation::snapshot(&results, false, false, 100));
    assert_eq!(text.matches("Details:").count(), 1, "{text}");
    assert!(text.ends_with("Details: super status 'supertests/a file.py' --details"));
    assert_eq!(text.matches("Normalizing twice changes the text.").count(), 3);
    assert!(!lines_text(&presentation::snapshot(&results, false, true, 100)).contains("Details:"));
    results.rows.truncate(1);
    assert!(
        lines_text(&presentation::snapshot(&results, false, false, 100))
            .ends_with("Details: super status --check 1 --details")
    );
}

#[test]
fn hidden_navigation_keys_do_nothing_and_history_actions_match_the_footer() {
    let mut results = results(3);
    results.rows[1].current = Some(normalization_check(2));
    results.rows[1].supertest = results.rows[1].current.as_ref().unwrap().supertest.clone();
    let mut browser = Browser::new(&results);
    browser.key(KeyCode::Down, &results, 24, 80);
    for inspect in [false, true] {
        if inspect {
            browser.key(KeyCode::Enter, &results, 24, 80);
            browser.key(KeyCode::PageDown, &results, 24, 80);
        }
        let before = browser.frame(&results, 80, 24, false);
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('p'),
            KeyCode::Char('q'),
            KeyCode::Home,
            KeyCode::End,
        ] {
            browser.key(key, &results, 24, 80);
            assert_eq!(browser.frame(&results, 80, 24, false), before, "{key:?}");
        }
    }
    for (loaded, loading, next, error, hint) in [
        (true, false, None, None, None),
        (false, false, None, None, Some("n load history")),
        (true, false, Some(2), None, Some("n older")),
        (true, false, Some(2), Some("unavailable"), Some("n retry history")),
        (true, true, Some(2), Some("unavailable"), None),
    ] {
        results.history_loaded = loaded;
        results.history_loading = loading;
        results.next_before = next;
        results.history_error = error.map(str::to_owned);
        assert_eq!(browser.history_action(&results), hint);
        let frame = plain_frame(&mut browser, &results, 80, 24).join("\n");
        for action in ["n load history", "n older", "n retry history"] {
            assert_eq!(frame.contains(action), hint == Some(action), "{frame}");
        }
        browser.key(KeyCode::Esc, &results, 24, 80);
        assert_eq!(browser.history_action(&results), None);
        browser.key(KeyCode::Enter, &results, 24, 80);
    }
}

#[test]
fn pending_fix_output_does_not_claim_active_preparation() {
    use super_types::{CheckAssurance::Uncertified, CheckOutcome};

    let mut queued = check(1);
    queued.presentation.status.label = "queued".into();
    let mut errored = check(1);
    errored.terminal = true;
    errored.operational_error = Some(super_types::CheckOperationalError::Error);
    let mut finding = check(1);
    finding.problematic = true;
    for mut check in [
        queued,
        check(1),
        result_check(1, CheckOutcome::Pass, Uncertified),
        result_check(1, CheckOutcome::Conditional, Uncertified),
        errored,
        finding,
        result_check(1, CheckOutcome::Fail, Uncertified),
    ] {
        check.fix_pending = true;
        check.updates_pending = true;
        let original_json = serde_json::to_value(&check).unwrap();
        for details in [false, true] {
            let output = lines_text(&evidence(&check, EvidenceView::snapshot(details), false));
            assert_eq!(
                output.contains("A fix proposal may still arrive."),
                check.problematic && check.operational_error.is_none()
            );
            assert!(!output.contains("being prepared"));
            assert!(!output.contains("super fix"));
        }
        assert_eq!(serde_json::to_value(&check).unwrap(), original_json);
        check.fix_pending = false;
        let output = lines_text(&evidence(&check, EvidenceView::Summary, false));
        assert!(!output.contains("may still arrive"));
        assert_eq!(
            output.contains("No fix proposal available."),
            check.terminal && check.problematic
        );
    }
}

#[test]
fn pending_fix_summary_offers_watch_and_preserves_the_detail_preference() {
    let mut results = results(1);
    let mut check = normalization_check(42);
    check.fix_pending = true;
    check.updates_pending = true;
    results.rows[0].current = Some(check);
    for details in [false, true] {
        let text = lines_text(&presentation::snapshot(&results, false, details, 100));
        assert!(text.contains("A fix proposal may still arrive."));
        assert!(!text.contains("No fix proposal"));
        assert!(!text.contains("Details: super status"));
        assert!(
            text.ends_with(if details {
                "Watch: super status --check 42 --watch --details"
            } else {
                "Watch: super status --check 42 --watch"
            }),
            "{text}"
        );
    }
}

#[test]
fn pending_fix_hint_clears_when_delivery_stops_or_connection_is_lost() {
    let mut check = normalization_check(42);
    check.fix_pending = true;
    check.updates_pending = true;
    let original_json = serde_json::to_value(&check).unwrap();
    for details in [false, true] {
        assert!(
            lines_text(&evidence(&check, EvidenceView::snapshot(details), false))
                .contains("A fix proposal may still arrive.")
        );
        assert!(!lines_text(&evidence(&check, EvidenceView::snapshot(details), true)).contains("may still arrive"));
        assert!(!lines_text(&evidence(&check, EvidenceView::snapshot(details), true)).contains("No fix proposal"));
    }
    assert_eq!(serde_json::to_value(&check).unwrap(), original_json);
    for state in 0..6 {
        let mut stopped = check.clone();
        match state {
            0 => stopped.fix_pending = false,
            1 => stopped.updates_pending = false,
            2 => stopped.problematic = false,
            3 => stopped.operational_error = Some(super_types::CheckOperationalError::Canceled),
            4 => stopped.operational_error = Some(super_types::CheckOperationalError::Blocked),
            _ => stopped.operational_error = Some(super_types::CheckOperationalError::Error),
        }
        assert!(!lines_text(&evidence(&stopped, EvidenceView::Summary, false)).contains("may still arrive"));
        assert_eq!(
            lines_text(&evidence(&stopped, EvidenceView::Summary, false)).contains("No fix proposal available."),
            state < 2,
            "Only show no proposal when the API confirms that none remains pending"
        );
    }
    let mut results = results(1);
    results.rows[0].current = Some(check);
    let mut browser = Browser::new(&results);
    assert!(
        plain_frame(&mut browser, &results, 80, 24)
            .join("\n")
            .contains("may still arrive")
    );
    results.rows[0].current.as_mut().unwrap().fix_pending = false;
    assert!(
        !plain_frame(&mut browser, &results, 80, 24)
            .join("\n")
            .contains("may still arrive")
    );
}

#[test]
fn available_fix_is_shown_without_a_preparation_hint_or_changes_to_its_data() {
    let mut check = normalization_check(42);
    check.fix_pending = true;
    check.updates_pending = true;
    check.fix = Some(super_types::FixProposal {
        id: Uuid::nil(),
        state: super_types::FixProposalState::Proposed,
        base_revision_id: check.revision.id,
        base_tree_sha256: check.revision.tree_sha256.clone(),
        summary: "Repeat the replacement until no adjacent spaces remain.".into(),
        diff: "diff --git a/normalize.py b/normalize.py\n--- a/normalize.py\n+++ b/normalize.py\n@@ -1 +1 @@\n-    return text.replace('  ', ' ')\n+    return ' '.join(text.split(' '))\n".into(),
        instructions: "Review the patch before applying it.".into(),
        files: vec![super_types::FixFile { path: "normalize.py".into(), change: super_types::FixFileChange::Modified }],
        validation: vec!["super check".into()],
        created_at: check.updated_at,
    });
    let original_json = serde_json::to_value(&check).unwrap();
    for details in [false, true] {
        let output = lines_text(&evidence(&check, EvidenceView::snapshot(details), false));
        assert!(output.contains("Review and apply: super fix --check 42"));
        assert!(!output.contains("being prepared"));
        assert!(!output.contains("may still arrive"));
        assert!(!output.contains("No fix proposal"));
        assert_eq!(output.contains(&check.fix.as_ref().unwrap().summary), details);
    }
    assert_eq!(serde_json::to_value(&check).unwrap(), original_json);
}

#[test]
fn compact_results_keep_exact_evidence_and_expand_reproduction_in_full_details() {
    console::set_colors_enabled(true);
    let check = normalization_check(42);
    let before = serde_json::to_value(&check).unwrap();
    let compact = lines_text(&presentation::evidence(&check, EvidenceView::Summary, false));
    assert_eq!(
        compact,
        "Normalizing twice changes the text.\n\n  A single replacement can leave adjacent spaces.\n\nCounterexample\n  witness  = text = \"a   b\"\n  expected = the second call returns the same text as the first\n  observed = first call: \"a  b\"; second call: \"a b\"\n\nNo fix proposal available."
    );
    let details = lines_text(&presentation::evidence(&check, EvidenceView::Full, false));
    assert!(!details.contains("Conclusion"));
    assert!(details.contains("Normalizing twice changes the text.\n\n  A single replacement"));
    assert!(details.contains("A single replacement can leave adjacent spaces.\n\nProblem 1"));
    assert!(details.contains("reproduce = Call collapse_spaces"));
    for view in [EvidenceView::Selected, EvidenceView::Full] {
        let lines = evidence(&check, view, false);
        let headline = lines
            .iter()
            .find(|line| line.text() == "Normalizing twice changes the text.")
            .unwrap();
        assert!(text_style(&headline.render(120, 2), "Normalizing twice").bold);
        let counterexample = lines.iter().find(|line| line.text() == "Counterexample").unwrap();
        assert!(text_style(&counterexample.render(120, 2), "Counterexample").bold);
        assert!(!text_style(&counterexample.render(120, 2), "Counterexample").italic);
    }
    assert_eq!(serde_json::to_value(&check).unwrap(), before);
    let mut embedded = check;
    embedded.presentation.details[9]
        .text
        .push_str("\n  reproduce = this is part of the witness\nProblem 900");
    let compact = lines_text(&super::compact::details(&embedded));
    assert!(compact.contains("reproduce = this is part of the witness\nProblem 900"));
    let mut evidence_only = normalization_check(42);
    evidence_only.presentation.details.drain(..4);
    let compact = lines_text(&super::compact::details(&evidence_only));
    assert!(compact.starts_with("Counterexample\n  witness"));
    assert!(
        !compact.contains("Problem"),
        "one counterexample needs only its own heading"
    );
}

#[test]
fn unknown_or_partial_presentation_is_preserved_in_compact_output() {
    let original = normalization_check(42);
    for index in 0..original.presentation.details.len() {
        let mut check = original.clone();
        check.presentation.details[index].emphasized = !check.presentation.details[index].emphasized;
        assert_eq!(
            super::compact::details(&check),
            presentation::full_details(&check),
            "unrecognized record {index}"
        );
    }
    let mut check = original.clone();
    check.presentation.details.push(super_types::PresentationLine {
        text: "Additional result information".into(),
        emphasized: true,
    });
    assert_eq!(super::compact::details(&check), presentation::full_details(&check));
    check = original.clone();
    check.presentation.details.insert(
        4,
        super_types::PresentationLine {
            text: "  An unrecognized narrative record.".into(),
            emphasized: false,
        },
    );
    assert_eq!(super::compact::details(&check), presentation::full_details(&check));
    check = original;
    check.presentation.details.pop();
    assert_eq!(super::compact::details(&check), presentation::full_details(&check));
}

#[test]
fn compact_completion_omits_passes_regardless_of_explanation_and_counts_certification() {
    let mut results = results(4);
    for row in &mut results.rows {
        let mut passed = result_check(
            row.current.as_ref().unwrap().number,
            super_types::CheckOutcome::Pass,
            super_types::CheckAssurance::Uncertified,
        );
        // Optional output can still be pending after a pass; it does not imply a fix is needed.
        passed.fix_pending = true;
        passed.presentation.details = vec![
            super_types::PresentationLine {
                text: "Conclusion".into(),
                emphasized: true,
            },
            super_types::PresentationLine {
                text: "  No problems found within this supertest.".into(),
                emphasized: false,
            },
        ];
        row.current = Some(passed);
    }
    let all_pass = presentation::snapshot(&results, false, false, 80);
    assert_eq!(all_pass.len(), 1);
    assert!(all_pass[0].text().starts_with("✓ 4 supertests passed"));
    results.rows[0].current = Some(normalization_check(42));
    let mixed = lines_text(&presentation::snapshot(&results, false, false, 80));
    assert!(mixed.contains("3 passed · 1 failed"));
    assert!(!mixed.contains("law_2"));
    assert!(mixed.contains("super status . --details"));
    results.rows[1]
        .current
        .as_mut()
        .unwrap()
        .presentation
        .details
        .push(super_types::PresentationLine {
            text: "  The function preserves every input value.".into(),
            emphasized: false,
        });
    results.rows[2]
        .current
        .as_mut()
        .unwrap()
        .result
        .as_mut()
        .unwrap()
        .assurance = super_types::CheckAssurance::Certified;
    let certified = lines_text(&presentation::snapshot(&results, false, false, 80));
    assert!(certified.contains("3 passed (1 certified) · 1 failed"));
    for row in &results.rows[1..] {
        assert!(!certified.contains(&row.supertest.name));
    }
    assert!(!certified.contains("The function preserves every input value."));
    assert!(!certified.contains('─'));
    let full = lines_text(&presentation::snapshot(&results, false, true, 80));
    assert!(full.contains("The function preserves every input value."));
    assert!(full.contains("pass (certified)"));
    assert_eq!(full.matches(&"─".repeat(76)).count(), 3);
    results.rows.remove(0);
    let all_pass = presentation::snapshot(&results, false, false, 80);
    assert_eq!(all_pass.len(), 1);
    assert!(all_pass[0].text().starts_with("✓ 3 supertests passed (1 certified)"));
}

#[test]
fn unknown_pass_layouts_remain_complete_in_selected_panels_but_require_details_in_snapshots() {
    let mut passed = result_check(
        1,
        super_types::CheckOutcome::Pass,
        super_types::CheckAssurance::Uncertified,
    );
    // A newer or differently formatted explanation must also stay out of compact output.
    passed.presentation.details = vec![super_types::PresentationLine {
        text:
            "Deduplication preserves unique items and removes repeats.\nThe function tracks previously seen integers."
                .into(),
        emphasized: false,
    }];
    let original_json = serde_json::to_value(&passed).unwrap();
    for count in [1, 2] {
        let mut results = results(count);
        results.rows[0].current = Some(passed.clone());
        let compact = lines_text(&presentation::snapshot(&results, false, false, 120));
        assert!(!compact.contains("Deduplication"));
        let full = lines_text(&presentation::snapshot(&results, false, true, 120));
        assert!(full.contains("Deduplication"));
        let mut browser = Browser::new(&results);
        let compact = plain_frame(&mut browser, &results, 120, 36).join("\n");
        assert!(compact.contains("Deduplication"));
        assert!(compact.contains("The function tracks previously seen integers."));
        assert!(!compact.contains("No problems found"));
        browser.key(KeyCode::Enter, &results, 36, 120);
        assert!(
            plain_frame(&mut browser, &results, 120, 36)
                .join("\n")
                .contains("Deduplication")
        );
        browser.key(KeyCode::Esc, &results, 36, 120);
        assert!(
            plain_frame(&mut browser, &results, 120, 36)
                .join("\n")
                .contains("Deduplication")
        );
        browser.configure(true, false);
        assert!(
            plain_frame(&mut browser, &results, 120, 36)
                .join("\n")
                .contains("Deduplication")
        );
        assert_eq!(
            serde_json::to_value(results.rows[0].current.as_ref().unwrap()).unwrap(),
            original_json
        );
    }
}

#[test]
fn selected_pass_shows_explanation_only_when_expanded() {
    console::set_colors_enabled(true);
    let mut passed = result_check(
        1,
        super_types::CheckOutcome::Pass,
        super_types::CheckAssurance::Uncertified,
    );
    passed.presentation.details = [
        ("", false),
        ("Conclusion", true),
        ("  Duplicate items are removed.", false),
        (
            "  The function tracks previously seen integers.\nRepeated values are skipped.",
            false,
        ),
    ]
    .map(|(text, emphasized)| super_types::PresentationLine {
        text: text.into(),
        emphasized,
    })
    .to_vec();
    let expected = "Duplicate items are removed.";
    assert_eq!(lines_text(&evidence(&passed, EvidenceView::Selected, false)), expected);
    let full = lines_text(&evidence(&passed, EvidenceView::Full, false));
    assert!(full.contains("Duplicate items are removed.\n\n  The function tracks previously seen integers."));
    assert!(full.ends_with("Repeated values are skipped."));
    let original_json = serde_json::to_value(&passed).unwrap();
    for count in [1, 2] {
        let mut results = results(count);
        results.rows[0].current = Some(passed.clone());
        let mut browser = Browser::new(&results);
        for expanded in [false, true, false] {
            if expanded {
                browser.key(KeyCode::Enter, &results, 36, 120);
            } else {
                browser.key(KeyCode::Esc, &results, 36, 120);
            }
            let text = plain_frame(&mut browser, &results, 120, 36).join("\n");
            assert!(text.contains("Duplicate items are removed."));
            assert_eq!(text.contains("Repeated values are skipped."), expanded);
            let frame = browser.frame(&results, 120, 36, false);
            let headline = frame
                .iter()
                .find(|line| line.contains("Duplicate items are removed."))
                .unwrap();
            assert_eq!(text_style(headline, "Duplicate items").bold, expanded);
            assert!(!text_style(headline, "Duplicate items").italic);
            assert!(!text.contains("Conclusion"));
        }
        browser.configure(true, false);
        assert!(
            plain_frame(&mut browser, &results, 120, 36)
                .join("\n")
                .contains("Repeated values are skipped.")
        );
        assert_eq!(
            serde_json::to_value(results.rows[0].current.as_ref().unwrap()).unwrap(),
            original_json
        );
    }
    // A headline-only fallback adds no empty section.
    for length in [4, 3] {
        passed.presentation.details.truncate(length);
        assert_eq!(
            lines_text(&evidence(&passed, EvidenceView::Selected, false)),
            "Duplicate items are removed."
        );
    }
}

#[test]
fn narrative_emphasis_depends_on_visible_explanation_across_result_states() {
    use super_types::{CheckOperationalError, CheckOutcome};

    console::set_colors_enabled(true);
    for (outcome, error) in [
        (Some(CheckOutcome::Pass), None),
        (Some(CheckOutcome::Fail), None),
        (Some(CheckOutcome::Conditional), None),
        (None, Some(CheckOperationalError::Blocked)),
        (None, Some(CheckOperationalError::Canceled)),
        (None, Some(CheckOperationalError::Error)),
        (None, Some(CheckOperationalError::MissingConclusion)),
        (None, None),
    ] {
        for length in [3, 4] {
            let mut check = normalization_check(42);
            check.result = outcome.map(|outcome| super_types::CheckResult {
                outcome,
                assurance: super_types::CheckAssurance::Uncertified,
            });
            check.operational_error = error;
            check.problematic = outcome.is_some_and(|outcome| outcome != CheckOutcome::Pass);
            check.presentation.details.truncate(length);
            for view in [EvidenceView::Selected, EvidenceView::Full] {
                let lines = evidence(&check, view, false);
                let headline = lines
                    .iter()
                    .find(|line| line.text() == "Normalizing twice changes the text.")
                    .unwrap();
                let headline_style = text_style(&headline.render(120, 2), "Normalizing twice");
                let explanation_visible = length > 3
                    && !(view == EvidenceView::Selected && outcome == Some(CheckOutcome::Pass) && error.is_none());
                assert_eq!(headline_style.bold, explanation_visible);
                assert!(!headline_style.italic);
                for line in lines.iter().skip(1) {
                    for wrapped in line.wrapped(24) {
                        for (character, style) in ansi_cells(&wrapped.render(28, 2)) {
                            if !character.is_whitespace() {
                                assert!(!style.italic);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn pass_compaction_does_not_hide_a_problem_error_or_unrecognized_multiline_headline() {
    let mut check = normalization_check(42);
    check.result.as_mut().unwrap().outcome = super_types::CheckOutcome::Pass;
    check.problematic = false;
    for error in [None, Some(super_types::CheckOperationalError::Blocked)] {
        check.operational_error = error;
        assert!(lines_text(&evidence(&check, EvidenceView::Selected, false)).contains("A single replacement"));
    }
    check.problematic = false;
    check.presentation.details.truncate(4);
    assert!(lines_text(&evidence(&check, EvidenceView::Selected, false)).contains("A single replacement"));
    check.operational_error = None;
    check.presentation.details[2]
        .text
        .push_str("\nAn unexpected second headline line.");
    assert!(lines_text(&evidence(&check, EvidenceView::Selected, false)).contains("A single replacement"));
}

#[test]
fn omitting_pass_explanations_does_not_hide_operational_errors_or_reported_problems() {
    let mut results = results(2);
    let mut passed = result_check(
        1,
        super_types::CheckOutcome::Pass,
        super_types::CheckAssurance::Uncertified,
    );
    passed.operational_error = Some(super_types::CheckOperationalError::Error);
    results.rows[0].current = Some(passed);
    let text = lines_text(&presentation::snapshot(&results, false, false, 80));
    assert!(text.contains("law_1") && text.contains("operational error"));
    let mut finding = normalization_check(42);
    finding.result.as_mut().unwrap().outcome = super_types::CheckOutcome::Pass;
    results.rows[0].current = Some(finding);
    let text = lines_text(&presentation::snapshot(&results, false, false, 80));
    assert!(text.contains("Normalizing twice changes the text."));
    assert!(text.contains("witness"));
}

#[test]
fn panel_grows_and_shrinks_with_content_preserves_padding_and_scrolls_to_the_last_line() {
    console::set_colors_enabled(true);
    let mut results = results(2);
    results.rows[1].current = Some(normalization_check(42));
    let mut browser = Browser::new(&results);
    let panel_height = |frame: &[String]| {
        frame
            .iter()
            .filter(|line| ansi_cells(line)[1].1.background == Some(style::PANEL))
            .count()
    };
    let short = browser.frame(&results, 80, 36, false);
    browser.key(KeyCode::Down, &results, 36, 80);
    let longer = browser.frame(&results, 80, 36, false);
    assert!(panel_height(&longer) > panel_height(&short));
    browser.key(KeyCode::Up, &results, 36, 80);
    let again = browser.frame(&results, 80, 36, false);
    assert_eq!(panel_height(&short), panel_height(&again));
    assert_panel_padding(&again, 80);
    results.rows[0]
        .current
        .as_mut()
        .unwrap()
        .presentation
        .details
        .push(super_types::PresentationLine {
            text: (0..150)
                .map(|index| format!("Evidence line {index:03}"))
                .collect::<Vec<_>>()
                .join("\n"),
            emphasized: false,
        });
    browser.key(KeyCode::Enter, &results, 24, 80);
    let small = browser.frame(&results, 80, 24, false);
    let large = browser.frame(&results, 80, 36, false);
    assert!(panel_height(&large) > panel_height(&small));
    page_to_end(&mut browser, &results, 80, 36);
    let end = browser.frame(&results, 80, 36, false);
    assert!(end.iter().any(|line| line.contains("Evidence line 149")));
    assert_panel_padding(&end, 80);
    assert_eq!(end[..3], large[..3], "scrolling cannot move the check's identity");
}

#[test]
fn very_long_identity_cannot_displace_controls_and_keyboard_scrolling_reaches_the_end() {
    console::set_colors_enabled(true);
    let mut results = results(1);
    let check = results.rows[0].current.as_mut().unwrap();
    check.supertest.name = "very_long_supertest_name_".repeat(40);
    check.supertest.path = format!("{}test.py", "nested/".repeat(50));
    check.presentation.details = vec![super_types::PresentationLine {
        text: "Evidence tail".into(),
        emphasized: false,
    }];
    for (width, height) in [(60, 20), (80, 24), (120, 36)] {
        let mut browser = Browser::new(&results);
        let frame = browser.frame(&results, width, height, false);
        assert_eq!(frame.len(), height);
        assert!(frame.iter().any(|line| line.contains("Esc/Ctrl+C detach")));
        browser.key(KeyCode::PageDown, &results, height, width);
        let scrolled = browser.frame(&results, width, height, false);
        if frame.iter().any(|line| line.contains("PgUp/PgDn scroll")) {
            assert_ne!(frame, scrolled);
        } else {
            assert_eq!(frame, scrolled, "a result that fits does not scroll");
        }
        page_to_end(&mut browser, &results, width, height);
        let end = browser.frame(&results, width, height, false);
        assert!(end.iter().any(|line| line.contains("Evidence tail")));
        assert_panel_padding(&end, width);
    }
}

#[test]
fn canceled_snapshots_are_concise_but_never_hide_existing_findings() {
    let mut results = results(1);
    let check = results.rows[0].current.as_mut().unwrap();
    check.terminal = true;
    check.operational_error = Some(super_types::CheckOperationalError::Canceled);
    check.supertest.path = "supertests/a file.py".into();
    let compact = presentation::snapshot(&results, false, false, 80);
    assert_eq!(
        lines_text(&compact),
        "○ canceled · law_1 · #1\n\nTo check again: super check 'supertests/a file.py::law_1'"
    );
    assert!(compact.last().unwrap().soft_wrap);
    let expanded = lines_text(&presentation::snapshot(&results, false, true, 80));
    assert!(expanded.contains("supertests/a file.py:3"));

    let check = results.rows[0].current.as_mut().unwrap();
    check.problematic = true;
    check.presentation.details.push(super_types::PresentationLine {
        text: "Exact evidence\n\nMore evidence".into(),
        emphasized: false,
    });
    let text = lines_text(&presentation::snapshot(&results, false, false, 80));
    assert!(text.contains("Exact evidence\n\nMore evidence"), "{text}");
    assert!(text.contains("Details: super status --check 1 --details"));

    let passed = result_check(
        2,
        super_types::CheckOutcome::Pass,
        super_types::CheckAssurance::Uncertified,
    );
    results.rows.push(ScopeStatusRow {
        supertest: passed.supertest.clone(),
        current: Some(passed),
        previous: None,
    });
    let text = lines_text(&presentation::snapshot(&results, false, false, 80));
    assert!(text.contains("1 passed · 1 canceled"), "{text}");
    assert!(text.contains("Exact evidence\n\nMore evidence"), "{text}");
}

#[test]
fn cancellation_tables_align_with_unicode_and_fall_back_on_narrow_terminals() {
    use crate::cancel::{Entry, Outcome};
    let mut results = results(3);
    let canceled = results.rows[0].current.as_mut().unwrap();
    canceled.terminal = true;
    canceled.operational_error = Some(super_types::CheckOperationalError::Canceled);
    results.rows[1].current = Some(result_check(
        2,
        super_types::CheckOutcome::Pass,
        super_types::CheckAssurance::Certified,
    ));
    results.rows[0].current.as_mut().unwrap().supertest.name = "unicode_界_law".into();
    results.rows[1].current.as_mut().unwrap().supertest.name = "another_long_supertest_name".into();
    results.rows[2].current.as_mut().unwrap().supertest.path = "other/test.py".into();
    let entries: Vec<_> = [Outcome::Canceled, Outcome::AlreadyFinished, Outcome::Requested]
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| Entry {
            check_number: index as u64 + 1,
            outcome,
            error: None,
        })
        .collect();
    for width in [40, 60, 80, 120] {
        let lines = cancellation::render(&results, &entries, width);
        let text = lines_text(&lines);
        assert_eq!(text.matches("Watch:").count(), 1, "{text}");
        assert!(!text.contains("--run"));
        assert!(text.contains("other/test.py"));
        assert!(text.contains("1 canceled · 1 cancellation requested · 1 already finished"));
        assert!(text.contains("pass (certified)"), "{text}");
        assert_eq!(
            text.matches("already finished").count(),
            1,
            "only the summary describes the cancellation outcome: {text}"
        );
        let header = lines.iter().find(|line| line.text().contains("SUPERTEST"));
        if width == 40 {
            assert!(header.is_none());
            assert!(text.contains("another_long_supertest_name"));
        } else {
            let header = header.unwrap().text();
            let column = measure_text_width(header.split('·').next().unwrap());
            for line in lines.iter().filter(|line| line.text().contains(" · #")) {
                assert!(line.width() <= width - 4);
                assert_eq!(measure_text_width(line.text().split('·').next().unwrap()), column);
            }
        }
        for line in lines.iter().filter(|line| !line.soft_wrap) {
            assert!(line.wrapped(width - 4).iter().all(|line| line.width() <= width - 4));
        }
    }
}
