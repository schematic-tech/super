use super::{
    Ui,
    style::{Ink, Line},
};
use anyhow::{Context, Result, ensure};
use crossterm::{
    cursor::{MoveToColumn, MoveUp},
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use dialoguer::{Password, theme::Theme};
use futures_util::StreamExt;
use std::{fmt, io::Write};

fn question(prompt: &str) -> Line {
    Line::default().push("? ", Ink::Accent, true).bold(prompt)
}

fn frame(prompt: &str, default: bool, answer: &str, invalid: bool, width: usize) -> Vec<Line> {
    let width = width.saturating_sub(4).max(1);
    let question = question(prompt);
    let input = Line::default()
        .muted(if default { "[Y/n] › " } else { "[y/N] › " })
        .push(answer, Ink::Accent, true);
    let mut lines = if question.width() + input.width() < width {
        vec![question.plain(" ").append(input)]
    } else {
        let mut lines = question.wrapped(width);
        lines.extend(input.wrapped(width));
        lines
    };
    if invalid {
        lines.insert(
            0,
            Line::default().push("Enter y or n, then press Enter.", Ink::Warning, false),
        );
    }
    lines
}

struct Prompt {
    rows: usize,
}
impl Prompt {
    fn draw(&mut self, lines: &[Line], width: usize) -> Result<()> {
        if self.rows > 1 {
            let rows = u16::try_from(self.rows - 1)?;
            execute!(std::io::stderr(), MoveUp(rows))?;
        }
        execute!(std::io::stderr(), MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
        self.rows = lines.len();
        let rendered: Vec<_> = lines
            .iter()
            .map(|line| line.render(width, 2).trim_end().to_owned())
            .collect();
        write!(std::io::stderr(), "{}", rendered.join("\r\n"))?;
        std::io::stderr().flush()?;
        Ok(())
    }
}
impl Drop for Prompt {
    fn drop(&mut self) {
        let _ = write!(std::io::stderr(), "\r\n");
        let _ = disable_raw_mode();
    }
}

impl Ui {
    /// Both confirmations share editable input, explicit Enter, and visible defaults.
    /// Cancellation is distinct from No so callers cannot remember a canceled choice.
    pub async fn confirm(&self, prompt: &str, default: bool) -> Result<Option<bool>> {
        ensure!(self.interactive, "confirmation requires an interactive terminal");
        enable_raw_mode()?;
        let mut screen = Prompt { rows: 0 };
        let mut events = EventStream::new();
        let mut answer = String::new();
        let mut invalid = false;
        let accepted = loop {
            let width = Self::width();
            screen.draw(&frame(prompt, default, &answer, invalid, width), width)?;
            tokio::select! {
                signal = tokio::signal::ctrl_c() => { signal?; break None; }
                event = events.next() => match event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => match key.code {
                        KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
                        KeyCode::Esc => break None,
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            answer.clear(); invalid = false;
                        }
                        KeyCode::Backspace | KeyCode::Delete => { answer.pop(); invalid = false; }
                        KeyCode::Enter => match answer.trim().to_ascii_lowercase().as_str() {
                            "" => break Some(default),
                            "y" | "yes" => break Some(true),
                            "n" | "no" => break Some(false),
                            _ => invalid = true,
                        },
                        KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && !c.is_control() && answer.len() < 32 => {
                            answer.push(c); invalid = false;
                        }
                        _ => {}
                    },
                    Some(Err(error)) => return Err(error.into()),
                    None => break None,
                    _ => {}
                }
            }
        };
        let answer = accepted.map_or("canceled", |yes| if yes { "yes" } else { "no" });
        let width = Self::width();
        screen.draw(&frame(prompt, default, answer, false, width), width)?;
        Ok(accepted)
    }

    pub fn api_key(&self) -> Result<String> {
        ensure!(self.interactive, "noninteractive login requires SUPER_ACCESS_TOKEN");
        Password::with_theme(&PromptTheme)
            .with_prompt("Schematic API key")
            .allow_empty_password(false)
            .interact()
            .context("could not read the Schematic API key")
    }
}

struct PromptTheme;
impl Theme for PromptTheme {
    fn format_password_prompt(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        write!(f, "{} ", question(prompt).muted(" ›").render(Ui::width(), 2).trim_end())
    }

    fn format_password_prompt_selection(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        write!(
            f,
            "{}",
            question(prompt).muted(" › [hidden]").render(Ui::width(), 2).trim_end()
        )
    }

    fn format_error(&self, f: &mut dyn fmt::Write, error: &str) -> fmt::Result {
        write!(
            f,
            "{}",
            Line::default()
                .push(error, Ink::Fail, false)
                .render(Ui::width(), 2)
                .trim_end()
        )
    }
}
