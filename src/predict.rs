//! Conservative local echo. Authoritative terminal cells are never mutated.
//!
//! Ported from the herdr remote-typing predictor. The first echo on each line is learned
//! from the remote side: nothing is drawn until the application has visibly echoed a
//! character at the cursor. This keeps ordinary password prompts dark, but it is not a
//! guarantee about the remote application's echo settings, which an SSH byte stream
//! does not expose.
//!
//! Two additions are specific to running as a byte-stream proxy instead of inside a
//! server-rendered client:
//! - Each line is verified against the real terminal with a cursor position report
//!   before anything is drawn, so a local model that disagrees with the terminal (wide
//!   glyph widths, reflow after resize) never places text in the wrong cell.
//! - A mismatch is given a short settle window before it discards confidence, because a
//!   frame can arrive split across reads and the intermediate state is not a real answer.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const DEFAULT_LIFETIME: Duration = Duration::from_millis(750);
pub const SETTLE: Duration = Duration::from_millis(30);
const MAX_TEXT: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: vt100::Color,
    pub bg: vt100::Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Style {
    pub fn of(cell: &vt100::Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellData {
    pub symbol: String,
    pub wide: bool,
    pub continuation: bool,
    pub style: Style,
}

impl CellData {
    pub fn of(cell: Option<&vt100::Cell>) -> Self {
        match cell {
            Some(cell) => Self {
                symbol: normalized(cell.contents()).to_owned(),
                wide: cell.is_wide(),
                continuation: cell.is_wide_continuation(),
                style: Style::of(cell),
            },
            None => Self {
                symbol: " ".into(),
                wide: false,
                continuation: false,
                style: Style::default(),
            },
        }
    }

    fn blank(&self) -> bool {
        self.symbol == " " && !self.continuation
    }
}

/// Never-written cells and written spaces look identical.
pub fn normalized(symbol: &str) -> &str {
    if symbol.is_empty() { " " } else { symbol }
}

/// Authoritative screen state, as reconstructed from the remote output stream.
#[derive(Clone, Copy)]
pub struct Surface<'a> {
    pub screen: &'a vt100::Screen,
    /// Changes whenever coordinates stop being comparable (resize, full reset).
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Text(String),
    Neutral,
    Control,
}

/// Cells to draw over the authoritative screen, all on one row, plus the caret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlay {
    pub row: u16,
    pub col: u16,
    pub cells: Vec<CellData>,
    pub caret: (u16, u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verify {
    Unrequested,
    Pending,
    Verified,
    Failed,
}

#[derive(Default, Clone, Debug)]
pub struct Stats {
    pub shown: u64,
    pub confirmed: u64,
    pub discarded: u64,
    pub echo_ms: Vec<f64>,
}

pub struct Predictor {
    line: Option<PredictedLine>,
    next_id: u64,
    lifetime: Duration,
    /// Underline characters the remote side has not echoed yet. Off by default:
    /// predictions are drawn exactly as the real echo will look.
    pub underline: bool,
    pub stats: Stats,
}

struct PredictedLine {
    id: u64,
    epoch: u64,
    size: (u16, u16),
    modes: (bool, bool),
    cursor_visible: bool,
    y: u16,
    x: u16,
    row: Vec<CellData>,
    pending: VecDeque<PendingCharacter>,
    trained: bool,
    verify: Verify,
    unsettled_since: Option<Instant>,
    shown: usize,
}

struct PendingCharacter {
    byte: u8,
    sent_at: Instant,
}

struct EligibleRow {
    size: (u16, u16),
    modes: (bool, bool),
    cursor_visible: bool,
    x: u16,
    y: u16,
    cells: Vec<CellData>,
}

struct SoftwareCursor<'a> {
    cell: &'a CellData,
    padding: &'a CellData,
}

fn same_style(left: &CellData, right: &CellData) -> bool {
    left.style == right.style
}

fn eligible_row(surface: Surface<'_>) -> Option<EligibleRow> {
    let screen = surface.screen;
    let (rows, cols) = screen.size();
    let (y, x) = screen.cursor_position();
    // A TUI may draw its own caret while keeping the terminal cursor hidden.
    // Coordinates alone do not grant confidence: an exact echo must still be learned.
    if x >= cols || y >= rows {
        return None;
    }
    let cells = (0..cols)
        .map(|col| CellData::of(screen.cell(y, col)))
        .collect();
    Some(EligibleRow {
        size: (rows, cols),
        modes: (
            screen.alternate_screen(),
            screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None,
        ),
        cursor_visible: !screen.hide_cursor(),
        x,
        y,
        cells,
    })
}

enum Comparison {
    Mismatch,
    Match { confirmed: usize, row: EligibleRow },
}

impl PredictedLine {
    fn software_cursor(&self) -> Option<SoftwareCursor<'_>> {
        if self.cursor_visible {
            return None;
        }
        let index = usize::from(self.x);
        let cell = self.row.get(index)?;
        let padding = self.row.get(index + 1)?;
        // A hidden hardware cursor can anchor an application-painted blank caret.
        // Its style may be inverse video or explicit colors. This is only a candidate:
        // observe must prove that this exact style moves with echoed text, leaving
        // ordinary padding style behind.
        (cell.blank() && padding.blank() && cell != padding)
            .then_some(SoftwareCursor { cell, padding })
    }

    fn matches_context(&self, surface: Surface<'_>, row: &EligibleRow) -> bool {
        self.epoch == surface.epoch
            && self.size == row.size
            && self.modes == row.modes
            && self.cursor_visible == row.cursor_visible
            && self.y == row.y
    }

    fn visible(&self) -> bool {
        self.trained
            && self.verify == Verify::Verified
            && self.unsettled_since.is_none()
            && !self.pending.is_empty()
    }

    fn deadline(&self, lifetime: Duration) -> Option<Instant> {
        // Only outstanding input can time out. Pausing at a confirmed, unchanged
        // prompt must not turn the next character into another network round trip.
        let pending = self
            .pending
            .front()
            .map(|pending| pending.sent_at + lifetime);
        let settle = self.unsettled_since.map(|since| since + SETTLE);
        match (pending, settle) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        }
    }

    fn compare(&self, surface: Surface<'_>) -> Comparison {
        let Some(row) = eligible_row(surface) else {
            return Comparison::Mismatch;
        };
        if !self.matches_context(surface, &row) {
            return Comparison::Mismatch;
        }
        let Some(confirmed) = row.x.checked_sub(self.x).map(usize::from) else {
            return Comparison::Mismatch;
        };
        if confirmed > self.pending.len() {
            return Comparison::Mismatch;
        }
        let start = usize::from(self.x);
        let software_cursor = self.software_cursor();
        let matches = row
            .cells
            .iter()
            .zip(&self.row)
            .enumerate()
            .all(|(index, (actual, base))| {
                if index >= start && index < start + confirmed {
                    let pending = &self.pending[index - start];
                    // Applications may style their echoed text. Geometry and the unchanged
                    // remainder of the row still have to agree exactly.
                    actual.symbol.as_bytes() == [pending.byte]
                        && !actual.continuation
                        && !actual.wide
                        && software_cursor
                            .as_ref()
                            .is_none_or(|cursor| same_style(actual, cursor.padding))
                } else if confirmed != 0 && index == start + confirmed && software_cursor.is_some()
                {
                    // Permit only the exact painted caret moving to its new anchor;
                    // other style/content changes anywhere on this row invalidate it.
                    software_cursor
                        .as_ref()
                        .is_some_and(|cursor| actual == cursor.cell && base == cursor.padding)
                } else {
                    actual == base
                }
            });
        if matches {
            Comparison::Match { confirmed, row }
        } else {
            Comparison::Mismatch
        }
    }
}

impl Predictor {
    pub fn new(lifetime: Duration) -> Self {
        Self {
            line: None,
            next_id: 1,
            lifetime,
            underline: false,
            stats: Stats::default(),
        }
    }

    pub fn has_pending(&self) -> bool {
        self.line
            .as_ref()
            .is_some_and(|line| !line.pending.is_empty())
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.line
            .as_ref()
            .and_then(|line| line.deadline(self.lifetime))
    }

    /// Returns whether removing the prediction needs a repaint.
    pub fn clear(&mut self) -> bool {
        let Some(line) = self.line.take() else {
            return false;
        };
        let visible = line.visible();
        self.stats.discarded += line.shown as u64;
        if line.shown > 0 {
            crate::log!("prediction discarded: {} shown characters", line.shown);
        }
        visible
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            crate::log!("prediction expired");
            self.clear()
        } else {
            false
        }
    }

    /// Hides predictions while a mismatch might be a partially received frame, and
    /// discards confidence once it outlives the settle window.
    fn unsettle(&mut self, now: Instant) -> bool {
        let Some(line) = self.line.as_mut() else {
            return false;
        };
        match line.unsettled_since {
            Some(since) if now >= since + SETTLE => self.clear(),
            Some(_) => false,
            None => {
                let visible = line.visible();
                line.unsettled_since = Some(now);
                visible
            }
        }
    }

    /// Reconcile against the authoritative screen after remote output. Output is not an
    /// input acknowledgment: unrelated rows can change while input is still travelling,
    /// and must not erase predictions prematurely.
    pub fn observe(&mut self, surface: Surface<'_>, now: Instant) -> bool {
        let repaint = self.expire(now);
        let Some(line) = self.line.as_ref() else {
            return repaint;
        };
        let (confirmed, row) = match line.compare(surface) {
            Comparison::Mismatch => return self.unsettle(now) || repaint,
            Comparison::Match { confirmed, row } => (confirmed, row),
        };
        let Some(line) = self.line.as_mut() else {
            return repaint;
        };
        let resettled = line.unsettled_since.take().is_some() && line.visible();
        if confirmed == 0 {
            self.account();
            return repaint || resettled;
        }
        let was_visible = line.visible();
        // A blank echoed space alone does not establish that the application displays
        // characters; some password inputs move the cursor while leaving blank cells.
        line.trained |= line
            .pending
            .iter()
            .take(confirmed)
            .any(|pending| pending.byte != b' ');
        for pending in line.pending.drain(..confirmed) {
            self.stats
                .echo_ms
                .push(now.saturating_duration_since(pending.sent_at).as_secs_f64() * 1000.0);
        }
        let shown = line.shown.min(confirmed);
        line.shown -= shown;
        self.stats.confirmed += shown as u64;
        line.row = row.cells;
        line.x = row.x;
        crate::log!(
            "confirmed {} (pending {}, shown {})",
            confirmed,
            line.pending.len(),
            shown
        );
        let repaint = repaint || resettled || was_visible || line.visible();
        self.account();
        repaint
    }

    /// Record input that was forwarded to the remote side. Enter, editing, paste, mouse
    /// and other controls invalidate confidence. Releases and reports have no echo.
    pub fn record_input(&mut self, surface: Surface<'_>, event: &Event, now: Instant) -> bool {
        let text = match event {
            Event::Neutral => return self.expire(now),
            Event::Control => return self.clear(),
            Event::Text(text) => text,
        };
        if text.is_empty()
            || text.len() > MAX_TEXT
            || !text.bytes().all(|byte| (b' '..=b'~').contains(&byte))
        {
            return self.clear();
        }
        let mut repaint = self.observe(surface, now);
        let Some(row) = eligible_row(surface) else {
            return self.clear() || repaint;
        };
        if self.line.is_none() {
            let id = self.next_id;
            self.next_id += 1;
            self.line = Some(PredictedLine {
                id,
                epoch: surface.epoch,
                size: row.size,
                modes: row.modes,
                cursor_visible: row.cursor_visible,
                y: row.y,
                x: row.x,
                row: row.cells.clone(),
                pending: VecDeque::new(),
                trained: false,
                verify: Verify::Unrequested,
                unsettled_since: None,
                shown: 0,
            });
        }
        let Some(line) = self.line.as_mut() else {
            return repaint;
        };
        let start = usize::from(line.x) + line.pending.len();
        let end = start.saturating_add(text.len());
        // Leave a spare cell: writing the final column can trigger terminal wrapping.
        // Free space is judged on the current row, which may have changed while an
        // earlier mismatch is still settling.
        if end >= row.cells.len()
            || !row.cells[start..end].iter().all(CellData::blank)
            || line.software_cursor().is_some_and(|cursor| {
                let after_cursor = usize::from(line.x) + 1;
                !row.cells[after_cursor..=end]
                    .iter()
                    .all(|cell| cell == cursor.padding)
            })
        {
            return self.clear() || repaint;
        }
        line.pending.extend(
            text.bytes()
                .map(|byte| PendingCharacter { byte, sent_at: now }),
        );
        repaint |= line.visible();
        self.account();
        repaint
    }

    /// Everything pending on a visible line is on screen; count it once.
    fn account(&mut self) {
        if let Some(line) = self.line.as_mut().filter(|line| line.visible())
            && line.shown < line.pending.len()
        {
            self.stats.shown += (line.pending.len() - line.shown) as u64;
            line.shown = line.pending.len();
        }
    }

    /// The line that still needs a cursor position check, with the anchor the real
    /// terminal must report.
    pub fn verification_request(&self) -> Option<(u64, (u16, u16))> {
        self.line
            .as_ref()
            .filter(|line| line.verify == Verify::Unrequested)
            .map(|line| (line.id, (line.y, line.x)))
    }

    pub fn verification_sent(&mut self, id: u64) {
        if let Some(line) = self.line.as_mut().filter(|line| line.id == id) {
            line.verify = Verify::Pending;
        }
    }

    pub fn verification_result(&mut self, id: u64, ok: bool) -> bool {
        let Some(line) = self.line.as_mut().filter(|line| line.id == id) else {
            return false;
        };
        line.verify = if ok { Verify::Verified } else { Verify::Failed };
        if !ok {
            crate::log!("cursor check failed: local model disagrees with terminal");
        }
        // Characters typed before the check completed become visible now.
        let visible = line.visible();
        self.account();
        visible
    }

    pub fn overlay(&self, surface: Surface<'_>) -> Option<Overlay> {
        let line = self.line.as_ref().filter(|line| line.visible())?;
        if line.epoch != surface.epoch {
            return None;
        }
        let (rows, cols) = surface.screen.size();
        let (y, x) = surface.screen.cursor_position();
        if (y, x) != (line.y, line.x) || y >= rows {
            return None;
        }
        let end = usize::from(x) + line.pending.len();
        if end >= usize::from(cols) {
            return None;
        }
        let software_cursor = line.software_cursor();
        let mut cells: Vec<CellData> = line
            .pending
            .iter()
            .enumerate()
            .map(|(index, pending)| {
                let base = &line.row[usize::from(x) + index];
                let mut style = software_cursor
                    .as_ref()
                    .map_or(base.style, |cursor| cursor.padding.style);
                style.underline |= self.underline;
                CellData {
                    symbol: char::from(pending.byte).to_string(),
                    wide: false,
                    continuation: false,
                    style,
                }
            })
            .collect();
        if let Some(cursor) = software_cursor {
            cells.push(cursor.cell.clone());
        }
        Some(Overlay {
            row: y,
            col: x,
            cells,
            caret: (y, end as u16),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: u16 = 30;

    /// A prompt on the second row with the cursor after `text`.
    fn screen(text: &str) -> vt100::Parser {
        let mut parser = vt100::Parser::new(4, COLS, 0);
        parser.process(format!("\r\n{text}").as_bytes());
        parser
    }

    /// Pi hides the hardware cursor and paints a blank inverse caret at its coordinates.
    fn software_cursor_screen(text: &str) -> vt100::Parser {
        let mut parser = vt100::Parser::new(4, COLS, 0);
        parser.process(format!("\x1b[?25l\r\n{text}\x1b[7m \x1b[m\x1b[D").as_bytes());
        parser
    }

    fn surface(parser: &vt100::Parser) -> Surface<'_> {
        Surface {
            screen: parser.screen(),
            epoch: 0,
        }
    }

    fn record(
        prediction: &mut Predictor,
        parser: &vt100::Parser,
        text: &str,
        now: Instant,
    ) -> bool {
        let repaint = prediction.record_input(surface(parser), &Event::Text(text.into()), now);
        verify(prediction) || repaint
    }

    fn underlined() -> Predictor {
        let mut prediction = Predictor::new(DEFAULT_LIFETIME);
        prediction.underline = true;
        prediction
    }

    /// Answers every cursor check as the real terminal would in these tests.
    fn verify(prediction: &mut Predictor) -> bool {
        match prediction.verification_request() {
            Some((id, _)) => {
                prediction.verification_sent(id);
                prediction.verification_result(id, true)
            }
            None => false,
        }
    }

    /// Row text as the user would see it, with the overlay applied.
    fn rendered(prediction: &Predictor, parser: &vt100::Parser) -> (String, (u16, u16)) {
        let screen = parser.screen();
        let mut cells: Vec<CellData> = (0..COLS)
            .map(|col| CellData::of(screen.cell(1, col)))
            .collect();
        let mut caret = screen.cursor_position();
        if let Some(overlay) = prediction.overlay(surface(parser)) {
            assert_eq!(overlay.row, 1);
            for (index, cell) in overlay.cells.into_iter().enumerate() {
                cells[usize::from(overlay.col) + index] = cell;
            }
            caret = overlay.caret;
        }
        (
            cells.iter().map(|cell| cell.symbol.as_str()).collect(),
            caret,
        )
    }

    fn row_text(prediction: &Predictor, parser: &vt100::Parser) -> String {
        rendered(prediction, parser).0
    }

    fn trained(now: Instant) -> (Predictor, vt100::Parser) {
        let mut prediction = underlined();
        record(&mut prediction, &screen("$ "), "a", now);
        let echoed = screen("$ a");
        prediction.observe(surface(&echoed), now + Duration::from_millis(100));
        (prediction, echoed)
    }

    #[test]
    fn prediction_waits_for_echo_then_displays_pending_suffix_without_mutating_authority() {
        let now = Instant::now();
        let mut prediction = underlined();
        let initial = screen("$ ");
        assert!(!record(&mut prediction, &initial, "abc", now));
        assert!(prediction.overlay(surface(&initial)).is_none());

        let echoed = screen("$ a");
        assert!(prediction.observe(surface(&echoed), now + Duration::from_millis(100)));
        let (text, caret) = rendered(&prediction, &echoed);
        assert!(text.starts_with("$ abc"));
        assert_eq!(caret, (1, 5));
        let overlay = prediction.overlay(surface(&echoed)).unwrap();
        assert_eq!(overlay.col, 3);
        assert!(overlay.cells.iter().all(|cell| cell.style.underline));
        assert_eq!(CellData::of(echoed.screen().cell(1, 3)).symbol, " ");
    }

    #[test]
    fn nothing_is_drawn_until_the_terminal_confirms_the_cursor() {
        let now = Instant::now();
        let mut prediction = Predictor::new(DEFAULT_LIFETIME);
        prediction.record_input(surface(&screen("$ ")), &Event::Text("ab".into()), now);
        let (id, anchor) = prediction.verification_request().unwrap();
        assert_eq!(anchor, (1, 2));
        prediction.verification_sent(id);
        assert!(prediction.verification_request().is_none());
        let echoed = screen("$ a");
        prediction.observe(surface(&echoed), now + Duration::from_millis(100));
        assert!(prediction.overlay(surface(&echoed)).is_none());
        assert!(prediction.verification_result(id, true));
        assert!(row_text(&prediction, &echoed).starts_with("$ ab"));

        let mut prediction = Predictor::new(DEFAULT_LIFETIME);
        prediction.record_input(surface(&screen("$ ")), &Event::Text("ab".into()), now);
        let (id, _) = prediction.verification_request().unwrap();
        prediction.verification_sent(id);
        assert!(!prediction.verification_result(id, false));
        prediction.observe(surface(&echoed), now + Duration::from_millis(100));
        assert!(prediction.overlay(surface(&echoed)).is_none());
    }

    #[test]
    fn prediction_learns_moving_software_cursor_and_moves_it_through_partial_echo() {
        let now = Instant::now();
        let mut prediction = underlined();
        let initial = software_cursor_screen("");
        assert!(!record(&mut prediction, &initial, "a", now));
        let echoed = software_cursor_screen("a");
        prediction.observe(surface(&echoed), now + Duration::from_millis(100));
        assert!(record(
            &mut prediction,
            &echoed,
            "bc",
            now + Duration::from_millis(110)
        ));
        let overlay = prediction.overlay(surface(&echoed)).unwrap();
        assert_eq!(overlay.col, 1);
        assert_eq!(overlay.cells.len(), 3);
        for cell in &overlay.cells[..2] {
            assert!(!cell.style.inverse);
            assert!(cell.style.underline);
        }
        assert!(overlay.cells[2].style.inverse);
        assert_eq!(overlay.caret, (1, 3));
        assert!(row_text(&prediction, &echoed).starts_with("abc "));

        let partial = software_cursor_screen("ab");
        prediction.observe(surface(&partial), now + Duration::from_millis(150));
        assert!(row_text(&prediction, &partial).starts_with("abc "));
        let complete = software_cursor_screen("abc");
        prediction.observe(surface(&complete), now + Duration::from_millis(200));
        assert!(!prediction.has_pending());
        assert!(prediction.overlay(surface(&complete)).is_none());
        assert!(record(
            &mut prediction,
            &complete,
            "d",
            now + Duration::from_secs(2)
        ));
        assert!(prediction.expire(now + Duration::from_secs(3)));
        assert!(prediction.overlay(surface(&complete)).is_none());
    }

    #[test]
    fn software_cursor_prediction_rejects_non_cursor_style_changes_and_wrong_echo() {
        let now = Instant::now();
        let changes: [&str; 4] = [
            "\x1b[2;1H!",
            "\x1b[2;1H\x1b[35ma\x1b[m",
            "\x1b[2;2H\x1b[45m \x1b[m",
            "\x1b[2;6H\x1b[35m.\x1b[m",
        ];
        for change in changes {
            let mut prediction = Predictor::new(DEFAULT_LIFETIME);
            record(&mut prediction, &software_cursor_screen(""), "a", now);
            let mut echoed = software_cursor_screen("a");
            echoed.process(format!("{change}\x1b[2;2H").as_bytes());
            prediction.observe(surface(&echoed), now + Duration::from_millis(100));
            prediction.observe(surface(&echoed), now + Duration::from_millis(100) + SETTLE);
            assert!(!prediction.has_pending(), "{change:?}");
            assert!(prediction.overlay(surface(&echoed)).is_none());
        }
    }

    #[test]
    fn software_cursor_without_echo_never_reveals_input() {
        let now = Instant::now();
        let prompt = software_cursor_screen("");
        let mut prediction = Predictor::new(DEFAULT_LIFETIME);
        record(&mut prediction, &prompt, "secret", now);
        prediction.observe(surface(&prompt), now + Duration::from_millis(100));
        assert!(prediction.overlay(surface(&prompt)).is_none());
        let mut moved = software_cursor_screen("");
        moved.process(b"\x1b[2;7H");
        prediction.observe(surface(&moved), now + Duration::from_millis(200));
        prediction.observe(surface(&moved), now + Duration::from_millis(200) + SETTLE);
        assert!(!prediction.has_pending());
        assert!(prediction.overlay(surface(&moved)).is_none());
    }

    #[test]
    fn partial_and_complete_echo_retire_exactly_the_confirmed_prefix() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        assert!(record(
            &mut prediction,
            &echoed,
            "bcd",
            now + Duration::from_millis(110)
        ));
        let partial = screen("$ abc");
        prediction.observe(surface(&partial), now + Duration::from_millis(200));
        assert!(row_text(&prediction, &partial).starts_with("$ abcd"));
        let complete = screen("$ abcd");
        prediction.observe(surface(&complete), now + Duration::from_millis(300));
        assert!(!prediction.has_pending());
        assert!(prediction.overlay(surface(&complete)).is_none());
        assert_eq!(prediction.stats.confirmed, 3);
        assert_eq!(prediction.stats.discarded, 0);
    }

    #[test]
    fn subsequent_input_uses_the_end_of_pending_text() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        record(
            &mut prediction,
            &echoed,
            "c",
            now + Duration::from_millis(120),
        );
        assert!(row_text(&prediction, &echoed).starts_with("$ abc"));
    }

    #[test]
    fn unrelated_output_does_not_count_as_input_acknowledgment() {
        let now = Instant::now();
        let (mut prediction, mut echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        echoed.process(b"\x1b7\x1b[1;1H!\x1b8");
        prediction.observe(surface(&echoed), now + Duration::from_millis(120));
        assert!(prediction.has_pending());
        assert!(row_text(&prediction, &echoed).starts_with("$ ab"));
    }

    #[test]
    fn mismatched_echo_rolls_back_after_settling_and_requires_new_training() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        let mismatch = screen("$ a!");
        assert!(prediction.observe(surface(&mismatch), now + Duration::from_millis(120)));
        assert!(prediction.overlay(surface(&mismatch)).is_none());
        assert!(prediction.has_pending());
        prediction.observe(
            surface(&mismatch),
            now + Duration::from_millis(120) + SETTLE,
        );
        assert!(!prediction.has_pending());
        assert!(!record(
            &mut prediction,
            &mismatch,
            "c",
            now + Duration::from_millis(200)
        ));
        assert!(prediction.overlay(surface(&mismatch)).is_none());
        assert_eq!(prediction.stats.discarded, 1);
    }

    #[test]
    fn split_frame_mismatch_recovers_when_the_frame_completes() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "bc",
            now + Duration::from_millis(110),
        );
        // The application erased the row and has not redrawn it yet.
        let mut torn = screen("$ a");
        torn.process(b"\r\x1b[K");
        prediction.observe(surface(&torn), now + Duration::from_millis(120));
        assert!(prediction.overlay(surface(&torn)).is_none());
        let redrawn = screen("$ ab");
        assert!(prediction.observe(surface(&redrawn), now + Duration::from_millis(125)));
        assert!(row_text(&prediction, &redrawn).starts_with("$ abc"));
    }

    #[test]
    fn pending_deadline_does_not_extend_with_more_input_or_unchanged_frames() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        let sent = now + Duration::from_millis(110);
        record(&mut prediction, &echoed, "b", sent);
        record(
            &mut prediction,
            &echoed,
            "c",
            sent + Duration::from_millis(500),
        );
        prediction.observe(surface(&echoed), sent + Duration::from_millis(600));
        assert_eq!(prediction.deadline(), Some(sent + DEFAULT_LIFETIME));
        assert!(prediction.expire(sent + DEFAULT_LIFETIME));
        assert!(!prediction.has_pending());
        assert!(prediction.overlay(surface(&echoed)).is_none());
    }

    #[test]
    fn non_echoing_password_input_never_appears_and_expires() {
        let now = Instant::now();
        let mut prediction = Predictor::new(DEFAULT_LIFETIME);
        let prompt = screen("Password: ");
        assert!(!record(&mut prediction, &prompt, "secret", now));
        prediction.observe(surface(&prompt), now + Duration::from_millis(500));
        assert!(prediction.overlay(surface(&prompt)).is_none());
        prediction.expire(now + DEFAULT_LIFETIME);
        assert!(!prediction.has_pending());
    }

    #[test]
    fn cursor_motion_over_blank_space_does_not_train_echo() {
        let now = Instant::now();
        let mut prediction = Predictor::new(DEFAULT_LIFETIME);
        record(&mut prediction, &screen("$ "), " ", now);
        let blank_echo = screen("$  ");
        prediction.observe(surface(&blank_echo), now + Duration::from_millis(100));
        assert!(!record(
            &mut prediction,
            &blank_echo,
            "secret",
            now + Duration::from_millis(110)
        ));
        assert!(prediction.overlay(surface(&blank_echo)).is_none());
    }

    #[test]
    fn controls_and_invalid_text_clear_pending_text_and_confidence() {
        let now = Instant::now();
        for event in [
            Event::Control,
            Event::Text("é".into()),
            Event::Text("a".repeat(MAX_TEXT + 1)),
        ] {
            let (mut prediction, echoed) = trained(now);
            record(
                &mut prediction,
                &echoed,
                "b",
                now + Duration::from_millis(110),
            );
            assert!(prediction.record_input(
                surface(&echoed),
                &event,
                now + Duration::from_millis(120)
            ));
            assert!(!prediction.has_pending());
            assert!(!record(
                &mut prediction,
                &echoed,
                "c",
                now + Duration::from_millis(130)
            ));
        }
    }

    #[test]
    fn context_changes_clear_predictions_after_settling() {
        let now = Instant::now();
        let changes: [&str; 5] = [
            "\x1b[?1049h\x1b[2;4H",
            "\x1b[?1000h",
            "\x1b[?25l",
            "\x1b[3;4H",
            "\r\n\r\n\r\n\x1b[A\x1b[A\x1b[2;4H",
        ];
        for change in changes {
            let (mut prediction, mut echoed) = trained(now);
            record(
                &mut prediction,
                &echoed,
                "b",
                now + Duration::from_millis(110),
            );
            echoed.process(change.as_bytes());
            assert!(prediction.observe(surface(&echoed), now + Duration::from_millis(120)));
            prediction.observe(surface(&echoed), now + Duration::from_millis(120) + SETTLE);
            assert!(!prediction.has_pending(), "{change:?}");
        }
        let (mut prediction, echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        let moved_epoch = Surface {
            screen: echoed.screen(),
            epoch: 1,
        };
        assert!(prediction.overlay(moved_epoch).is_none());
    }

    #[test]
    fn occupied_cells_and_terminal_right_edge_are_never_predicted() {
        let now = Instant::now();
        let (mut prediction, mut echoed) = trained(now);
        echoed.process(b"\x1b[2;5H!\x1b[2;4H");
        assert!(!record(
            &mut prediction,
            &echoed,
            "bc",
            now + Duration::from_millis(110)
        ));
        assert!(!prediction.has_pending());
        let initial = screen(&"x".repeat(usize::from(COLS) - 2));
        let mut prediction = Predictor::new(DEFAULT_LIFETIME);
        assert!(!record(&mut prediction, &initial, "ab", now));
        assert!(!prediction.has_pending());
    }

    #[test]
    fn neutral_input_and_idle_pause_preserve_confirmed_prompt_confidence() {
        let now = Instant::now();
        let (mut prediction, echoed) = trained(now);
        assert!(!prediction.record_input(
            surface(&echoed),
            &Event::Neutral,
            now + Duration::from_millis(110)
        ));
        assert!(record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(120)
        ));
        assert!(!prediction.record_input(
            surface(&echoed),
            &Event::Neutral,
            now + Duration::from_millis(130)
        ));
        assert!(prediction.has_pending());
        let (mut prediction, echoed) = trained(now);
        assert_eq!(prediction.deadline(), None);
        prediction.expire(now + Duration::from_millis(100) + DEFAULT_LIFETIME);
        assert!(record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_secs(1)
        ));
    }

    #[test]
    fn cursor_movement_without_matching_characters_does_not_confirm_pending_input() {
        let now = Instant::now();
        let (mut prediction, mut echoed) = trained(now);
        record(
            &mut prediction,
            &echoed,
            "b",
            now + Duration::from_millis(110),
        );
        echoed.process(b"\x1b[C");
        assert!(prediction.observe(surface(&echoed), now + Duration::from_millis(120)));
        prediction.observe(surface(&echoed), now + Duration::from_millis(120) + SETTLE);
        assert!(!prediction.has_pending());
    }
}
