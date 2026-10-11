//! End-to-end session behaviour against an emulated local terminal.

use super::*;

const ROWS: u16 = 6;
const COLS: u16 = 40;

/// The user's terminal: renders what foretype writes and answers cursor queries.
struct Terminal {
    parser: vt100::Parser,
}

impl Terminal {
    fn new() -> Self {
        Self {
            parser: vt100::Parser::new(ROWS, COLS, 0),
        }
    }

    /// Renders bytes, returning the replies the terminal would type back.
    fn render(&mut self, bytes: &[u8]) -> Vec<u8> {
        // vt100 ignores SCOSC/SCORC; real terminals do not.
        let bytes = replace(&replace(bytes, b"\x1b[s", b"\x1b7"), b"\x1b[u", b"\x1b8");
        let mut replies = Vec::new();
        let mut rest = bytes.as_slice();
        while let Some(index) = find(rest, CURSOR_QUERY) {
            self.parser.process(&rest[..index]);
            let (row, col) = self.parser.screen().cursor_position();
            replies.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            rest = &rest[index + CURSOR_QUERY.len()..];
        }
        self.parser.process(rest);
        replies
    }

    fn row(&self, row: u16) -> String {
        (0..COLS)
            .map(|col| {
                normalized(
                    self.parser
                        .screen()
                        .cell(row, col)
                        .map_or("", |cell| cell.contents()),
                )
                .to_owned()
            })
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn underlined(&self, row: u16, col: u16) -> bool {
        self.parser
            .screen()
            .cell(row, col)
            .is_some_and(vt100::Cell::underline)
    }

    fn cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }
}

fn replace(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = haystack;
    while let Some(index) = find(rest, from) {
        out.extend_from_slice(&rest[..index]);
        out.extend_from_slice(to);
        rest = &rest[index + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

struct Rig {
    session: Session,
    terminal: Terminal,
    /// Everything foretype sent to the remote side.
    sent: Vec<u8>,
    now: Instant,
}

impl Rig {
    fn new() -> Self {
        let mut session = Session::new(ROWS, COLS, true, crate::predict::DEFAULT_LIFETIME);
        session.boost();
        Self::with(session)
    }

    /// A session that has to measure echo latency before it draws anything.
    fn measuring() -> Self {
        Self::with(Session::new(
            ROWS,
            COLS,
            true,
            crate::predict::DEFAULT_LIFETIME,
        ))
    }

    fn with(mut session: Session) -> Self {
        // Underlined predictions make tentative cells easy to tell apart in tests.
        session.predictor.underline = true;
        Self {
            session,
            terminal: Terminal::new(),
            sent: Vec::new(),
            now: Instant::now(),
        }
    }

    fn deliver(&mut self, io: Io) {
        self.sent.extend_from_slice(&io.child);
        let replies = self.terminal.render(&io.terminal);
        if !replies.is_empty() {
            let mut io = Io::default();
            self.session.input(&replies, self.now, &mut io);
            self.deliver(io);
        }
    }

    fn remote(&mut self, bytes: &[u8]) {
        let mut io = Io::default();
        self.session.output(bytes, self.now, &mut io);
        self.deliver(io);
    }

    fn type_keys(&mut self, bytes: &[u8]) {
        let mut io = Io::default();
        self.session.input(bytes, self.now, &mut io);
        self.deliver(io);
    }

    fn advance(&mut self, duration: Duration) {
        self.now += duration;
        let mut io = Io::default();
        self.session.tick(self.now, &mut io);
        self.deliver(io);
    }

    /// The local terminal shows exactly the authoritative screen.
    fn assert_authoritative(&mut self) {
        self.session.take_model_back();
        let shadow = self.session.shadow.screen();
        let terminal = self.terminal.parser.screen();
        for row in 0..ROWS {
            for col in 0..COLS {
                assert!(
                    look(shadow.cell(row, col)) == look(terminal.cell(row, col)),
                    "cell {row},{col}: terminal {:?} vs remote {:?}\nterminal row: {:?}",
                    terminal.cell(row, col).map(vt100::Cell::contents),
                    shadow.cell(row, col).map(vt100::Cell::contents),
                    self.terminal.row(row),
                );
            }
        }
        assert_eq!(terminal.cursor_position(), shadow.cursor_position());
        assert_eq!(terminal.hide_cursor(), shadow.hide_cursor());
        assert_eq!(Style::of_screen(terminal), Style::of_screen(shadow));
    }

    /// A shell: prompt, then type `first` and wait for its echo to train the line.
    fn trained_shell(&mut self) {
        self.remote(b"$ ");
        self.type_keys(b"a");
        assert_eq!(self.terminal.row(0), "$");
        self.advance(Duration::from_millis(100));
        self.remote(b"a");
        self.assert_authoritative();
    }
}

impl Style {
    fn of_screen(screen: &vt100::Screen) -> Self {
        Self {
            fg: screen.fgcolor(),
            bg: screen.bgcolor(),
            bold: screen.bold(),
            dim: screen.dim(),
            italic: screen.italic(),
            underline: screen.underline(),
            inverse: screen.inverse(),
        }
    }
}

#[test]
fn shell_typing_appears_immediately_and_settles_to_the_remote_screen() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"bc");
    assert_eq!(rig.sent, b"abc");
    assert_eq!(rig.terminal.row(0), "$ abc");
    assert!(rig.terminal.underlined(0, 3));
    assert!(rig.terminal.underlined(0, 4));
    assert!(!rig.terminal.underlined(0, 2));
    assert_eq!(rig.terminal.cursor(), (0, 5));

    rig.advance(Duration::from_millis(80));
    rig.remote(b"b");
    assert_eq!(rig.terminal.row(0), "$ abc");
    assert!(!rig.terminal.underlined(0, 3));
    assert!(rig.terminal.underlined(0, 4));
    rig.remote(b"c");
    rig.assert_authoritative();
    assert_eq!(rig.session.predictor.stats.confirmed, 2);
}

#[test]
fn remote_output_lands_where_the_remote_expects_while_predictions_show() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"bcd");
    // Relative output from the remote: it believes the cursor is right after "a".
    rig.remote(b"b\x08b");
    assert_eq!(rig.terminal.row(0), "$ abcd");
    assert!(rig.terminal.underlined(0, 4));
    rig.remote(b"\x1b[s\x1b[3;1Hstatus\x1b[u");
    assert_eq!(rig.terminal.row(2), "status");
    // A different echo than what was typed hides the rest immediately.
    rig.remote(b"\x08B");
    assert_eq!(rig.terminal.row(0), "$ aB");
    rig.advance(Duration::from_secs(1));
    rig.assert_authoritative();
    assert_eq!(rig.terminal.row(0), "$ aB");
}

#[test]
fn wrong_echo_is_withdrawn() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"xyz");
    assert_eq!(rig.terminal.row(0), "$ axyz");
    rig.remote(b"X");
    rig.advance(SETTLE_FOR_TEST);
    rig.assert_authoritative();
    assert_eq!(rig.terminal.row(0), "$ aX");
    assert_eq!(rig.session.predictor.stats.discarded, 3);
}

const SETTLE_FOR_TEST: Duration = Duration::from_millis(40);

#[test]
fn no_echo_means_nothing_is_ever_drawn() {
    let mut rig = Rig::new();
    rig.remote(b"Password: ");
    for key in b"hunter2" {
        rig.type_keys(&[*key]);
        rig.assert_authoritative();
        rig.advance(Duration::from_millis(50));
    }
    rig.advance(Duration::from_secs(1));
    rig.assert_authoritative();
    assert_eq!(rig.session.predictor.stats.shown, 0);
}

#[test]
fn enter_and_editing_keys_stop_prediction() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"b\r");
    // The unconfirmed "b" is withdrawn as soon as Enter is pressed.
    rig.assert_authoritative();
    rig.remote(b"b\r\n$ ");
    rig.type_keys(b"q");
    rig.assert_authoritative();
    rig.type_keys(b"\x7f");
    rig.assert_authoritative();
}

#[test]
fn timed_out_predictions_disappear() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"bc");
    assert_eq!(rig.terminal.row(0), "$ abc");
    rig.advance(Duration::from_millis(800));
    rig.assert_authoritative();
}

#[test]
fn split_escape_sequences_are_not_interrupted() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"bc");
    rig.remote(b"b\x1b[3");
    // Typing while the remote sequence is incomplete must not inject into it.
    rig.type_keys(b"d");
    rig.remote(b"1mc\x1b[m");
    rig.remote(b"d");
    rig.assert_authoritative();
    let cell = rig.session.shadow.screen().cell(0, 4).unwrap();
    assert_eq!(cell.contents(), "c");
    assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
}

#[test]
fn synchronized_frames_are_left_alone_until_complete() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"bc");
    rig.remote(b"\x1b[?2026h\r\x1b[K$ ab");
    rig.remote(b"\x1b[?2026l");
    assert_eq!(rig.terminal.row(0), "$ abc");
    rig.remote(b"\x1b[?2026h\r\x1b[K$ abc\x1b[?2026l");
    rig.assert_authoritative();
}

#[test]
fn application_cursor_queries_are_answered_to_the_application() {
    let mut rig = Rig::new();
    rig.remote(b"$ ");
    // Our own cursor check goes out with the first keystroke of a line.
    rig.type_keys(b"a");
    rig.remote(b"\x1b[6n");
    let report = b"\x1b[1;3R";
    assert!(
        rig.sent.ends_with(report),
        "{:?}",
        String::from_utf8_lossy(&rig.sent)
    );
    assert_eq!(
        rig.sent
            .windows(report.len())
            .filter(|w| *w == report)
            .count(),
        1
    );
}

/// foretype normally starts below the shell prompt that launched it.
fn started_mid_screen(sync: bool) -> Rig {
    let mut rig = Rig::new();
    rig.terminal.render(b"local% ssh host\r\n");
    if sync {
        let mut io = Io::default();
        rig.session.sync_cursor(rig.now, &mut io);
        rig.deliver(io);
    }
    rig
}

#[test]
fn starting_below_existing_output_syncs_the_cursor_first() {
    let mut rig = started_mid_screen(true);
    rig.remote(b"banner\r\n$ ");
    rig.type_keys(b"a");
    rig.remote(b"a");
    rig.type_keys(b"bc");
    assert_eq!(rig.terminal.row(0), "local% ssh host");
    assert_eq!(rig.terminal.row(2), "$ abc");
    assert!(rig.terminal.underlined(2, 3));
    rig.remote(b"bc");
    assert_eq!(rig.terminal.row(2), "$ abc");
    assert!(!rig.terminal.underlined(2, 3));
    assert_eq!(
        rig.terminal.cursor(),
        rig.session.shadow.screen().cursor_position()
    );
    assert_eq!(rig.session.cursor_checks_failed(), 0);
}

#[test]
fn a_disagreeing_cursor_check_draws_nothing_and_resyncs_the_model() {
    let mut rig = started_mid_screen(false);
    rig.remote(b"$ ");
    rig.type_keys(b"a");
    // The model believed row 0; the terminal reported row 1.
    assert_eq!(rig.session.cursor_checks_failed(), 1);
    rig.remote(b"a");
    rig.type_keys(b"bc");
    assert_eq!(rig.terminal.row(1), "$ a");
    assert!(!rig.terminal.underlined(1, 3));
    rig.remote(b"bc\r\n$ ");
    rig.advance(SETTLE_FOR_TEST);
    // From the next line on, the model agrees with the terminal and prediction works.
    rig.type_keys(b"x");
    rig.remote(b"x");
    rig.type_keys(b"yz");
    assert_eq!(rig.terminal.row(2), "$ xyz");
    assert!(rig.terminal.underlined(2, 4));
    assert_eq!(rig.session.cursor_checks_failed(), 1);
}

#[test]
fn a_silent_terminal_turns_foretype_into_a_pipe() {
    let mut session = Session::new(ROWS, COLS, true, crate::predict::DEFAULT_LIFETIME);
    session.boost();
    let now = Instant::now();
    let mut io = Io::default();
    session.sync_cursor(now, &mut io);
    assert_eq!(io.terminal, CURSOR_QUERY);
    let mut io = Io::default();
    session.tick(now + QUERY_TIMEOUT, &mut io);
    session.output(b"$ ", now + QUERY_TIMEOUT, &mut io);
    session.input(b"a", now + QUERY_TIMEOUT, &mut io);
    session.output(b"a", now + QUERY_TIMEOUT, &mut io);
    session.input(b"bc", now + QUERY_TIMEOUT, &mut io);
    assert_eq!(io.terminal, b"$ a");
    assert_eq!(io.child, b"abc");
}

#[test]
fn software_caret_editors_get_a_moving_caret() {
    let mut rig = Rig::new();
    // Hidden hardware cursor, inverse blank caret drawn by the application.
    let frame = |text: &str| format!("\x1b[?25l\r\x1b[K> {text}\x1b[7m \x1b[m\x1b[D");
    rig.remote(frame("").as_bytes());
    rig.type_keys(b"h");
    rig.advance(Duration::from_millis(100));
    rig.remote(frame("h").as_bytes());
    rig.type_keys(b"ey");
    assert_eq!(rig.terminal.row(0), "> hey");
    assert!(rig.terminal.parser.screen().cell(0, 5).unwrap().inverse());
    assert!(!rig.terminal.parser.screen().cell(0, 3).unwrap().inverse());
    rig.remote(frame("he").as_bytes());
    rig.remote(frame("hey").as_bytes());
    rig.assert_authoritative();
}

#[test]
fn scrolling_output_carries_leftovers_and_they_are_repaired() {
    let mut rig = Rig::new();
    rig.remote(b"\r\n\r\n\r\n\r\n\r\n$ ");
    rig.type_keys(b"a");
    rig.remote(b"a");
    rig.type_keys(b"bcd");
    assert_eq!(rig.terminal.row(5), "$ abcd");
    // The remote prints output that scrolls the predicted cells up.
    rig.remote(b"\r\nline1\r\nline2\r\n$ ");
    rig.assert_authoritative();
}

#[test]
fn application_saved_cursor_is_never_clobbered() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"bc");
    // The application saves its cursor and restores it in a later write.
    rig.remote(b"\x1b7\x1b[4;1Hprogress");
    rig.type_keys(b"d");
    rig.remote(b"\x1b8b");
    rig.advance(SETTLE_FOR_TEST);
    rig.advance(Duration::from_secs(1));
    rig.assert_authoritative();
    assert_eq!(rig.terminal.row(0), "$ ab");
    assert_eq!(rig.terminal.row(3), "progress");
}

#[test]
fn stale_overlay_is_repaired_even_if_the_application_never_restores() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"bc");
    rig.remote(b"\x1b7\x1b[4;1Hx\x1b[1;4H");
    rig.advance(Duration::from_millis(200));
    rig.assert_authoritative();
}

#[test]
fn resize_drops_predictions() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"bc");
    let mut io = Io::default();
    rig.session.resize(ROWS, COLS - 1, rig.now, &mut io);
    rig.terminal.parser.screen_mut().set_size(ROWS, COLS - 1);
    rig.deliver(io);
    rig.remote(b"bc");
    assert_eq!(rig.terminal.row(0), "$ abc");
    assert_eq!(rig.terminal.cursor(), (0, 5));
}

#[test]
fn kitty_keyboard_text_is_predicted_and_releases_are_ignored() {
    let mut rig = Rig::new();
    rig.trained_shell();
    rig.type_keys(b"\x1b[98u\x1b[98;1:3u\x1b[99;2u");
    assert_eq!(rig.terminal.row(0), "$ abC");
}

#[test]
fn disabled_sessions_are_plain_passthrough() {
    let mut session = Session::new(ROWS, COLS, false, crate::predict::DEFAULT_LIFETIME);
    let now = Instant::now();
    let mut io = Io::default();
    session.output(b"$ ", now, &mut io);
    session.input(b"a", now, &mut io);
    session.output(b"a", now, &mut io);
    session.input(b"bc", now, &mut io);
    assert_eq!(io.terminal, b"$ a");
    assert_eq!(io.child, b"abc");
}

/// Random remote behaviour and typing: whatever happens, once the remote has caught up
/// and timers have run, the terminal shows exactly what the remote drew.
#[test]
fn randomized_sessions_always_converge_to_the_remote_screen() {
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut random = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    let noise: [&[u8]; 9] = [
        b"\x1b[1;30Hspin",
        b"\x1b7\x1b[2;1H\x1b[32mok\x1b[m\x1b8",
        b"\r\n",
        b"\x1b[?2026h\x1b[3;1Hframe\x1b[?2026l",
        b"\x1b[K",
        b"\x1b[?25l",
        b"\x1b[?25h",
        b"\x08",
        "\u{fe0f}\u{1f600}".as_bytes(),
    ];
    for iteration in 0..300 {
        let mut rig = Rig::new();
        if iteration % 2 == 0 {
            rig.session.parse_in_background();
        }
        rig.remote(b"$ ");
        let mut unechoed: VecDeque<u8> = VecDeque::new();
        for _ in 0..60 {
            match random(10) {
                0..=3 => {
                    let key = b'a' + random(26) as u8;
                    rig.type_keys(&[key]);
                    unechoed.push_back(key);
                }
                4..=6 => {
                    // Echo some characters, sometimes split mid-sequence.
                    let mut echo = Vec::new();
                    for _ in 0..=random(3) {
                        if let Some(key) = unechoed.pop_front() {
                            echo.push(key);
                        }
                    }
                    if random(4) == 0 && echo.len() > 1 {
                        let (left, right) = echo.split_at(1);
                        rig.remote(left);
                        rig.remote(right);
                    } else {
                        rig.remote(&echo);
                    }
                }
                7 => {
                    let bytes = noise[random(noise.len() as u64) as usize];
                    let split = random(bytes.len() as u64 + 1) as usize;
                    rig.remote(&bytes[..split]);
                    rig.remote(&bytes[split..]);
                }
                8 => rig.advance(Duration::from_millis(random(120))),
                _ => {
                    if random(5) == 0 {
                        rig.type_keys(b"\r");
                        unechoed.clear();
                        rig.remote(b"\r\n$ ");
                    }
                }
            }
        }
        let rest: Vec<u8> = unechoed.drain(..).collect();
        rig.remote(&rest);
        rig.advance(Duration::from_millis(100));
        rig.advance(Duration::from_secs(2));
        rig.assert_authoritative();
    }
}

#[test]
fn predictions_look_exactly_like_the_echo_by_default() {
    let mut rig = Rig::new();
    rig.session.predictor.underline = false;
    rig.trained_shell();
    rig.type_keys(b"bc");
    assert_eq!(rig.terminal.row(0), "$ abc");
    assert!(!rig.terminal.underlined(0, 3));
    assert_eq!(rig.terminal.cursor(), (0, 5));
    rig.remote(b"bc");
    rig.assert_authoritative();
}

#[test]
fn fast_local_programs_are_only_measured_never_touched() {
    let mut session = Session::new(ROWS, COLS, true, crate::predict::DEFAULT_LIFETIME);
    let now = Instant::now();
    let mut io = Io::default();
    session.set_foreground(42, now, &mut io);
    // A local shell echoes immediately. Partial sequences and lone escapes go through
    // untouched, and nothing of foretype's own is ever written.
    session.output(b"$ ", now, &mut io);
    for (index, key) in b"vim notes".iter().enumerate() {
        let at = now + Duration::from_millis(index as u64 * 50);
        session.input(&[*key], at, &mut io);
        session.output(&[*key], at + Duration::from_millis(1), &mut io);
    }
    for chunk in [&b"\x1b[3"[..], b"1mx\x1b", b"[0m", "\u{e9}".as_bytes()] {
        session.output(chunk, now + Duration::from_secs(1), &mut io);
    }
    session.input(b"\x1b", now + Duration::from_secs(1), &mut io);
    assert_eq!(io.terminal, "$ vim notes\x1b[31mx\x1b[0m\u{e9}".as_bytes());
    assert_eq!(io.child, b"vim notes\x1b");
}

#[test]
fn a_laggy_program_is_boosted_after_its_first_echo() {
    let mut rig = Rig::measuring();
    let mut io = Io::default();
    rig.session.set_foreground(7, rig.now, &mut io);
    rig.deliver(io);
    rig.remote(b"local% ssh host\r\n$ ");
    rig.type_keys(b"a");
    rig.advance(Duration::from_millis(150));
    rig.remote(b"a");
    // 150 ms echo: from now on typing shows immediately.
    rig.type_keys(b"bc");
    assert_eq!(rig.terminal.row(1), "$ abc");
    assert!(rig.terminal.underlined(1, 3));
    rig.remote(b"bc");
    rig.assert_authoritative();
    // The job ends and the shell is back in the foreground: measured from scratch.
    let mut io = Io::default();
    rig.session.set_foreground(1, rig.now, &mut io);
    rig.deliver(io);
    rig.remote(b"\r\nlocal% ");
    rig.type_keys(b"l");
    rig.advance(Duration::from_millis(1));
    rig.remote(b"l");
    rig.type_keys(b"s");
    assert_eq!(rig.terminal.row(2), "local% l");
}

#[test]
fn boosting_stops_when_the_echo_becomes_fast() {
    let mut rig = Rig::new();
    rig.remote(b"$ ");
    for (index, key) in b"abcdefghij".iter().enumerate() {
        rig.type_keys(&[*key]);
        rig.advance(Duration::from_millis(1));
        rig.remote(&[*key]);
        if index == 9 {
            assert!(!rig.session.boosting);
        }
    }
    rig.assert_authoritative();
}

#[test]
fn keystrokes_are_never_delayed_when_no_query_is_outstanding() {
    let mut rig = Rig::new();
    rig.trained_shell();
    let mut io = Io::default();
    rig.session.input(b"\x1b", rig.now, &mut io);
    assert_eq!(io.child, b"\x1b");
    let mut io = Io::default();
    rig.session.input(b"[A", rig.now, &mut io);
    assert_eq!(io.child, b"[A");
    // The escape sequence still counts as a control key for prediction.
    rig.type_keys(b"b");
    rig.assert_authoritative();
}

#[test]
fn the_background_model_matches_parsing_inline() {
    let now = Instant::now();
    let mut inline = Session::new(ROWS, COLS, true, crate::predict::DEFAULT_LIFETIME);
    let mut background = Session::new(ROWS, COLS, true, crate::predict::DEFAULT_LIFETIME);
    background.parse_in_background();
    let chunks: [&[u8]; 6] = [
        b"$ ls\r\nfile\r\n",
        b"\x1b[31mred\x1b[m \x1b[s\x1b[1;30Hcorner\x1b[u",
        b"\x1b[3",
        b"bx\x1b]0;title\x07",
        "wide \u{4e2d} done\r\n$ ".as_bytes(),
        b"\x1b[?1049h\x1b[2Jalt\x1b[?1049l",
    ];
    for chunk in chunks {
        let (mut a, mut b) = (Io::default(), Io::default());
        inline.output(chunk, now, &mut a);
        background.output(chunk, now, &mut b);
        assert_eq!(a.terminal, b.terminal);
    }
    assert!(background.offloaded);
    // A keystroke brings the model back, identical to the inline one.
    let (mut a, mut b) = (Io::default(), Io::default());
    inline.input(b"x", now, &mut a);
    background.input(b"x", now, &mut b);
    assert!(!background.offloaded);
    assert_eq!(a.child, b.child);
    assert_eq!(
        inline.shadow.screen().contents_formatted(),
        background.shadow.screen().contents_formatted()
    );
    assert_eq!(
        inline.shadow.screen().cursor_position(),
        background.shadow.screen().cursor_position()
    );
}

/// `cargo test --release -- --ignored --nocapture throughput`
#[test]
#[ignore]
fn throughput() {
    let mut text = Vec::new();
    for index in 0..700_000 {
        text.extend_from_slice(
            format!("line {index} lorem ipsum dolor sit amet lorem ipsum dolor sit amet\r\n")
                .as_bytes(),
        );
    }
    let chunks: Vec<&[u8]> = text.chunks(16 * 1024).collect();
    let mb = text.len() as f64 / 1e6;

    let start = Instant::now();
    let mut scanner = crate::scan::Scanner::default();
    for chunk in &chunks {
        scanner.feed(chunk);
        std::hint::black_box(scanner.take_model());
    }
    let scan = start.elapsed();

    let start = Instant::now();
    let mut parser = vt100::Parser::new(45, 160, 0);
    for chunk in &chunks {
        parser.process(chunk);
    }
    let model = start.elapsed();

    let start = Instant::now();
    let mut session = Session::new(45, 160, true, crate::predict::DEFAULT_LIFETIME);
    let now = Instant::now();
    for chunk in &chunks {
        let mut io = Io::default();
        session.output(chunk, now, &mut io);
        std::hint::black_box(io);
    }
    let total = start.elapsed();
    for (name, time) in [("scanner", scan), ("vt100", model), ("session", total)] {
        println!(
            "{name:>8}: {:6.0} MB/s ({:?})",
            mb / time.as_secs_f64(),
            time
        );
    }
}
