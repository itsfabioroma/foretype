//! How foretype prints its own messages. Styled on a terminal; plain text when piped,
//! with `NO_COLOR` set, or on a dumb terminal.

use std::fmt::Display;
use std::io::IsTerminal as _;
use std::os::fd::AsFd as _;

/// The wordmark from `assets/logo-*.svg`, one cell per 2×2 logo pixels: the part
/// already typed, the part typed ahead, and the cursor.
const WORDMARK: [[&str; 3]; 3] = [
    ["█▀▀▘▟▀▀▙▐▛▀▙▐▛▀▀", "▝▀█▀▘▜▖▗▛▐▛▀▙▐▛▀▀ ", "███"],
    ["█▀▘ █  █▐▛▀▙▐▛▀ ", "  █   ▜▛ ▐▛▀▘▐▛▀  ", "███"],
    ["▀   ▝▀▀▘▝▘ ▀▝▀▀▀", "  ▀   ▝▘ ▝▘  ▝▀▀▀ ", "▀▀▀"],
];

/// Width of the verb column in install and uninstall steps.
const VERB: usize = 12;
/// Width of the label column in `foretype status`.
const LABEL: usize = 15;

#[derive(Clone, Copy)]
pub struct Style {
    color: bool,
}

impl Style {
    pub fn stdout() -> Self {
        Self::new(std::io::stdout().is_terminal())
    }

    pub fn stderr() -> Self {
        Self::new(std::io::stderr().is_terminal())
    }

    fn new(terminal: bool) -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        let forced = ["CLICOLOR_FORCE", "FORCE_COLOR"]
            .into_iter()
            .any(|name| set(name) && std::env::var_os(name).is_some_and(|value| value != "0"));
        let dumb = std::env::var("TERM").map_or(true, |term| term.is_empty() || term == "dumb");
        Self {
            color: !set("NO_COLOR") && (forced || (terminal && !dumb)),
        }
    }

    fn paint(self, sgr: &str, text: impl Display) -> String {
        if self.color {
            format!("\x1b[{sgr}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(self, text: impl Display) -> String {
        self.paint("1", text)
    }

    pub fn dim(self, text: impl Display) -> String {
        self.paint("2", text)
    }

    pub fn green(self, text: impl Display) -> String {
        self.paint("32", text)
    }

    pub fn red(self, text: impl Display) -> String {
        self.paint("31", text)
    }

    /// "foretype" the way the logo writes it: typed, then typed ahead.
    pub fn name(self) -> String {
        format!("{}{}", self.bold("fore"), self.dim("type"))
    }

    /// The name and version, as the first line of a block.
    pub fn title(self) -> String {
        format!("{} {}", self.name(), self.dim(env!("CARGO_PKG_VERSION")))
    }

    /// The block wordmark with the version beside it, when there is color and room.
    pub fn wordmark(self) -> Option<String> {
        let wide =
            crate::pty::winsize(std::io::stdout().as_fd()).is_some_and(|size| size.ws_col >= 48);
        if !self.color || !wide {
            return None;
        }
        let mut text = String::new();
        for (row, [typed, ahead, cursor]) in WORDMARK.iter().enumerate() {
            text += &format!("  {}{}{}", self.bold(typed), self.dim(ahead), cursor);
            if row == WORDMARK.len() - 1 {
                text += &format!("  {}", self.dim(env!("CARGO_PKG_VERSION")));
            }
            text.push('\n');
        }
        Some(text)
    }

    /// One row of a two-column list under a heading, `left` padded to `width`.
    pub fn row(self, left: &str, right: impl Display, width: usize) -> String {
        let pad = " ".repeat(width.saturating_sub(left.chars().count()) + 3);
        let left = match left.split_once(' ') {
            Some(("sudo", rest)) => format!("{} {rest}", self.dim("sudo")),
            _ => match left.split_once('=') {
                Some((name, value)) => format!("{name}{}", self.dim(format!("={value}"))),
                None => left.to_owned(),
            },
        };
        format!("    {left}{pad}{}", self.dim(right))
    }

    /// A finished step: `✓ verb  what changed`.
    pub fn done(self, verb: &str, detail: impl Display) -> String {
        format!(
            "  {} {}{detail}",
            self.green("✓"),
            self.bold(pad(verb, VERB))
        )
    }

    /// A line of `foretype status`, marked good, bad or idle.
    pub fn check(self, mark: Mark, label: &str, value: impl Display) -> String {
        let mark = match mark {
            Mark::Good => self.green("✓"),
            Mark::Bad => self.red("✗"),
            Mark::Idle => self.dim("○"),
            Mark::None => " ".to_owned(),
        };
        format!("  {mark} {}{value}", self.dim(pad(label, LABEL)))
    }
}

#[derive(Clone, Copy)]
pub enum Mark {
    Good,
    Bad,
    Idle,
    None,
}

fn pad(label: &str, width: usize) -> String {
    format!("{label:<width$}")
}

/// Something went wrong: a headline, an optional explanation, and the command that
/// fixes it, printed to stderr.
pub struct Problem {
    headline: String,
    note: Option<String>,
    fix: Option<String>,
}

impl Problem {
    pub fn new(headline: impl Display) -> Self {
        Self {
            headline: headline.to_string(),
            note: None,
            fix: None,
        }
    }

    pub fn note(mut self, note: impl Display) -> Self {
        self.note = Some(note.to_string());
        self
    }

    pub fn fix(mut self, command: impl Display) -> Self {
        self.fix = Some(command.to_string());
        self
    }

    pub fn print(&self) {
        let style = Style::stderr();
        eprintln!();
        eprintln!("  {} {}", style.red("✗"), style.bold(&self.headline));
        if let Some(note) = &self.note {
            eprintln!("    {}", style.dim(note));
        }
        if let Some(fix) = &self.fix {
            eprintln!();
            eprintln!("    {}", style.bold(fix));
        }
        eprintln!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wordmark_is_a_rectangle() {
        for row in WORDMARK {
            assert_eq!(row.map(|part| part.chars().count()), [16, 18, 3]);
        }
    }

    #[test]
    fn plain_rows_line_up() {
        let plain = Style { color: false };
        let command = plain.row("sudo foretype install", "set up", 21);
        let variable = plain.row("FORETYPE=off", "start without", 21);
        assert_eq!(command.find("set up"), variable.find("start without"));
        assert_eq!(
            plain.done("registered", "a").find('a'),
            plain.done("kept", "b").find('b'),
        );
    }
}
