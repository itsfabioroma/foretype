//! Tracks just enough of the remote output stream to know when bytes of our own can be
//! interleaved with it. The terminal model lives in `vt100`; this only answers "is the
//! stream between sequences", "is the application mid-frame", and "does the application
//! currently rely on its saved cursor".
//!
//! It also produces the byte stream for the model: identical to the remote output except
//! for sequences real terminals honour and `vt100` ignores (SCOSC/SCORC and REP), which
//! are rewritten into equivalents so the model does not drift from the screen. The local
//! terminal always receives the original bytes.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    String,
    StringEscape,
}

const MAX_PARAMS: usize = 32;
const MAX_REPEAT: usize = 4096;

pub struct Scanner {
    state: State,
    utf8_remaining: u8,
    string_is_osc: bool,
    esc_intermediate: u8,
    csi_private: u8,
    csi_intermediate: u8,
    params: Vec<u32>,
    param: Option<u32>,
    in_subparam: bool,
    sync: bool,
    alt: bool,
    app_saved: [bool; 2],
    g0: u8,
    shifted_out: bool,
    cpr_queries: usize,
    reset: bool,
    model: Vec<u8>,
    model_safe: usize,
    sequence: Vec<u8>,
    /// The current sequence was already handed to the model unfinished.
    sequence_direct: bool,
    rewrite: Option<Vec<u8>>,
    last_graphic: Vec<u8>,
    character: Vec<u8>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self {
            state: State::Ground,
            utf8_remaining: 0,
            string_is_osc: false,
            esc_intermediate: 0,
            csi_private: 0,
            csi_intermediate: 0,
            params: Vec::new(),
            param: None,
            in_subparam: false,
            sync: false,
            alt: false,
            app_saved: [false; 2],
            g0: b'B',
            shifted_out: false,
            cpr_queries: 0,
            reset: false,
            model: Vec::new(),
            model_safe: 0,
            sequence: Vec::new(),
            sequence_direct: false,
            rewrite: None,
            last_graphic: Vec::new(),
            character: Vec::new(),
        }
    }
}

impl Scanner {
    /// True between escape sequences and between UTF-8 characters.
    pub fn at_ground(&self) -> bool {
        self.state == State::Ground && self.utf8_remaining == 0
    }

    /// The application has opened a synchronized update (DECSET 2026) and not closed it.
    pub fn sync(&self) -> bool {
        self.sync
    }

    /// The application saved the cursor (DECSC and friends) on the current screen and has
    /// not restored it yet. Injecting our own DECSC would clobber that slot.
    pub fn app_saved_cursor(&self) -> bool {
        self.app_saved[usize::from(self.alt)]
    }

    pub fn charset_ascii(&self) -> bool {
        self.g0 == b'B' && !self.shifted_out
    }

    /// Bytes that restore the application's character set after we forced ASCII.
    pub fn charset_restore(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.g0 != b'B' {
            out.extend_from_slice(&[0x1b, b'(', self.g0]);
        }
        if self.shifted_out {
            out.push(0x0e);
        }
        out
    }

    /// Cursor position queries (`CSI 6 n`) seen since the last call.
    pub fn take_cpr_queries(&mut self) -> usize {
        std::mem::take(&mut self.cpr_queries)
    }

    /// Whether a full reset (RIS) was seen since the last call.
    pub fn take_reset(&mut self) -> bool {
        std::mem::take(&mut self.reset)
    }

    /// Scans `bytes` and returns the length of the longest prefix after which the stream
    /// is at ground state. Zero means no safe split point was found.
    pub fn feed(&mut self, bytes: &[u8]) -> usize {
        let mut safe = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            self.byte(byte);
            if self.at_ground() {
                safe = index + 1;
                self.model_safe = self.model.len();
            }
        }
        safe
    }

    /// Model bytes for everything up to the last ground point.
    pub fn take_model(&mut self) -> Vec<u8> {
        let rest = self.model.split_off(self.model_safe);
        self.model_safe = 0;
        std::mem::replace(&mut self.model, rest)
    }

    /// Model bytes for everything scanned, including an unfinished sequence, which is
    /// then passed through to the model as it arrives.
    pub fn take_model_all(&mut self) -> Vec<u8> {
        if !self.sequence.is_empty() {
            let sequence = std::mem::take(&mut self.sequence);
            self.model.extend_from_slice(&sequence);
            self.sequence_direct = true;
        }
        self.model_safe = 0;
        std::mem::take(&mut self.model)
    }

    fn byte(&mut self, byte: u8) {
        if self.state == State::Ground {
            self.ground(byte);
            return;
        }
        if byte == 0x1b && !matches!(self.state, State::String) {
            // ESC aborts the sequence in progress; whatever came before reaches the model
            // unchanged and a new sequence starts here.
            self.end_sequence();
            self.begin_sequence();
            self.state = State::Escape;
            return;
        }
        if self.sequence_direct {
            self.model.push(byte);
        } else {
            self.sequence.push(byte);
        }
        match self.state {
            State::Ground => {}
            State::Escape => self.escape(byte),
            State::EscapeIntermediate => self.escape_intermediate(byte),
            State::Csi => self.csi(byte),
            State::String => self.string(byte),
            State::StringEscape => {
                if byte == b'\\' {
                    self.state = State::Ground;
                } else {
                    // ESC inside a string aborts it and starts a new sequence.
                    self.state = State::Escape;
                    self.escape(byte);
                }
            }
        }
        if self.state == State::Ground {
            self.end_sequence();
        }
    }

    fn begin_sequence(&mut self) {
        self.sequence.push(0x1b);
    }

    fn end_sequence(&mut self) {
        let sequence = std::mem::take(&mut self.sequence);
        match self.rewrite.take() {
            Some(rewrite) if !self.sequence_direct => self.model.extend_from_slice(&rewrite),
            _ => self.model.extend_from_slice(&sequence),
        }
        self.sequence_direct = false;
    }

    fn ground(&mut self, byte: u8) {
        if self.utf8_remaining > 0 {
            if (0x80..=0xbf).contains(&byte) {
                self.model.push(byte);
                self.character.push(byte);
                self.utf8_remaining -= 1;
                if self.utf8_remaining == 0 {
                    self.last_graphic = std::mem::take(&mut self.character);
                }
                return;
            }
            self.utf8_remaining = 0;
            self.character.clear();
        }
        match byte {
            0x1b => {
                self.begin_sequence();
                self.state = State::Escape;
            }
            0x20..=0x7e => {
                self.model.push(byte);
                self.last_graphic.clear();
                self.last_graphic.push(byte);
            }
            0xc2..=0xf4 => {
                self.model.push(byte);
                self.character.clear();
                self.character.push(byte);
                self.utf8_remaining = match byte {
                    0xc2..=0xdf => 1,
                    0xe0..=0xef => 2,
                    _ => 3,
                };
            }
            _ => {
                self.model.push(byte);
                self.execute(byte);
            }
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0e => self.shifted_out = true,
            0x0f => self.shifted_out = false,
            _ => {}
        }
    }

    /// C0 handling shared by the sequence states. Returns true when the byte was consumed.
    fn control_in_sequence(&mut self, byte: u8) -> bool {
        match byte {
            0x18 | 0x1a => {
                self.state = State::Ground;
                true
            }
            0x00..=0x1f => {
                self.execute(byte);
                true
            }
            0x7f => true,
            _ => false,
        }
    }

    fn escape(&mut self, byte: u8) {
        if self.control_in_sequence(byte) {
            return;
        }
        self.state = State::Ground;
        match byte {
            0x20..=0x2f => {
                self.esc_intermediate = byte;
                self.state = State::EscapeIntermediate;
            }
            b'[' => {
                self.csi_private = 0;
                self.csi_intermediate = 0;
                self.params.clear();
                self.param = None;
                self.in_subparam = false;
                self.state = State::Csi;
            }
            b']' => {
                self.string_is_osc = true;
                self.state = State::String;
            }
            b'P' | b'X' | b'^' | b'_' => {
                self.string_is_osc = false;
                self.state = State::String;
            }
            b'7' => self.app_saved[usize::from(self.alt)] = true,
            b'8' => self.app_saved[usize::from(self.alt)] = false,
            b'c' => self.full_reset(),
            _ => {}
        }
    }

    fn escape_intermediate(&mut self, byte: u8) {
        if self.control_in_sequence(byte) {
            return;
        }
        match byte {
            0x20..=0x2f => self.esc_intermediate = byte,
            _ => {
                if self.esc_intermediate == b'(' {
                    self.g0 = byte;
                }
                self.state = State::Ground;
            }
        }
    }

    fn csi(&mut self, byte: u8) {
        if self.control_in_sequence(byte) {
            return;
        }
        match byte {
            b'0'..=b'9' => {
                if !self.in_subparam {
                    let digit = u32::from(byte - b'0');
                    self.param = Some(
                        self.param
                            .unwrap_or(0)
                            .saturating_mul(10)
                            .saturating_add(digit)
                            .min(65_535),
                    );
                }
            }
            b';' => {
                if self.params.len() < MAX_PARAMS {
                    self.params.push(self.param.unwrap_or(0));
                }
                self.param = None;
                self.in_subparam = false;
            }
            b':' => self.in_subparam = true,
            0x3c..=0x3f => {
                if self.params.is_empty() && self.param.is_none() && self.csi_private == 0 {
                    self.csi_private = byte;
                }
            }
            0x20..=0x2f => self.csi_intermediate = byte,
            0x40..=0x7e => {
                if let Some(param) = self.param.take()
                    && self.params.len() < MAX_PARAMS
                {
                    self.params.push(param);
                }
                self.dispatch_csi(byte);
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn dispatch_csi(&mut self, final_byte: u8) {
        let screen = usize::from(self.alt);
        // Rewrites only apply to the plain form, without embedded controls.
        let plain = self.sequence.len() >= 3
            && self.sequence[2..self.sequence.len() - 1]
                .iter()
                .all(u8::is_ascii_digit);
        match (self.csi_private, self.csi_intermediate, final_byte) {
            (b'?', 0, b'h' | b'l') => {
                let set = final_byte == b'h';
                for index in 0..self.params.len() {
                    match self.params[index] {
                        2026 => self.sync = set,
                        47 | 1047 | 1049 => self.alt = set,
                        1048 => self.app_saved[usize::from(self.alt)] = set,
                        _ => {}
                    }
                }
            }
            (0, 0, b's') if self.params.is_empty() => {
                self.app_saved[screen] = true;
                if plain {
                    self.rewrite = Some(b"\x1b7".to_vec());
                }
            }
            (0, 0, b'u') if self.params.is_empty() => {
                self.app_saved[screen] = false;
                if plain {
                    self.rewrite = Some(b"\x1b8".to_vec());
                }
            }
            (0, 0, b'b') if plain && !self.last_graphic.is_empty() => {
                let count = self.params.first().copied().unwrap_or(1).max(1) as usize;
                self.rewrite = Some(self.last_graphic.repeat(count.min(MAX_REPEAT)));
            }
            (0, 0, b'n') if self.params.as_slice() == [6] => self.cpr_queries += 1,
            // DECSTR soft reset homes the saved cursor and resets character sets.
            (0, b'!', b'p') => {
                self.app_saved = [false; 2];
                self.g0 = b'B';
                self.shifted_out = false;
                self.sync = false;
            }
            _ => {}
        }
    }

    fn string(&mut self, byte: u8) {
        match byte {
            0x07 if self.string_is_osc => self.state = State::Ground,
            0x1b => self.state = State::StringEscape,
            0x18 | 0x1a => self.state = State::Ground,
            _ => {}
        }
    }

    fn full_reset(&mut self) {
        let cpr_queries = self.cpr_queries;
        let model = std::mem::take(&mut self.model);
        let model_safe = self.model_safe;
        let sequence = std::mem::take(&mut self.sequence);
        let sequence_direct = self.sequence_direct;
        *self = Self::default();
        self.cpr_queries = cpr_queries;
        self.model = model;
        self.model_safe = model_safe;
        self.sequence = sequence;
        self.sequence_direct = sequence_direct;
        self.reset = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_only_between_sequences_and_characters() {
        let mut scanner = Scanner::default();
        assert_eq!(scanner.feed(b"ab\x1b[3"), 2);
        assert!(!scanner.at_ground());
        assert_eq!(scanner.feed(b"1mc"), 3);
        assert_eq!(scanner.feed("é".as_bytes()), 2);
        assert_eq!(scanner.feed(&"é".as_bytes()[..1]), 0);
        assert_eq!(scanner.feed(&"é".as_bytes()[1..]), 1);
        assert_eq!(scanner.feed(b"\x1b]0;title"), 0);
        assert_eq!(scanner.feed(b"\x07x"), 2);
        assert_eq!(scanner.feed(b"\x1bP1$r0m\x1b"), 0);
        assert_eq!(scanner.feed(b"\\"), 1);
    }

    #[test]
    fn model_bytes_match_output_except_for_rewrites() {
        let mut scanner = Scanner::default();
        scanner.feed(b"ab\x1b[31m\x1b[s\x1b[3;1Hx\x1b[u\x1b[3bz\x1b[?u\x1b[");
        assert_eq!(
            scanner.take_model(),
            b"ab\x1b[31m\x1b7\x1b[3;1Hx\x1b8xxxz\x1b[?u".to_vec()
        );
        scanner.feed(b"5");
        assert!(scanner.take_model().is_empty());
        scanner.feed(b"mq");
        assert_eq!(scanner.take_model(), b"\x1b[5mq".to_vec());
        scanner.feed("é\x1b[2b".as_bytes());
        assert_eq!(scanner.take_model(), "ééé".as_bytes().to_vec());
        // An aborted sequence still reaches the model in order.
        scanner.feed(b"\x1b[1\x1b[2mx");
        assert_eq!(scanner.take_model(), b"\x1b[1\x1b[2mx".to_vec());
    }

    #[test]
    fn unfinished_sequences_can_be_flushed_to_the_model() {
        let mut scanner = Scanner::default();
        scanner.feed(b"a\x1b]52;c;QUJD");
        assert_eq!(scanner.take_model_all(), b"a\x1b]52;c;QUJD".to_vec());
        scanner.feed(b"REVG\x07b");
        assert_eq!(scanner.take_model_all(), b"REVG\x07b".to_vec());
        assert!(scanner.at_ground());
    }

    #[test]
    fn tracks_synchronized_updates_and_alternate_screens() {
        let mut scanner = Scanner::default();
        scanner.feed(b"\x1b[?2026h");
        assert!(scanner.sync());
        scanner.feed(b"frame\x1b[?2026l");
        assert!(!scanner.sync());
        scanner.feed(b"\x1b[?1049h\x1b7");
        assert!(scanner.app_saved_cursor());
        scanner.feed(b"\x1b[?1049l");
        assert!(!scanner.app_saved_cursor());
        scanner.feed(b"\x1b[?1049h\x1b8");
        assert!(!scanner.app_saved_cursor());
    }

    #[test]
    fn tracks_application_cursor_saves() {
        let mut scanner = Scanner::default();
        scanner.feed(b"\x1b[s");
        assert!(scanner.app_saved_cursor());
        scanner.feed(b"\x1b[u");
        assert!(!scanner.app_saved_cursor());
        // Kitty keyboard queries share the final byte but carry a private marker.
        scanner.feed(b"\x1b[?u\x1b[>1u\x1b[<u");
        assert!(!scanner.app_saved_cursor());
    }

    #[test]
    fn tracks_charsets_queries_and_resets() {
        let mut scanner = Scanner::default();
        scanner.feed(b"\x1b(0");
        assert!(!scanner.charset_ascii());
        assert_eq!(scanner.charset_restore(), b"\x1b(0");
        scanner.feed(b"\x1b(B\x0e");
        assert!(!scanner.charset_ascii());
        scanner.feed(b"\x0f\x1b[6n\x1b[?6n\x1b[6n");
        assert!(scanner.charset_ascii());
        assert_eq!(scanner.take_cpr_queries(), 2);
        scanner.feed(b"\x1b[?2026h\x1bc");
        assert!(!scanner.sync());
        assert!(scanner.take_reset());
        assert!(!scanner.take_reset());
    }
}
