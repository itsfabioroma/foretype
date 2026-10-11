//! Splits local keyboard input into tokens. Bytes are never rewritten: each token keeps
//! its exact bytes, and only the classification decides what the predictor may do.

/// What a token means for prediction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// Printable ASCII the remote side should echo verbatim.
    Text(String),
    /// Input that has no echo and does not change the prompt: key releases, focus-in,
    /// terminal reports, plain mouse motion.
    Neutral,
    /// A cursor position report, which may answer one of our own queries.
    CursorReport { row: u16, col: u16 },
    /// Anything else: editing keys, controls, paste, clicks, non-ASCII text.
    Control,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub bytes: Vec<u8>,
    pub key: Key,
}

const MAX_STRING: usize = 64 * 1024;

#[derive(Default)]
pub struct InputParser {
    carry: Vec<u8>,
    paste: bool,
}

enum Parsed {
    Complete(usize, Key),
    Incomplete,
}

impl InputParser {
    /// Whether an incomplete escape sequence is waiting for more bytes.
    pub fn has_carry(&self) -> bool {
        !self.carry.is_empty()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Token> {
        self.carry.extend_from_slice(bytes);
        self.drain(false)
    }

    /// Treats a dangling escape prefix as complete (a lone Escape key press).
    pub fn flush(&mut self) -> Vec<Token> {
        self.drain(true)
    }

    fn drain(&mut self, flush: bool) -> Vec<Token> {
        let buffer = std::mem::take(&mut self.carry);
        let mut tokens = Vec::new();
        let mut index = 0;
        while index < buffer.len() {
            match parse(&buffer[index..], flush) {
                Parsed::Complete(len, key) => {
                    let bytes = buffer[index..index + len].to_vec();
                    tokens.push(self.token(bytes, key));
                    index += len;
                }
                Parsed::Incomplete => {
                    self.carry = buffer[index..].to_vec();
                    break;
                }
            }
        }
        tokens
    }

    fn token(&mut self, bytes: Vec<u8>, key: Key) -> Token {
        let key = match (&key, bytes.as_slice()) {
            (_, b"\x1b[200~") => {
                self.paste = true;
                Key::Control
            }
            (_, b"\x1b[201~") => {
                self.paste = false;
                Key::Control
            }
            _ if self.paste => Key::Control,
            _ => key,
        };
        Token { bytes, key }
    }
}

fn parse(buffer: &[u8], flush: bool) -> Parsed {
    match buffer[0] {
        0x1b => parse_escape(buffer, flush),
        0x20..=0x7e => {
            let len = buffer
                .iter()
                .take_while(|byte| (0x20..=0x7e).contains(*byte))
                .count();
            let text = String::from_utf8_lossy(&buffer[..len]).into_owned();
            Parsed::Complete(len, Key::Text(text))
        }
        0x80..=0xff => {
            let len = buffer.iter().take_while(|byte| **byte >= 0x80).count();
            Parsed::Complete(len, Key::Control)
        }
        _ => Parsed::Complete(1, Key::Control),
    }
}

fn parse_escape(buffer: &[u8], flush: bool) -> Parsed {
    let Some(&next) = buffer.get(1) else {
        return if flush {
            Parsed::Complete(1, Key::Control)
        } else {
            Parsed::Incomplete
        };
    };
    match next {
        b'[' => parse_csi(buffer, flush),
        b']' | b'P' | b'_' | b'^' | b'X' => parse_string(buffer, flush),
        b'O' => match buffer.get(2) {
            Some(_) => Parsed::Complete(3, Key::Control),
            None if flush => Parsed::Complete(2, Key::Control),
            None => Parsed::Incomplete,
        },
        // Alt+key, or Escape followed by another key in the same read.
        _ => Parsed::Complete(2, Key::Control),
    }
}

fn parse_string(buffer: &[u8], flush: bool) -> Parsed {
    let mut index = 2;
    while index < buffer.len() {
        match buffer[index] {
            0x07 => return Parsed::Complete(index + 1, Key::Neutral),
            0x1b if buffer.get(index + 1) == Some(&b'\\') => {
                return Parsed::Complete(index + 2, Key::Neutral);
            }
            _ => index += 1,
        }
        if index > MAX_STRING {
            return Parsed::Complete(index, Key::Control);
        }
    }
    if flush {
        Parsed::Complete(buffer.len(), Key::Control)
    } else {
        Parsed::Incomplete
    }
}

fn parse_csi(buffer: &[u8], flush: bool) -> Parsed {
    let mut index = 2;
    while index < buffer.len() {
        let byte = buffer[index];
        match byte {
            0x20..=0x3f => index += 1,
            0x40..=0x7e => {
                // X10 mouse reports carry three raw bytes after `CSI M`.
                if byte == b'M' && index == 2 {
                    return if buffer.len() >= 6 {
                        Parsed::Complete(6, Key::Control)
                    } else if flush {
                        Parsed::Complete(buffer.len(), Key::Control)
                    } else {
                        Parsed::Incomplete
                    };
                }
                let key = classify_csi(&buffer[2..index], byte);
                return Parsed::Complete(index + 1, key);
            }
            // Malformed: keep the prefix as an opaque control token.
            _ => return Parsed::Complete(index, Key::Control),
        }
    }
    if flush {
        Parsed::Complete(buffer.len(), Key::Control)
    } else {
        Parsed::Incomplete
    }
}

fn params(body: &[u8]) -> Vec<Vec<u32>> {
    body.split(|byte| *byte == b';')
        .map(|param| {
            param
                .split(|byte| *byte == b':')
                .map(|value| {
                    std::str::from_utf8(value)
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0)
                })
                .collect()
        })
        .collect()
}

fn classify_csi(body: &[u8], final_byte: u8) -> Key {
    let private = body
        .first()
        .copied()
        .filter(|byte| (0x3c..=0x3f).contains(byte));
    let intermediate = body.iter().any(|byte| (0x20..=0x2f).contains(byte));
    let digits = if private.is_some() { &body[1..] } else { body };
    match (private, intermediate, final_byte) {
        // Focus in has no echo; focus out means the user is no longer typing here.
        (None, false, b'I') if digits.is_empty() => Key::Neutral,
        (None, false, b'R') => {
            let values = params(digits);
            match values.as_slice() {
                [row, col] if row.len() == 1 && col.len() == 1 => Key::CursorReport {
                    row: clamp(row[0]),
                    col: clamp(col[0]),
                },
                _ => Key::Control,
            }
        }
        (None, false, b'u') => kitty_key(&params(digits)),
        (Some(b'<'), false, b'M' | b'm') => {
            let values = params(digits);
            let button = values.first().and_then(|value| value.first()).copied();
            // Motion with no button held never changes what the application draws
            // for typed text; clicks, drags and wheel events can.
            match button {
                Some(button) if button & 32 != 0 && button & 3 == 3 && button < 64 => Key::Neutral,
                _ => Key::Control,
            }
        }
        // Terminal reports: DA1/DA2, kitty flags, DECRPM, window and DSR reports.
        (Some(b'?' | b'>'), _, b'c' | b'u' | b'R' | b'n')
        | (_, true, b'y')
        | (None, false, b't' | b'n') => Key::Neutral,
        _ => Key::Control,
    }
}

fn clamp(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Kitty keyboard protocol: `CSI code[:shifted[:base]] ; mods[:event] ; text u`.
fn kitty_key(values: &[Vec<u32>]) -> Key {
    let code = values.first().and_then(|value| value.first()).copied();
    let shifted = values.first().and_then(|value| value.get(1)).copied();
    let modifiers = values
        .get(1)
        .and_then(|value| value.first())
        .copied()
        .unwrap_or(1)
        .saturating_sub(1);
    let event = values
        .get(1)
        .and_then(|value| value.get(1))
        .copied()
        .unwrap_or(1);
    if event == 3 {
        return Key::Neutral;
    }
    // Caps lock and num lock do not change what an already-composed key produces.
    let modifiers = modifiers & !(64 | 128);
    if modifiers & !1 != 0 {
        return Key::Control;
    }
    let text: Option<String> = match values.get(2) {
        Some(codepoints) if !codepoints.is_empty() && codepoints.iter().any(|cp| *cp != 0) => {
            codepoints.iter().map(|cp| char::from_u32(*cp)).collect()
        }
        _ => {
            let code = if modifiers == 1 {
                shifted.or_else(|| {
                    code.filter(|code| (u32::from(b'a')..=u32::from(b'z')).contains(code))
                        .map(|code| code - 32)
                })
            } else {
                code
            };
            code.and_then(char::from_u32).map(String::from)
        }
    };
    match text {
        Some(text)
            if !text.is_empty() && text.bytes().all(|byte| (b' '..=b'~').contains(&byte)) =>
        {
            Key::Text(text)
        }
        _ => Key::Control,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(parser: &mut InputParser, bytes: &[u8]) -> Vec<Key> {
        parser
            .feed(bytes)
            .into_iter()
            .map(|token| token.key)
            .collect()
    }

    #[test]
    fn printable_runs_and_controls() {
        let mut parser = InputParser::default();
        assert_eq!(
            keys(&mut parser, b"ab\rc\x7f"),
            vec![
                Key::Text("ab".into()),
                Key::Control,
                Key::Text("c".into()),
                Key::Control
            ]
        );
        assert_eq!(keys(&mut parser, "é".as_bytes()), vec![Key::Control]);
    }

    #[test]
    fn escape_sequences_wait_for_completion_and_flush_as_escape() {
        let mut parser = InputParser::default();
        assert!(keys(&mut parser, b"\x1b").is_empty());
        assert!(parser.has_carry());
        assert_eq!(keys(&mut parser, b"[A"), vec![Key::Control]);
        assert!(keys(&mut parser, b"\x1b").is_empty());
        let flushed: Vec<Key> = parser.flush().into_iter().map(|t| t.key).collect();
        assert_eq!(flushed, vec![Key::Control]);
        assert_eq!(keys(&mut parser, b"\x1bx"), vec![Key::Control]);
    }

    #[test]
    fn reports_are_neutral_and_cursor_reports_are_parsed() {
        let mut parser = InputParser::default();
        assert_eq!(
            keys(
                &mut parser,
                b"\x1b[12;40R\x1b[?62;22c\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[I"
            ),
            vec![
                Key::CursorReport { row: 12, col: 40 },
                Key::Neutral,
                Key::Neutral,
                Key::Neutral
            ]
        );
        assert_eq!(keys(&mut parser, b"\x1b[O"), vec![Key::Control]);
    }

    #[test]
    fn kitty_keys() {
        let mut parser = InputParser::default();
        assert_eq!(
            keys(
                &mut parser,
                b"\x1b[97u\x1b[97;2u\x1b[97;1:3u\x1b[97;5u\x1b[13u\x1b[49;2;33u"
            ),
            vec![
                Key::Text("a".into()),
                Key::Text("A".into()),
                Key::Neutral,
                Key::Control,
                Key::Control,
                Key::Text("!".into())
            ]
        );
    }

    #[test]
    fn bracketed_paste_is_never_text() {
        let mut parser = InputParser::default();
        assert_eq!(
            keys(&mut parser, b"\x1b[200~secret\x1b[201~x"),
            vec![
                Key::Control,
                Key::Control,
                Key::Control,
                Key::Text("x".into())
            ]
        );
    }

    #[test]
    fn mouse_motion_is_neutral_but_clicks_are_not() {
        let mut parser = InputParser::default();
        assert_eq!(
            keys(
                &mut parser,
                b"\x1b[<35;10;5M\x1b[<0;10;5M\x1b[<64;1;1M\x1b[M !!"
            ),
            vec![Key::Neutral, Key::Control, Key::Control, Key::Control]
        );
    }
}
