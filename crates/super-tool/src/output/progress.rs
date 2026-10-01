use super::{
    TICKS, Ui,
    style::{Ink, Line},
};
use crate::api::SourceProgress;
use anyhow::Result;
use crossterm::{
    cursor::{MoveToColumn, MoveUp},
    execute, queue,
    terminal::{Clear, ClearType},
};
use std::{
    future::Future,
    io::Write,
    time::{Duration, Instant},
};
use tokio::sync::watch;

pub struct Progress {
    sender: watch::Sender<SourceProgress>,
    receiver: watch::Receiver<SourceProgress>,
    interactive: bool,
    started: Instant,
    link_repository: Option<String>,
}

impl Ui {
    pub fn progress(&self, message: impl Into<String>) -> Progress {
        let (sender, receiver) = watch::channel(SourceProgress::Message(message.into()));
        Progress {
            sender,
            receiver,
            interactive: self.interactive,
            started: Instant::now(),
            link_repository: None,
        }
    }

    pub fn link_progress(&self, repository: &str) -> Progress {
        let mut progress = self.progress("connecting");
        progress.link_repository = Some(repository.to_owned());
        progress
    }
}

impl Progress {
    pub fn sender(&self) -> watch::Sender<SourceProgress> {
        self.sender.clone()
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn label(&self, progress: &SourceProgress) -> String {
        self.link_repository.as_ref().map_or_else(
            || label(progress, self.elapsed()),
            |repository| link_label(repository, progress, self.elapsed()),
        )
    }

    pub fn run<T>(&self, future: impl Future<Output = Result<T>>) -> impl Future<Output = Result<T>> {
        self.run_with_interrupt(
            future,
            "Interrupted. Remote work was not canceled; run the command again to retry.",
        )
    }

    pub fn run_with_interrupt<T>(
        &self,
        future: impl Future<Output = Result<T>>,
        interrupted: &str,
    ) -> impl Future<Output = Result<T>> {
        self.drive(Box::pin(future), interrupted.to_owned())
    }

    async fn drive<T>(
        &self,
        mut future: std::pin::Pin<Box<impl Future<Output = Result<T>>>>,
        interrupted: String,
    ) -> Result<T> {
        let mut receiver = self.receiver.clone();
        let mut guard = Inline::new(std::io::stderr());
        let mut tick = 0;
        let mut timer = tokio::time::interval(Duration::from_millis(120));
        let mut previous = None;
        let interrupted_signal = tokio::signal::ctrl_c();
        tokio::pin!(interrupted_signal);
        loop {
            let current = receiver.borrow_and_update().clone();
            let width = usize::from(crossterm::terminal::size().map_or(80, |size| size.0)).max(40);
            if self.interactive {
                let rows = self.link_repository.as_ref().map_or_else(
                    || frame(&current, self.elapsed(), tick),
                    |repository| link_frame(repository, &current, self.elapsed(), tick),
                );
                guard.draw(&rows, width)?;
            } else if previous
                .as_ref()
                .is_none_or(|prior| stage(prior) != stage(&current) || self.label(prior) != self.label(&current))
            {
                // Keep pipes/JSON free of animation and byte-by-byte progress chatter.
                eprintln!("{}", super::style::clean(&self.label(&current)));
            }
            previous = Some(current);
            tokio::select! {
                result = &mut future => return result,
                signal = &mut interrupted_signal => {
                    signal?;
                    anyhow::bail!("{interrupted}");
                }
                _ = receiver.changed() => {}
                _ = timer.tick(), if self.interactive => tick = (tick + 1) % TICKS.len(),
            }
        }
    }
}

fn stage(progress: &SourceProgress) -> u8 {
    match progress {
        SourceProgress::Message(_) => 0,
        SourceProgress::Preparing => 1,
        SourceProgress::Comparing => 2,
        SourceProgress::Uploading { .. } => 3,
        SourceProgress::Finalizing => 4,
    }
}

fn label(progress: &SourceProgress, elapsed: Duration) -> String {
    match progress {
        SourceProgress::Message(label) => label.clone(),
        SourceProgress::Preparing => "Preparing source".into(),
        SourceProgress::Comparing => "Checking source with Super".into(),
        SourceProgress::Uploading { .. } => "Syncing changes".into(),
        SourceProgress::Finalizing => if elapsed.as_secs() >= 30 {
            "Waiting for Super to confirm the source"
        } else {
            "Confirming source"
        }
        .into(),
    }
}

fn link_label(repository: &str, progress: &SourceProgress, elapsed: Duration) -> String {
    let stage = match progress {
        SourceProgress::Message(message) => message,
        SourceProgress::Preparing => "preparing source",
        SourceProgress::Comparing => "checking source",
        SourceProgress::Uploading { .. } => "syncing source",
        SourceProgress::Finalizing if elapsed.as_secs() >= 30 => "waiting for source confirmation",
        SourceProgress::Finalizing => "confirming source",
    };
    format!("Linking {repository} · {stage}")
}

fn link_frame(repository: &str, progress: &SourceProgress, elapsed: Duration, tick: usize) -> Vec<Line> {
    let mut rows = vec![
        Line::default()
            .push(format!("{} ", TICKS[tick % TICKS.len()]), Ink::Accent, false)
            .plain(link_label(repository, progress, elapsed)),
    ];
    if let SourceProgress::Uploading { completed, total } = progress
        && *total > 0
    {
        let complete = (*completed).min(*total);
        let (completed, amount, unit) = upload_amounts(complete, *total);
        rows.push(
            Line::new("  ")
                .append(upload_bar(complete, *total))
                .muted(format!(" · {completed} / {amount} {unit}")),
        );
    }
    rows
}

pub(super) fn frame(progress: &SourceProgress, elapsed: Duration, tick: usize) -> Vec<Line> {
    let seconds = elapsed.as_secs();
    let duration = if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    };
    let mut rows = vec![
        Line::default()
            .push(format!("{} ", TICKS[tick % TICKS.len()]), Ink::Accent, false)
            .plain(label(progress, elapsed))
            .muted(format!(" · {duration}")),
    ];
    if let SourceProgress::Uploading { completed, total } = progress {
        if *total > 0 {
            let complete = (*completed).min(*total);
            let (completed, amount, unit) = upload_amounts(complete, *total);
            rows.extend([
                Line::default(),
                upload_bar(complete, *total),
                Line::default().muted(format!("{completed} of {amount} {unit} synced")),
            ]);
        }
    } else if matches!(progress, SourceProgress::Finalizing) {
        rows.push(Line::default().muted("Source ready. Waiting for confirmation."));
    }
    rows
}

fn upload_bar(completed: u64, total: u64) -> Line {
    let percent = u128::from(completed) * 100 / u128::from(total);
    let filled = ((u128::from(completed) * 20 + u128::from(total) / 2) / u128::from(total)) as usize;
    Line::default()
        .push("━".repeat(filled), Ink::Accent, false)
        .muted("─".repeat(20 - filled))
        .plain(format!("  {percent}%"))
}

fn upload_amounts(completed: u64, total: u64) -> (String, String, &'static str) {
    if total < 1_000 {
        return (completed.to_string(), total.to_string(), "B");
    }
    let (scale, unit) = if total < 1_000_000 {
        (1_000, "KB")
    } else {
        (1_000_000, "MB")
    };
    let decimal = |bytes: u64| {
        let tenths = (u128::from(bytes) * 10 + scale / 2) / scale;
        format!("{}.{}", tenths / 10, tenths % 10)
    };
    (decimal(completed), decimal(total), unit)
}

struct Inline<W: Write> {
    output: W,
    rows: usize,
}
impl<W: Write> Inline<W> {
    fn new(output: W) -> Self {
        Self { output, rows: 0 }
    }

    fn clear(&mut self) -> Result<()> {
        if self.rows > 0 {
            let rows = u16::try_from(self.rows)?;
            execute!(
                self.output,
                MoveUp(rows),
                MoveToColumn(0),
                Clear(ClearType::FromCursorDown)
            )?;
            self.rows = 0;
        }
        Ok(())
    }
    fn draw(&mut self, lines: &[Line], width: usize) -> Result<()> {
        // Prepare the complete repaint before touching the terminal. Clearing first, or sending
        // lines separately, lets it display a frame with the status or upload details missing.
        let mut update = Vec::new();
        if self.rows > 0 {
            let previous_rows = u16::try_from(self.rows)?;
            queue!(update, MoveUp(previous_rows))?;
        }
        queue!(update, MoveToColumn(0))?;
        let mut rows = 0;
        for line in lines.iter().flat_map(|line| line.wrapped(width.saturating_sub(4))) {
            write!(update, "{}", line.render(width, 2).trim_end())?;
            // Overwrite existing text before clearing its old tail. This also handles a shorter
            // stage label without exposing a blank status line between frames.
            queue!(update, Clear(ClearType::UntilNewLine))?;
            writeln!(update)?;
            rows += 1;
        }
        if rows < self.rows {
            // A stage change can remove the bar. Clear only the rows below the new frame.
            queue!(update, Clear(ClearType::FromCursorDown))?;
        }
        self.output.write_all(&update)?;
        self.rows = rows;
        self.output.flush()?;
        Ok(())
    }
}
impl<W: Write> Drop for Inline<W> {
    fn drop(&mut self) {
        let _ = self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Output {
        writes: Vec<Vec<u8>>,
        flushes: usize,
    }

    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes.push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn link_progress_keeps_only_the_current_stage_and_transfer_details() {
        let rows = link_frame(
            "text-tools",
            &SourceProgress::Uploading {
                completed: 1_280_000,
                total: 2_000_000,
            },
            Duration::from_secs(12),
            0,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text(), "⠋ Linking text-tools · syncing source");
        assert_eq!(rows[1].text(), "  ━━━━━━━━━━━━━───────  64% · 1.3 / 2.0 MB");
        for (state, expected) in [
            (SourceProgress::Message("connecting".into()), "connecting"),
            (SourceProgress::Preparing, "preparing source"),
            (SourceProgress::Comparing, "checking source"),
            (SourceProgress::Finalizing, "confirming source"),
            (SourceProgress::Uploading { completed: 0, total: 0 }, "syncing source"),
        ] {
            let rows = link_frame("text-tools", &state, Duration::ZERO, 0);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].text(), format!("⠋ Linking text-tools · {expected}"));
        }
        let rows = link_frame("text-tools", &SourceProgress::Finalizing, Duration::from_secs(30), 0);
        assert_eq!(rows[0].text(), "⠋ Linking text-tools · waiting for source confirmation");
        let rows = link_frame(
            "text-tools",
            &SourceProgress::Uploading {
                completed: 150,
                total: 100,
            },
            Duration::ZERO,
            0,
        );
        assert_eq!(rows[1].text(), "  ━━━━━━━━━━━━━━━━━━━━  100% · 100 / 100 B");
    }

    #[test]
    fn compact_link_repaints_keep_the_bar_visible_and_clear_details_after_the_new_stage() {
        for width in [40, 80, 120] {
            let mut inline = Inline::new(Output::default());
            for (tick, completed) in [0, 7_800_000, 7_800_000, 15_900_000].into_iter().enumerate() {
                inline.output.writes.clear();
                inline.output.flushes = 0;
                inline
                    .draw(
                        &link_frame(
                            "text-tools",
                            &SourceProgress::Uploading {
                                completed,
                                total: 15_900_000,
                            },
                            Duration::from_secs(58 + tick as u64),
                            tick,
                        ),
                        width,
                    )
                    .unwrap();
                assert_eq!(
                    inline.output.writes.len(),
                    1,
                    "a repaint must be one complete output buffer"
                );
                assert_eq!(inline.output.flushes, 1);
                let update = String::from_utf8(inline.output.writes[0].clone()).unwrap();
                assert!(update.contains("15.9 MB"));
                assert!(
                    !update.contains("\x1b[J"),
                    "an upload repaint must not erase the progress region"
                );
                assert!(update.find("Linking text-tools").unwrap() < update.find("\x1b[K").unwrap());
            }
            let upload_rows = inline.rows;
            inline.output.writes.clear();
            inline
                .draw(
                    &link_frame("text-tools", &SourceProgress::Finalizing, Duration::ZERO, 0),
                    width,
                )
                .unwrap();
            let update = String::from_utf8(inline.output.writes[0].clone()).unwrap();
            assert!(update.starts_with(&format!("\x1b[{upload_rows}A\x1b[1G")));
            assert!(update.find("source").unwrap() < update.find("\x1b[J").unwrap());
            assert!(inline.rows < upload_rows);
            let final_rows = inline.rows;
            inline.output.writes.clear();
            inline.clear().unwrap();
            assert_eq!(
                inline.output.writes.concat(),
                format!("\x1b[{final_rows}A\x1b[1G\x1b[J").as_bytes()
            );
        }
    }

    #[test]
    fn upload_redraw_never_erases_the_status_before_replacing_it() {
        for width in [80, 120] {
            let mut inline = Inline::new(Output::default());
            for (tick, completed) in [0, 7_800_000, 7_800_000, 15_900_000].into_iter().enumerate() {
                inline.output.writes.clear();
                inline.output.flushes = 0;
                inline
                    .draw(
                        &frame(
                            &SourceProgress::Uploading {
                                completed,
                                total: 15_900_000,
                            },
                            Duration::from_secs(58 + tick as u64),
                            tick,
                        ),
                        width,
                    )
                    .unwrap();
                assert_eq!(
                    inline.output.writes.len(),
                    1,
                    "a repaint must be one complete output buffer"
                );
                assert_eq!(inline.output.flushes, 1);
                let update = String::from_utf8(inline.output.writes[0].clone()).unwrap();
                assert!(update.contains("Syncing changes"));
                assert!(update.contains("MB synced"));
                assert!(
                    !update.contains("\x1b[J"),
                    "an upload repaint must not erase the progress region"
                );
                assert!(update.find("Syncing changes").unwrap() < update.find("\x1b[K").unwrap());
            }
        }
    }

    #[test]
    fn shorter_stages_remove_old_details_after_drawing_and_cleanup_restores_the_cursor() {
        let mut output = Output::default();
        {
            let mut inline = Inline::new(&mut output);
            inline
                .draw(
                    &frame(
                        &SourceProgress::Uploading {
                            completed: 50,
                            total: 100,
                        },
                        Duration::from_secs(1),
                        0,
                    ),
                    80,
                )
                .unwrap();
            assert_eq!(inline.rows, 4);
            inline.output.writes.clear();
            inline
                .draw(&frame(&SourceProgress::Finalizing, Duration::from_secs(2), 1), 80)
                .unwrap();
            let update = String::from_utf8(inline.output.writes[0].clone()).unwrap();
            assert!(update.starts_with("\x1b[4A\x1b[1G"));
            assert!(update.find("Waiting for confirmation.").unwrap() < update.find("\x1b[J").unwrap());
            assert!(update.ends_with("\x1b[K\n\x1b[J"));
            assert_eq!(inline.rows, 2);

            // Wrapped messages use their physical row count when returning to a short stage.
            inline
                .draw(&[Line::new("A long preparation message ".repeat(8))], 40)
                .unwrap();
            let wrapped_rows = inline.rows;
            assert!(wrapped_rows > 2);
            inline.output.writes.clear();
            inline.draw(&[Line::new("Ready")], 40).unwrap();
            let update = String::from_utf8(inline.output.writes[0].clone()).unwrap();
            assert!(update.starts_with(&format!("\x1b[{wrapped_rows}A\x1b[1G")));
            assert!(update.find("Ready").unwrap() < update.find("\x1b[J").unwrap());
            inline.output.writes.clear();
        }
        assert_eq!(output.writes.concat(), b"\x1b[1A\x1b[1G\x1b[J");
    }
}
