use console::{Style, measure_text_width};
use std::io::IsTerminal;

pub(super) const PANEL: (u8, u8, u8) = (55, 60, 73);
pub(super) const SELECTION: (u8, u8, u8) = (74, 64, 87);
pub(super) const ADDED: (u8, u8, u8) = (36, 60, 45);
pub(super) const REMOVED: (u8, u8, u8) = (72, 42, 43);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Ink {
    #[default]
    Plain,
    Muted,
    Accent,
    Activity,
    Pass,
    Fail,
    Warning,
    AddedText,
    RemovedText,
}

impl Ink {
    pub fn style(self) -> Style {
        let rgb = match self {
            Self::Plain => return Style::new(),
            Self::Muted => (176, 182, 197),
            Self::Accent => (209, 177, 245),
            Self::Activity => return Style::new().cyan().bright(),
            Self::Pass => (149, 212, 173),
            Self::Fail => (243, 161, 161),
            Self::Warning => (230, 193, 126),
            Self::AddedText => (173, 222, 183),
            Self::RemovedText => (246, 178, 178),
        };
        Style::new().true_color(rgb.0, rgb.1, rgb.2)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Line {
    pub parts: Vec<(String, Ink, bool)>,
    pub background: Option<(u8, u8, u8)>,
    pub soft_wrap: bool,
}

impl Line {
    /// Let the terminal wrap a copyable command without inserting shell-significant newlines.
    pub fn soft_wrap(mut self) -> Self {
        self.soft_wrap = true;
        self
    }

    pub fn new(text: impl AsRef<str>) -> Self {
        Self::default().plain(text)
    }

    pub fn plain(self, text: impl AsRef<str>) -> Self {
        self.push(text, Ink::Plain, false)
    }

    pub fn muted(self, text: impl AsRef<str>) -> Self {
        self.push(text, Ink::Muted, false)
    }

    pub fn bold(self, text: impl AsRef<str>) -> Self {
        self.push(text, Ink::Plain, true)
    }

    pub fn push(mut self, text: impl AsRef<str>, ink: Ink, bold: bool) -> Self {
        self.parts.push((clean(text.as_ref()), ink, bold));
        self
    }

    pub fn append(mut self, other: Self) -> Self {
        self.parts.extend(other.parts);
        self
    }

    pub fn background(mut self, background: (u8, u8, u8)) -> Self {
        self.background = Some(background);
        self
    }

    pub fn selected(mut self) -> Self {
        for (_, ink, bold) in &mut self.parts {
            if *ink == Ink::Muted {
                *ink = Ink::Plain;
            }
            *bold = true;
        }
        self.background(SELECTION)
    }

    pub fn text(&self) -> String {
        self.parts.iter().map(|(text, _, _)| text.as_str()).collect()
    }

    pub fn width(&self) -> usize {
        measure_text_width(&self.text())
    }

    pub fn cell(mut self, width: usize) -> Self {
        let mut remaining = width;
        let truncated = self.width() > width;
        if truncated {
            remaining = remaining.saturating_sub(1);
        }
        let mut parts = Vec::new();
        for (text, ink, bold) in self.parts {
            let mut kept = String::new();
            let mut stopped = false;
            for c in text.chars().filter(|c| *c != '\n') {
                let length = measure_text_width(&c.to_string());
                if length > remaining {
                    stopped = true;
                    break;
                }
                remaining -= length;
                kept.push(c);
            }
            parts.push((kept, ink, bold));
            if remaining == 0 || stopped {
                break;
            }
        }
        self.parts = parts;
        if truncated && width > 0 {
            let (_, ink, bold) = self.parts.last().cloned().unwrap_or_default();
            self.parts.push(("…".into(), ink, bold));
        }
        let padding = width.saturating_sub(self.width());
        self.plain(" ".repeat(padding))
    }

    /// Render safe spans with a one-cell outer gutter around highlighted backgrounds.
    /// The gutter uses the existing margin, preserving text alignment and content width.
    pub fn render(&self, width: usize, margin: usize) -> String {
        // NO_COLOR removes the palette, but an interactive selection still benefits from
        // bold. Never force styling into redirected output or a dumb terminal.
        let monochrome_selection = self.background == Some(SELECTION)
            && !console::colors_enabled()
            && std::io::stderr().is_terminal()
            && std::env::var("TERM").as_deref() != Ok("dumb");
        let margin = margin.min(width / 2);
        let gutter = usize::from(self.background.is_some() && margin > 0);
        let content = self.clone().cell(width.saturating_sub(margin * 2));
        let mut parts = vec![(" ".repeat(margin - gutter), Ink::Plain, false)];
        parts.extend(content.parts);
        parts.push((" ".repeat(margin - gutter), Ink::Plain, false));
        let highlighted: String = parts
            .into_iter()
            .map(|(text, ink, bold)| {
                let mut style = if monochrome_selection {
                    Style::new().force_styling(true)
                } else {
                    ink.style()
                };
                if bold {
                    style = style.bold();
                }
                if !monochrome_selection && let Some((r, g, b)) = self.background {
                    style = style.on_true_color(r, g, b);
                }
                style.apply_to(text).to_string()
            })
            .collect();
        format!("{}{highlighted}{}", " ".repeat(gutter), " ".repeat(gutter))
    }

    /// Wrap at a word/identifier boundary where possible, retaining spans, empty paragraphs,
    /// and the indentation of continuation lines. No ANSI is involved in width calculations.
    pub fn wrapped(&self, width: usize) -> Vec<Self> {
        let width = width.max(1);
        let mut paragraphs = vec![Vec::new()];
        for (text, ink, bold) in &self.parts {
            for c in text.chars() {
                if c == '\n' {
                    paragraphs.push(Vec::new());
                } else {
                    paragraphs.last_mut().unwrap().push((c, *ink, *bold));
                }
            }
        }
        let mut lines = Vec::new();
        for paragraph in paragraphs {
            let indent = paragraph
                .iter()
                .take_while(|(c, _, _)| *c == ' ')
                .count()
                .min(width / 2);
            let mut offset = 0;
            loop {
                let continuation = if offset > 0 { indent } else { 0 };
                let available = width - continuation;
                let mut used = 0;
                let mut end = offset;
                let mut boundary = None;
                while let Some((c, _, _)) = paragraph.get(end) {
                    let length = measure_text_width(&c.to_string());
                    if used + length > available {
                        break;
                    }
                    used += length;
                    end += 1;
                    if (*c == ' ' || *c == '_') && used >= available / 2 {
                        boundary = Some(end);
                    }
                }
                if end < paragraph.len()
                    && let Some(boundary) = boundary
                {
                    end = boundary;
                }
                if end == offset && offset < paragraph.len() {
                    end += 1;
                }
                let mut line = Self::new(" ".repeat(continuation));
                for (c, ink, bold) in &paragraph[offset..end] {
                    if let Some((text, previous_ink, previous_bold)) = line.parts.last_mut()
                        && previous_ink == ink
                        && previous_bold == bold
                    {
                        text.push(*c);
                    } else {
                        line.parts.push((c.to_string(), *ink, *bold));
                    }
                }
                line.background = self.background;
                lines.push(line);
                offset = end;
                if offset >= paragraph.len() {
                    break;
                }
            }
        }
        lines
    }
}

/// Keep meaningful whitespace while discarding terminal escapes and other control characters.
/// Tabs are expanded to the terminal's conventional eight-column stops before layout.
pub(super) fn clean(value: &str) -> String {
    let value = console::strip_ansi_codes(value);
    let mut result = String::new();
    let mut column = 0;
    for c in value.chars() {
        match c {
            '\n' => {
                result.push(c);
                column = 0;
            }
            '\t' => {
                let spaces = 8 - column % 8;
                result.push_str(&" ".repeat(spaces));
                column += spaces;
            }
            c if c.is_control() => {}
            c => {
                result.push(c);
                column += measure_text_width(&c.to_string());
            }
        }
    }
    result
}

pub(super) fn panel(mut content: Vec<Line>, height: usize, width: usize) -> Vec<Line> {
    content.truncate(height.saturating_sub(2));
    content.resize(height.saturating_sub(2), Line::default());
    std::iter::once(Line::default())
        .chain(content)
        .chain(std::iter::once(Line::default()))
        // The renderer supplies a gutter and one shaded space on each side. Add a second
        // inner space here, balancing the panel's one-line vertical padding.
        .map(|line| {
            Line::new(" ")
                .append(line.cell(width.saturating_sub(6)))
                .plain(" ")
                .background(PANEL)
        })
        .collect()
}
