//! The proxy between the local terminal and the remote session.
//!
//! Remote output is forwarded byte for byte; nothing is re-rendered. A shadow `vt100`
//! model is fed the same bytes and is what the predictor reasons about. A second model
//! mirrors what the real terminal shows, including our overlay, so leftovers can be
//! repaired wherever later output moves them.
//!
//! Overlay protocol: we save the real cursor (DECSC), paint predicted cells, and park
//! the cursor after them. Before any remote byte is forwarded the real cursor, pen and
//! character set are restored (DECRC), so the remote stream always lands exactly where
//! the remote application expects. Injection only happens between escape sequences,
//! outside application synchronized updates, and never while the application itself
//! depends on its saved cursor.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::input::{InputParser, Key, Token};
use crate::predict::{CellData, Event, Overlay, Predictor, Style, Surface, normalized};
use crate::scan::Scanner;

/// Incomplete output sequences are held back at most this long.
const HOLD_TIMEOUT: Duration = Duration::from_millis(30);
/// ... or until this many bytes (large image or clipboard payloads).
const HOLD_LIMIT: usize = 256 * 1024;
/// A dangling escape prefix on input is a lone Escape key after this long.
const INPUT_TIMEOUT: Duration = Duration::from_millis(10);
/// A cursor check the terminal never answered.
const QUERY_TIMEOUT: Duration = Duration::from_secs(1);
/// Repair leftovers without DECSC if the application holds its saved cursor this long.
const FALLBACK_REPAIR: Duration = Duration::from_millis(100);
/// Prediction is shown once the foreground program's typical echo is slower than this
/// (milliseconds), and stops again below the lower bound. Local programs echo in about
/// a millisecond and are never touched.
const BOOST_ABOVE_MS: f64 = 30.0;
const BOOST_BELOW_MS: f64 = 12.0;
/// A keystroke waits for the background model to catch up only if at most this much
/// output is still unparsed (well under a millisecond); otherwise it goes through
/// unmeasured.
const CATCH_UP_LIMIT: usize = 64 * 1024;

const SAVE: &[u8] = b"\x1b7";
const RESTORE: &[u8] = b"\x1b8";
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
const CURSOR_QUERY: &[u8] = b"\x1b[6n";

/// Messages to the thread that keeps the screen model current while nobody types.
enum Parse {
    Bytes(Vec<u8>),
    Resize(u16, u16),
    Give(Box<vt100::Parser>),
    Take(mpsc::Sender<Box<vt100::Parser>>),
}

struct Background {
    to_parser: mpsc::Sender<Parse>,
    backlog: Arc<AtomicUsize>,
}

fn parse_in_background(messages: mpsc::Receiver<Parse>, backlog: Arc<AtomicUsize>) {
    let mut parser: Option<Box<vt100::Parser>> = None;
    while let Ok(message) = messages.recv() {
        match message {
            Parse::Bytes(bytes) => {
                if let Some(parser) = parser.as_mut() {
                    parser.process(&bytes);
                }
                backlog.fetch_sub(bytes.len(), Ordering::Release);
            }
            Parse::Resize(rows, cols) => {
                if let Some(parser) = parser.as_mut() {
                    parser.screen_mut().set_size(rows, cols);
                }
            }
            Parse::Give(given) => parser = Some(given),
            Parse::Take(reply) => {
                if let Some(taken) = parser.take() {
                    let _ = reply.send(taken);
                }
            }
        }
    }
}

#[derive(Default)]
pub struct Io {
    pub terminal: Vec<u8>,
    pub child: Vec<u8>,
}

enum Query {
    App,
    /// A per-line check before drawing.
    Ours {
        line: u64,
        expected: (u16, u16),
        outputs: u64,
        sent: Instant,
    },
    /// Learn where the cursor is (at start, after a resize).
    Sync {
        outputs: u64,
        sent: Instant,
    },
}

pub struct Session {
    shadow: vt100::Parser,
    display: vt100::Parser,
    scanner: Scanner,
    input: InputParser,
    pub predictor: Predictor,
    enabled: bool,
    epoch: u64,
    held: Vec<u8>,
    held_since: Option<Instant>,
    terminal_ground: bool,
    input_since: Option<Instant>,
    /// The display model may differ from the shadow model.
    dirty: bool,
    dirty_since: Option<Instant>,
    /// We hold the terminal's saved-cursor slot and the cursor is parked at `caret`.
    saved: bool,
    caret: Option<(u16, u16)>,
    queries: VecDeque<Query>,
    cursor_checks_failed: u64,
    /// Remote output chunks forwarded so far; a cursor report only describes the model
    /// if no output was forwarded after its query.
    outputs: u64,
    terminal_answers: bool,
    /// The foreground program echoes slowly enough to be worth predicting. Otherwise
    /// foretype only measures, and is a plain pipe.
    boosting: bool,
    /// Foreground process group; a new one is a different program, measured afresh.
    foreground: i32,
    /// Smoothed echo latency of the foreground program, in milliseconds.
    echo_ms: Option<f64>,
    samples_seen: usize,
    /// Input bytes already forwarded ahead of their tokens.
    input_sent: usize,
    background: Option<Background>,
    /// The shadow model lives on the background thread right now.
    offloaded: bool,
    size: (u16, u16),
}

impl Session {
    pub fn new(rows: u16, cols: u16, enabled: bool, lifetime: Duration) -> Self {
        Self {
            shadow: vt100::Parser::new(rows.max(1), cols.max(1), 0),
            display: vt100::Parser::new(rows.max(1), cols.max(1), 0),
            scanner: Scanner::default(),
            input: InputParser::default(),
            predictor: Predictor::new(lifetime),
            enabled,
            epoch: 0,
            held: Vec::new(),
            held_since: None,
            terminal_ground: true,
            input_since: None,
            dirty: false,
            dirty_since: None,
            saved: false,
            caret: None,
            queries: VecDeque::new(),
            cursor_checks_failed: 0,
            outputs: 0,
            terminal_answers: false,
            boosting: false,
            foreground: 0,
            echo_ms: None,
            samples_seen: 0,
            input_sent: 0,
            background: None,
            offloaded: false,
            size: (rows.max(1), cols.max(1)),
        }
    }

    /// Lets heavy output stream at full speed: while nothing is typed or boosted, the
    /// screen model is parsed on another thread.
    pub fn parse_in_background(&mut self) {
        let (to_parser, messages) = mpsc::channel();
        let backlog = Arc::new(AtomicUsize::new(0));
        let shared = backlog.clone();
        let spawned = std::thread::Builder::new()
            .name("foretype-model".into())
            .spawn(move || parse_in_background(messages, shared));
        if spawned.is_ok() {
            self.background = Some(Background { to_parser, backlog });
        }
    }

    /// Hands the model to the background thread when nothing needs it here.
    fn maybe_offload(&mut self) {
        let Some(background) = &self.background else {
            return;
        };
        let quiet = !self.offloaded
            && !self.boosting
            && !self.dirty
            && !self.saved
            && self.held.is_empty()
            && !self.input.has_carry()
            && !self.predictor.has_pending()
            && !self
                .queries
                .iter()
                .any(|query| !matches!(query, Query::App));
        if !quiet {
            return;
        }
        let parser = std::mem::replace(&mut self.shadow, vt100::Parser::new(1, 1, 0));
        if background
            .to_parser
            .send(Parse::Give(Box::new(parser)))
            .is_ok()
        {
            self.offloaded = true;
        } else {
            self.lose_model();
        }
    }

    /// Takes the model back once the background thread has parsed everything sent so
    /// far. Returns false if that would make a keystroke wait.
    fn catch_up(&mut self) -> bool {
        if !self.offloaded {
            return true;
        }
        let Some(background) = &self.background else {
            return true;
        };
        if background.backlog.load(Ordering::Acquire) > CATCH_UP_LIMIT {
            return false;
        }
        let (reply, taken) = mpsc::channel();
        let parser = background
            .to_parser
            .send(Parse::Take(reply))
            .ok()
            .and_then(|()| taken.recv().ok());
        self.offloaded = false;
        match parser {
            Some(parser) => self.shadow = *parser,
            None => self.lose_model(),
        }
        true
    }

    /// The background thread is gone; start over with an empty model. Cursor checks
    /// keep anything from being drawn until it agrees with the terminal again.
    fn lose_model(&mut self) {
        crate::log!("background model lost");
        self.background = None;
        self.offloaded = false;
        self.shadow = vt100::Parser::new(self.size.0, self.size.1, 0);
        self.epoch += 1;
        self.predictor.clear();
    }

    /// A different job came to the foreground of the terminal: measure it from scratch.
    pub fn set_foreground(&mut self, group: i32, now: Instant, io: &mut Io) {
        if group == self.foreground {
            return;
        }
        self.foreground = group;
        self.echo_ms = None;
        self.samples_seen = self.predictor.stats.echo_ms.len();
        self.predictor.clear();
        if self.boosting {
            let mut out = Vec::new();
            self.stop_boosting(now, &mut out);
            self.emit(out, io);
        }
    }

    /// Learns the foreground program's echo latency from confirmed keystrokes and turns
    /// drawing on or off.
    fn gate(&mut self, now: Instant, out: &mut Vec<u8>) {
        let samples = &self.predictor.stats.echo_ms;
        if samples.len() == self.samples_seen {
            return;
        }
        for &sample in &samples[self.samples_seen..] {
            self.echo_ms = Some(match self.echo_ms {
                Some(smoothed) => smoothed * 0.7 + sample * 0.3,
                None => sample,
            });
        }
        self.samples_seen = samples.len();
        let echo = self.echo_ms.unwrap_or(0.0);
        if !self.boosting && echo > BOOST_ABOVE_MS {
            crate::log!("echo {echo:.0} ms: boosting");
            self.boosting = true;
        } else if self.boosting && echo < BOOST_BELOW_MS {
            crate::log!("echo {echo:.0} ms: not boosting");
            self.stop_boosting(now, out);
        }
    }

    fn stop_boosting(&mut self, now: Instant, out: &mut Vec<u8>) {
        self.boosting = false;
        self.predictor.clear();
        self.reconcile(now, out);
    }

    #[cfg(test)]
    pub fn take_model_back(&mut self) {
        assert!(self.catch_up());
    }

    #[cfg(test)]
    pub fn boost(&mut self) {
        self.boosting = true;
        self.echo_ms = Some(100.0);
    }

    /// Asks the terminal where the cursor is. foretype usually starts below existing
    /// output, and the remote side draws relative to wherever that is.
    pub fn sync_cursor(&mut self, now: Instant, io: &mut Io) {
        if !self.enabled || !self.boosting || !self.can_inject() || self.saved {
            return;
        }
        io.terminal.extend_from_slice(CURSOR_QUERY);
        self.queries.push_back(Query::Sync {
            outputs: self.outputs,
            sent: now,
        });
    }

    fn set_model_cursor(&mut self, (row, col): (u16, u16)) {
        let (rows, cols) = self.shadow.screen().size();
        if row >= rows || col >= cols {
            return;
        }
        crate::log!("model cursor synced to {:?}", (row, col));
        let position = format!("\x1b[{};{}H", row + 1, col + 1);
        self.shadow.process(position.as_bytes());
        if self.dirty {
            self.display.process(position.as_bytes());
        }
    }

    pub fn cursor_checks_failed(&self) -> u64 {
        self.cursor_checks_failed
    }

    fn surface(&self) -> Surface<'_> {
        Surface {
            screen: self.shadow.screen(),
            epoch: self.epoch,
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        let query = self.queries.iter().find_map(|query| match query {
            Query::Ours { sent, .. } | Query::Sync { sent, .. } => Some(*sent + QUERY_TIMEOUT),
            Query::App => None,
        });
        let repair = self
            .dirty_since
            .filter(|_| self.dirty && !self.can_inject())
            .map(|since| since + FALLBACK_REPAIR);
        [
            self.held_since.map(|since| since + HOLD_TIMEOUT),
            self.input_since.map(|since| since + INPUT_TIMEOUT),
            self.predictor.deadline(),
            query,
            repair,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Our bytes may be interleaved with the remote stream right now.
    fn can_inject(&self) -> bool {
        self.terminal_ground && !self.scanner.sync() && !self.scanner.app_saved_cursor()
    }

    /// Prediction can stay alive: the stream is understood and we may draw later.
    fn prediction_context(&self) -> bool {
        self.enabled
            && self.terminal_ground
            && !self.scanner.app_saved_cursor()
            && self.scanner.charset_ascii()
    }

    /// Prediction can be drawn right now.
    fn prediction_allowed(&self) -> bool {
        self.prediction_context() && !self.scanner.sync()
    }

    // ---- remote output -------------------------------------------------------------

    pub fn output(&mut self, bytes: &[u8], now: Instant, io: &mut Io) {
        if self.offloaded {
            self.output_offloaded(bytes, io);
            return;
        }
        self.output_here(bytes, now, io);
        self.maybe_offload();
    }

    /// Nothing is typed or boosted: forward at once, and let the background thread
    /// keep the model current.
    fn output_offloaded(&mut self, bytes: &[u8], io: &mut Io) {
        self.scanner.feed(bytes);
        let model = self.scanner.take_model_all();
        self.terminal_ground = self.scanner.at_ground();
        self.outputs += 1;
        for _ in 0..self.scanner.take_cpr_queries() {
            self.queries.push_back(Query::App);
        }
        if self.scanner.take_reset() {
            self.epoch += 1;
        }
        io.terminal.extend_from_slice(bytes);
        if let Some(background) = &self.background {
            background.backlog.fetch_add(model.len(), Ordering::Release);
            if background.to_parser.send(Parse::Bytes(model)).is_err() {
                self.lose_model();
            }
        }
    }

    fn output_here(&mut self, bytes: &[u8], now: Instant, io: &mut Io) {
        let sync_before = self.scanner.sync();
        let idle = !self.boosting && !self.dirty && self.held.is_empty();
        if !self.terminal_ground || idle {
            // Not boosting, or a partial sequence was already flushed and the terminal is
            // mid-sequence: there is nothing to gain from holding back. The models still
            // follow along, so they are current when ssh comes to the foreground.
            self.scanner.feed(bytes);
            let model = self.scanner.take_model_all();
            self.terminal_ground = self.scanner.at_ground();
            self.forward(bytes, &model, sync_before, now, io);
            return;
        }
        let start = self.held.len();
        self.held.extend_from_slice(bytes);
        let safe = self.scanner.feed(bytes);
        if safe > 0 {
            let chunk: Vec<u8> = self.held.drain(..start + safe).collect();
            let model = self.scanner.take_model();
            self.held_since = (!self.held.is_empty()).then_some(now);
            self.forward(&chunk, &model, sync_before, now, io);
        } else if self.held_since.is_none() {
            self.held_since = Some(now);
        }
        if self.held.len() > HOLD_LIMIT {
            self.flush_held(now, io);
        }
    }

    fn flush_held(&mut self, now: Instant, io: &mut Io) {
        if self.held.is_empty() {
            return;
        }
        let chunk = std::mem::take(&mut self.held);
        self.held_since = None;
        // The held tail never contains a complete sequence, so no state changed, but
        // once it is forwarded the terminal itself is mid-sequence.
        let sync = self.scanner.sync();
        let model = self.scanner.take_model_all();
        self.terminal_ground = self.scanner.at_ground();
        self.forward(&chunk, &model, sync, now, io);
    }

    /// Sends `chunk` to the terminal; `model` is the same output as the models read it.
    fn forward(
        &mut self,
        chunk: &[u8],
        model: &[u8],
        sync_before: bool,
        now: Instant,
        io: &mut Io,
    ) {
        self.outputs += 1;
        let mut out = Vec::with_capacity(chunk.len() + 64);
        let restored = self.saved;
        if restored {
            out.extend_from_slice(RESTORE);
            self.display.process(RESTORE);
            self.saved = false;
            self.caret = None;
        }
        out.extend_from_slice(chunk);
        self.shadow.process(model);
        if self.dirty {
            self.display.process(model);
        }
        for _ in 0..self.scanner.take_cpr_queries() {
            self.queries.push_back(Query::App);
        }
        if self.scanner.take_reset() {
            self.epoch += 1;
            self.predictor.clear();
        }
        let mut tail = Vec::new();
        self.update(now, &mut tail);
        let injected = restored || !tail.is_empty();
        out.extend_from_slice(&tail);
        if injected && !sync_before && !self.scanner.sync() {
            io.terminal.extend_from_slice(SYNC_BEGIN);
            io.terminal.extend_from_slice(&out);
            io.terminal.extend_from_slice(SYNC_END);
        } else {
            io.terminal.extend_from_slice(&out);
        }
    }

    /// After remote output: learn from it, then repair and repaint.
    fn update(&mut self, now: Instant, out: &mut Vec<u8>) {
        if !self.prediction_context() {
            self.predictor.clear();
        } else if !self.scanner.sync() {
            // Mid-frame state is transient; wait for the application to finish drawing.
            let surface = Surface {
                screen: self.shadow.screen(),
                epoch: self.epoch,
            };
            self.predictor.observe(surface, now);
        }
        self.gate(now, out);
        self.reconcile(now, out);
        self.request_cursor_check(now, out);
    }

    // ---- local input ---------------------------------------------------------------

    pub fn input(&mut self, bytes: &[u8], now: Instant, io: &mut Io) {
        if !self.catch_up() {
            // Output is pouring in and the model is behind: this keystroke goes out
            // unmeasured rather than waiting.
            io.child.extend_from_slice(bytes);
            let _ = self.input.feed(bytes);
            let _ = self.input.flush();
            self.predictor.clear();
            return;
        }
        // Input is only held back while the terminal may be answering one of our own
        // cursor queries, so that the answer can be taken out. Otherwise it goes out
        // as it arrives, and tokens are only used to classify it.
        let ours = self
            .queries
            .iter()
            .any(|query| !matches!(query, Query::App));
        if !ours {
            io.child.extend_from_slice(bytes);
            self.input_sent += bytes.len();
        }
        let tokens = self.input.feed(bytes);
        self.tokens(tokens, now, io);
        self.input_since = self.input.has_carry().then_some(now);
        self.maybe_offload();
    }

    fn send_input(&mut self, bytes: &[u8], io: &mut Io) {
        let sent = self.input_sent.min(bytes.len());
        self.input_sent -= sent;
        io.child.extend_from_slice(&bytes[sent..]);
    }

    fn tokens(&mut self, tokens: Vec<Token>, now: Instant, io: &mut Io) {
        if tokens.is_empty() {
            return;
        }
        for token in tokens {
            if let Key::CursorReport { row, col } = token.key {
                let actual = (row.saturating_sub(1), col.saturating_sub(1));
                match self.queries.pop_front() {
                    Some(Query::Ours {
                        line,
                        expected,
                        outputs,
                        ..
                    }) => {
                        self.terminal_answers = true;
                        let ok = actual == expected;
                        if !ok {
                            self.cursor_checks_failed += 1;
                            crate::log!("cursor check: model {expected:?}, terminal {actual:?}");
                            if outputs == self.outputs {
                                self.set_model_cursor(actual);
                            }
                        }
                        self.predictor.verification_result(line, ok);
                        self.input_sent -= self.input_sent.min(token.bytes.len());
                        continue;
                    }
                    Some(Query::Sync { outputs, .. }) => {
                        self.terminal_answers = true;
                        if outputs == self.outputs {
                            self.set_model_cursor(actual);
                        }
                        self.input_sent -= self.input_sent.min(token.bytes.len());
                        continue;
                    }
                    // The application asked; the answer is its own.
                    Some(Query::App) | None => {}
                }
            }
            self.send_input(&token.bytes, io);
            let event = match token.key {
                Key::Text(text) if self.prediction_allowed() && !self.scanner.sync() => {
                    Event::Text(text)
                }
                Key::Text(_) | Key::Control => Event::Control,
                Key::Neutral | Key::CursorReport { .. } => Event::Neutral,
            };
            let surface = Surface {
                screen: self.shadow.screen(),
                epoch: self.epoch,
            };
            self.predictor.record_input(surface, &event, now);
        }
        let mut out = Vec::new();
        self.gate(now, &mut out);
        self.reconcile(now, &mut out);
        self.request_cursor_check(now, &mut out);
        self.emit(out, io);
    }

    // ---- timers and resize ---------------------------------------------------------

    pub fn tick(&mut self, now: Instant, io: &mut Io) {
        if self.offloaded && self.input_since.is_none() {
            // Nothing pending can need the model while it is offloaded.
            return;
        }
        if !self.catch_up() {
            return;
        }
        if self
            .held_since
            .is_some_and(|since| now >= since + HOLD_TIMEOUT)
        {
            self.flush_held(now, io);
        }
        if self
            .input_since
            .is_some_and(|since| now >= since + INPUT_TIMEOUT)
        {
            self.input_since = None;
            let tokens = self.input.flush();
            self.tokens(tokens, now, io);
        }
        let mut expired = Vec::new();
        let mut silent = false;
        self.queries.retain(|query| match query {
            Query::Ours { line, sent, .. } if now >= *sent + QUERY_TIMEOUT => {
                expired.push(*line);
                silent = true;
                false
            }
            Query::Sync { sent, .. } if now >= *sent + QUERY_TIMEOUT => {
                silent = true;
                false
            }
            _ => true,
        });
        for line in expired {
            crate::log!("cursor check timed out");
            self.predictor.verification_result(line, false);
        }
        if silent && !self.terminal_answers && self.enabled {
            // Something with a pty but no terminal emulator (expect, script). Never
            // write anything of our own into its output again.
            crate::log!("terminal does not answer cursor queries; passing through");
            self.enabled = false;
            self.predictor.clear();
        }
        self.predictor.expire(now);
        let mut out = Vec::new();
        self.reconcile(now, &mut out);
        if out.is_empty()
            && self.dirty
            && !self.can_inject()
            && self.terminal_ground
            && !self.scanner.sync()
            && self
                .dirty_since
                .is_some_and(|since| now >= since + FALLBACK_REPAIR)
        {
            self.repair_without_save(&mut out);
        }
        self.emit(out, io);
        self.maybe_offload();
    }

    pub fn resize(&mut self, rows: u16, cols: u16, now: Instant, io: &mut Io) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        if self.offloaded {
            self.epoch += 1;
            self.display.screen_mut().set_size(rows, cols);
            if let Some(background) = &self.background {
                let _ = background.to_parser.send(Parse::Resize(rows, cols));
            }
            return;
        }
        // The terminal reflows on its own; coordinates from before are not comparable.
        self.predictor.clear();
        self.epoch += 1;
        if self.saved && self.terminal_ground {
            io.terminal.extend_from_slice(RESTORE);
        }
        self.saved = false;
        self.caret = None;
        self.dirty = false;
        self.dirty_since = None;
        self.shadow.screen_mut().set_size(rows, cols);
        self.display.screen_mut().set_size(rows, cols);
        self.sync_cursor(now, io);
    }

    /// Before exit: forward anything held and remove our overlay.
    pub fn finish(&mut self, now: Instant, io: &mut Io) {
        self.predictor.clear();
        self.flush_held(now, io);
        let mut out = Vec::new();
        self.reconcile(now, &mut out);
        if self.saved {
            out.extend_from_slice(RESTORE);
            self.saved = false;
        }
        self.emit(out, io);
    }

    fn emit(&self, out: Vec<u8>, io: &mut Io) {
        if out.is_empty() {
            return;
        }
        if self.scanner.sync() {
            io.terminal.extend_from_slice(&out);
        } else {
            io.terminal.extend_from_slice(SYNC_BEGIN);
            io.terminal.extend_from_slice(&out);
            io.terminal.extend_from_slice(SYNC_END);
        }
    }

    // ---- drawing -------------------------------------------------------------------

    fn request_cursor_check(&mut self, now: Instant, out: &mut Vec<u8>) {
        // Nothing is ever written into a program that is only being measured.
        if !self.boosting {
            return;
        }
        let Some((line, anchor)) = self.predictor.verification_request() else {
            return;
        };
        if !self.can_inject() || self.saved || self.shadow.screen().cursor_position() != anchor {
            return;
        }
        out.extend_from_slice(CURSOR_QUERY);
        self.queries.push_back(Query::Ours {
            line,
            expected: anchor,
            outputs: self.outputs,
            sent: now,
        });
        self.predictor.verification_sent(line);
    }

    /// Make the real terminal show the authoritative screen plus the overlay.
    fn reconcile(&mut self, now: Instant, out: &mut Vec<u8>) {
        if self.offloaded {
            return;
        }
        let overlay = if self.boosting && self.prediction_allowed() {
            self.predictor.overlay(self.surface())
        } else {
            None
        };
        if !self.dirty && overlay.is_none() {
            return;
        }
        if !self.can_inject() {
            return;
        }
        if !self.dirty {
            *self.display.screen_mut() = self.shadow.screen().clone();
            self.dirty = true;
            self.dirty_since = Some(now);
        }
        let mut sequence = Vec::new();
        // Painting a narrow cell over half of a wide one changes its neighbour too, so
        // repeat until the mirror agrees. Two passes are enough in practice.
        for _ in 0..3 {
            let body = paint(
                self.display.screen(),
                self.shadow.screen(),
                overlay.as_ref(),
            );
            let caret_ok = match &overlay {
                Some(overlay) => self.saved && self.caret == Some(overlay.caret),
                None => !self.saved,
            };
            if body.is_empty() && caret_ok {
                break;
            }
            let mut step = Vec::new();
            if !self.saved && (!body.is_empty() || overlay.is_some()) {
                step.extend_from_slice(SAVE);
                self.saved = true;
            }
            if !body.is_empty() && !self.scanner.charset_ascii() {
                step.extend_from_slice(b"\x1b(B\x0f");
            }
            step.extend_from_slice(&body);
            match &overlay {
                Some(overlay) => {
                    cup(&mut step, overlay.caret.0, overlay.caret.1);
                    self.caret = Some(overlay.caret);
                }
                None => {
                    if self.saved {
                        step.extend_from_slice(RESTORE);
                    }
                    self.saved = false;
                    self.caret = None;
                }
            }
            self.display.process(&step);
            sequence.extend_from_slice(&step);
        }
        out.extend_from_slice(&sequence);
        if overlay.is_none() && !self.saved {
            self.dirty = false;
            self.dirty_since = None;
        }
    }

    /// The application kept its saved cursor across our leftovers. Repair with absolute
    /// positioning and restore the pen from the model instead of DECRC.
    fn repair_without_save(&mut self, out: &mut Vec<u8>) {
        let body = paint(self.display.screen(), self.shadow.screen(), None);
        let mut step = Vec::new();
        if !body.is_empty() {
            if !self.scanner.charset_ascii() {
                step.extend_from_slice(b"\x1b(B\x0f");
            }
            step.extend_from_slice(&body);
            if !self.scanner.charset_ascii() {
                step.extend_from_slice(&self.scanner.charset_restore());
            }
        }
        let (row, col) = self.shadow.screen().cursor_position();
        cup(&mut step, row, col.min(self.shadow.screen().size().1 - 1));
        step.extend_from_slice(&self.shadow.screen().attributes_formatted());
        self.display.process(&step);
        out.extend_from_slice(&step);
        crate::log!("repaired overlay without saved cursor");
        self.saved = false;
        self.caret = None;
        self.dirty = false;
        self.dirty_since = None;
    }
}

#[derive(PartialEq)]
struct Look<'a> {
    symbol: &'a str,
    wide: bool,
    continuation: bool,
    style: Style,
}

fn look(cell: Option<&vt100::Cell>) -> Look<'_> {
    match cell {
        Some(cell) => Look {
            symbol: normalized(cell.contents()),
            wide: cell.is_wide(),
            continuation: cell.is_wide_continuation(),
            style: Style::of(cell),
        },
        None => Look {
            symbol: " ",
            wide: false,
            continuation: false,
            style: Style::default(),
        },
    }
}

fn look_data(cell: &CellData) -> Look<'_> {
    Look {
        symbol: &cell.symbol,
        wide: cell.wide,
        continuation: cell.continuation,
        style: cell.style,
    }
}

fn desired<'a>(
    shadow: &'a vt100::Screen,
    overlay: Option<&'a Overlay>,
    row: u16,
    col: u16,
) -> Look<'a> {
    if let Some(overlay) = overlay
        && row == overlay.row
        && col >= overlay.col
        && let Some(cell) = overlay.cells.get(usize::from(col - overlay.col))
    {
        return look_data(cell);
    }
    look(shadow.cell(row, col))
}

/// Bytes that turn what the terminal shows (`display`) into `shadow` plus `overlay`.
fn paint(display: &vt100::Screen, shadow: &vt100::Screen, overlay: Option<&Overlay>) -> Vec<u8> {
    let (rows, cols) = shadow.size();
    let desired = |row, col| desired(shadow, overlay, row, col);
    let mut body = Vec::new();
    let mut pen: Option<(u16, u16)> = None;
    let mut style: Option<Style> = None;
    for row in 0..rows {
        let mut col = 0;
        while col < cols {
            let want = desired(row, col);
            if want == look(display.cell(row, col)) {
                col += 1;
                continue;
            }
            let (start, want) = if want.continuation && col > 0 {
                (col - 1, desired(row, col - 1))
            } else {
                (col, want)
            };
            if want.continuation {
                col += 1;
                continue;
            }
            if pen != Some((row, start)) {
                cup(&mut body, row, start);
            }
            if style != Some(want.style) {
                sgr(&mut body, want.style);
                style = Some(want.style);
            }
            body.extend_from_slice(want.symbol.as_bytes());
            let next = start + if want.wide { 2 } else { 1 };
            // Writing the last column leaves a pending wrap; always reposition after it.
            pen = (next < cols).then_some((row, next));
            col = next.max(col + 1);
        }
    }
    body
}

fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}

fn sgr(out: &mut Vec<u8>, style: Style) {
    let mut codes = String::from("\x1b[0");
    for (on, code) in [
        (style.bold, ";1"),
        (style.dim, ";2"),
        (style.italic, ";3"),
        (style.underline, ";4"),
        (style.inverse, ";7"),
    ] {
        if on {
            codes.push_str(code);
        }
    }
    color(&mut codes, style.fg, 30, 90, 38);
    color(&mut codes, style.bg, 40, 100, 48);
    codes.push('m');
    out.extend_from_slice(codes.as_bytes());
}

fn color(codes: &mut String, color: vt100::Color, base: u8, bright: u8, extended: u8) {
    use std::fmt::Write as _;
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(index @ 0..=7) => {
            let _ = write!(codes, ";{}", base + index);
        }
        vt100::Color::Idx(index @ 8..=15) => {
            let _ = write!(codes, ";{}", bright + index - 8);
        }
        vt100::Color::Idx(index) => {
            let _ = write!(codes, ";{extended};5;{index}");
        }
        vt100::Color::Rgb(red, green, blue) => {
            let _ = write!(codes, ";{extended};2;{red};{green};{blue}");
        }
    }
}

#[cfg(test)]
mod tests;
