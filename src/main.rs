//! foretype: type the future.
//!
//! Installed as the login shell, foretype runs the user's real shell on a pty of its own
//! and passes everything through untouched. When the program in the foreground echoes
//! slowly (ssh, kubectl exec, a remote console, anything far away), keystrokes are shown
//! immediately and reconciled with what actually comes back.

mod input;
mod install;
mod log;
mod platform;
mod predict;
mod pty;
mod scan;
mod session;
mod shell;

use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

const USAGE: &str = "\
foretype: make remote machines feel local.

    sudo foretype install     set up every new terminal (once; your shell stays the same)
    sudo foretype uninstall   undo it
    foretype status           is foretype active in this terminal?

    foretype shell [-l]       try it in this terminal without installing

Environment:
    FORETYPE=off              start terminals without foretype
    FORETYPE_TIMEOUT_MS=750   how long an unconfirmed character may stay on screen
    FORETYPE_LOG=<file>       write a debug log
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().cloned().unwrap_or_default();
    let name = Path::new(&argv0)
        .file_name()
        .map(|name| name.to_string_lossy().trim_start_matches('-').to_owned())
        .unwrap_or_default();
    // Started as the login shell: `-foretype-shell`, or a terminal running it directly.
    if name == shell::SHELL_NAME || argv0.starts_with('-') {
        return shell::main(&argv0, &args[1..]);
    }
    match args.get(1).map(String::as_str) {
        Some("shell") => shell::main(&argv0, &args[2..]),
        Some("install") => install::install(),
        Some("uninstall") => install::uninstall(),
        Some("status") => install::status(),
        Some("-V" | "--version" | "version") => {
            println!("foretype {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("foretype: unknown command `{other}`\n");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn lifetime() -> Duration {
    std::env::var("FORETYPE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| (50..=10_000).contains(ms))
        .map_or(predict::DEFAULT_LIFETIME, Duration::from_millis)
}

pub enum RunError {
    /// Nothing was started; the caller can still run the shell directly.
    Setup(io::Error),
    /// The session was running.
    Session(io::Error),
}

/// Runs `program` on a pty of its own and proxies the terminal until it exits.
pub fn run(program: &Path, arg0: &str, args: &[String]) -> Result<ExitCode, RunError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let size = pty::winsize(stdout.as_fd()).or_else(|| pty::winsize(stdin.as_fd()));
    let raw = pty::RawMode::enable(stdin.as_fd()).map_err(RunError::Setup)?;
    let child = match pty::spawn(program, arg0, args, size, Some(raw.original())) {
        Ok(child) => child,
        Err(error) => {
            drop(raw);
            return Err(RunError::Setup(error));
        }
    };
    proxy(child, raw, size).map_err(RunError::Session)
}

fn proxy(
    mut child: pty::Pty,
    raw: pty::RawMode,
    size: Option<nix::pty::Winsize>,
) -> io::Result<ExitCode> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stdin_fd = stdin.as_fd();
    let stdout_fd = stdout.as_fd();

    let (rows, cols) = size.map_or((24, 80), |size| (size.ws_row, size.ws_col));
    pty::set_nonblocking(child.master.as_fd())?;

    let (signal_read, signal_write) = std::os::unix::net::UnixStream::pair()?;
    signal_read.set_nonblocking(true)?;
    signal_write.set_nonblocking(true)?;
    let resized = Arc::new(AtomicBool::new(false));
    let terminated = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGWINCH, resized.clone())?;
    for signal in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGQUIT,
    ] {
        signal_hook::flag::register(signal, terminated.clone())?;
    }
    for signal in [
        signal_hook::consts::SIGCHLD,
        signal_hook::consts::SIGWINCH,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGQUIT,
    ] {
        signal_hook::low_level::pipe::register(signal, signal_write.try_clone()?)?;
    }

    let mut session = session::Session::new(rows, cols, true, lifetime());
    session.parse_in_background();
    let mut to_child: Vec<u8> = Vec::new();
    let mut stdin_open = true;
    let mut buffer = vec![0u8; 64 * 1024];

    loop {
        let now = Instant::now();
        let timeout = session.deadline().map_or(PollTimeout::NONE, |deadline| {
            let wait = deadline.saturating_duration_since(now);
            // Round up so the deadline has really passed when we wake.
            let ms = wait.as_micros().div_ceil(1000).min(60_000) as u16;
            PollTimeout::from(ms)
        });
        let master = child.master.as_fd();
        let mut master_events = PollFlags::POLLIN;
        if !to_child.is_empty() {
            master_events |= PollFlags::POLLOUT;
        }
        let mut fds = vec![
            PollFd::new(master, master_events),
            PollFd::new(signal_read.as_fd(), PollFlags::POLLIN),
        ];
        if stdin_open {
            fds.push(PollFd::new(stdin_fd, PollFlags::POLLIN));
        }
        match poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => {}
            Err(error) => return Err(error.into()),
        }
        let ready = |fd: &PollFd<'_>| {
            fd.revents().is_some_and(|events| {
                events.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR)
            })
        };
        let master_ready = ready(&fds[0]);
        let master_writable = fds[0]
            .revents()
            .is_some_and(|events| events.contains(PollFlags::POLLOUT));
        let signal_ready = ready(&fds[1]);
        let stdin_ready = stdin_open && ready(&fds[2]);
        drop(fds);

        let now = Instant::now();
        let mut io = session::Io::default();
        // Each foreground job is measured on its own; only laggy ones get prediction.
        session.set_foreground(
            platform::foreground_group(child.master.as_fd()),
            now,
            &mut io,
        );
        if signal_ready {
            let mut sink = [0u8; 64];
            while matches!(io::Read::read(&mut &signal_read, &mut sink), Ok(n) if n > 0) {}
        }
        if terminated.load(Ordering::Relaxed) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.child.id() as i32),
                nix::sys::signal::Signal::SIGHUP,
            );
            break;
        }
        if resized.swap(false, Ordering::Relaxed)
            && let Some(size) = pty::winsize(stdout_fd)
        {
            pty::set_winsize(child.master.as_fd(), &size);
            session.resize(size.ws_row, size.ws_col, now, &mut io);
        }
        if stdin_ready {
            match nix::unistd::read(stdin_fd, &mut buffer) {
                Ok(0) => stdin_open = false,
                Ok(n) => session.input(&buffer[..n], now, &mut io),
                Err(nix::errno::Errno::EINTR | nix::errno::Errno::EAGAIN) => {}
                Err(_) => stdin_open = false,
            }
        }
        // Keystrokes go out before anything else is done with this iteration.
        to_child.extend_from_slice(&io.child);
        io.child.clear();
        if !to_child.is_empty() {
            write_some(child.master.as_fd(), &mut to_child)?;
        }
        write_all(stdout_fd, &io.terminal)?;
        io.terminal.clear();

        let mut child_gone = false;
        if master_ready {
            // Drain what is available, but keep input responsive during floods.
            for _ in 0..16 {
                match nix::unistd::read(child.master.as_fd(), &mut buffer) {
                    Ok(0) => {
                        child_gone = true;
                        break;
                    }
                    Ok(n) => session.output(&buffer[..n], Instant::now(), &mut io),
                    Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => break,
                    Err(_) => {
                        child_gone = true;
                        break;
                    }
                }
            }
        }
        if master_writable && !to_child.is_empty() {
            write_some(child.master.as_fd(), &mut to_child)?;
        }
        session.tick(Instant::now(), &mut io);
        to_child.extend_from_slice(&io.child);
        write_all(stdout_fd, &io.terminal)?;
        // A background ControlMaster can keep the pty open after ssh itself exits.
        if child_gone || (!master_ready && child.child.try_wait()?.is_some()) {
            break;
        }
    }

    let mut io = session::Io::default();
    session.finish(Instant::now(), &mut io);
    write_all(stdout_fd, &io.terminal)?;
    drop(raw);
    let status = child.child.wait()?;
    if std::env::var_os("FORETYPE_STATS").is_some() {
        print_stats(&session);
    }
    use std::os::unix::process::ExitStatusExt as _;
    Ok(match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        (None, Some(signal)) => ExitCode::from(u8::try_from(128 + signal).unwrap_or(255)),
        (None, None) => ExitCode::from(255),
    })
}

fn print_stats(session: &session::Session) {
    let stats = &session.predictor.stats;
    let mut echo = stats.echo_ms.clone();
    echo.sort_by(f64::total_cmp);
    let median = if echo.is_empty() {
        "n/a".to_owned()
    } else {
        format!("{:.0} ms", echo[echo.len() / 2])
    };
    eprintln!(
        "foretype: {} characters shown early, {} confirmed, {} withdrawn; median echo {}",
        stats.shown, stats.confirmed, stats.discarded, median
    );
    if session.cursor_checks_failed() > 0 {
        eprintln!(
            "foretype: {} lines skipped because the local model disagreed with the terminal",
            session.cursor_checks_failed()
        );
    }
}

fn write_all(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match nix::unistd::write(fd, bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => bytes = &bytes[n..],
            Err(nix::errno::Errno::EINTR) => {}
            Err(nix::errno::Errno::EAGAIN) => {
                let mut fds = [PollFd::new(fd, PollFlags::POLLOUT)];
                let _ = poll(&mut fds, PollTimeout::from(100u16));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn write_some(fd: BorrowedFd<'_>, pending: &mut Vec<u8>) -> io::Result<()> {
    while !pending.is_empty() {
        match nix::unistd::write(fd, pending) {
            Ok(n) => {
                pending.drain(..n);
            }
            Err(nix::errno::Errno::EAGAIN) => break,
            Err(nix::errno::Errno::EINTR) => {}
            // The child went away; its output side will report it.
            Err(nix::errno::Errno::EIO) => {
                pending.clear();
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
