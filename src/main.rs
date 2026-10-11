//! foretype: optimistic typing for interactive ssh.
//!
//! Installed as `ssh` ahead of the real one on the system PATH, so every program that
//! runs ssh gets it. Interactive terminal sessions run through a local predictor that
//! shows typed characters immediately; everything else (scp, rsync, git, remote
//! commands, port forwards, pipes) is exec'd straight into the real ssh.

mod args;
mod input;
mod install;
mod log;
mod predict;
mod pty;
mod scan;
mod session;

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

const USAGE: &str = "\
foretype: optimistic typing for interactive ssh

Install once; from then on every program that runs ssh gets it:
    sudo foretype install     install as `ssh` in /usr/local/bin (ahead of /usr/bin/ssh)
    foretype install --user   same, in ~/.local/bin, for your account only
    foretype uninstall        remove it (add --user if installed with --user)
    foretype status           show which ssh programs will run

    foretype ssh [args]       run one ssh session through foretype explicitly

Environment:
    FORETYPE=off              run plain ssh for one command (FORETYPE=off ssh host)
    FORETYPE_TIMEOUT_MS=750   how long an unconfirmed character may stay on screen
    FORETYPE_STATS=1          print a short summary when the session ends
    FORETYPE_LOG=<file>       write a debug log
";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    let invoked_as_ssh = args
        .first()
        .and_then(|arg0| Path::new(arg0).file_name())
        .is_some_and(|name| name == "ssh");
    if invoked_as_ssh {
        args.remove(0);
        return ssh(&args);
    }
    let command = args.get(1).map(String::as_str);
    match command {
        Some("ssh") => ssh(&args[2..]),
        Some("install") => install::install(&args[2..]),
        Some("uninstall") => install::uninstall(&args[2..]),
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

/// Runs ssh with prediction when the session is an interactive terminal, and execs the
/// real ssh directly otherwise.
fn ssh(args: &[String]) -> ExitCode {
    let Some(real) = find_real_ssh() else {
        eprintln!("foretype: could not find ssh on PATH");
        return ExitCode::from(127);
    };
    let disabled = std::env::var("FORETYPE").is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    });
    let stdin = io::stdin();
    let stdout = io::stdout();
    let terminal = nix::unistd::isatty(stdin.as_fd()).unwrap_or(false)
        && nix::unistd::isatty(stdout.as_fd()).unwrap_or(false);
    let dumb = std::env::var("TERM").map_or(true, |term| term.is_empty() || term == "dumb");
    // ssh's own children (ProxyCommand, LocalCommand) that run `ssh` again.
    let nested = std::env::var_os(ACTIVE).is_some();
    if disabled || nested || dumb || !terminal || !args::interactive_ssh(args) {
        let error = Command::new(&real).args(args).env(ACTIVE, "1").exec();
        eprintln!("foretype: failed to run {}: {error}", real.display());
        return ExitCode::from(127);
    }
    if let Ok(path) = std::env::var("FORETYPE_LOG") {
        let _ = log::open(Path::new(&path));
    }
    match run(&real, args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("foretype: {error}");
            ExitCode::from(1)
        }
    }
}

const ACTIVE: &str = "FORETYPE_ACTIVE";

/// The first `ssh` on PATH that is not foretype, falling back to the system one.
pub(crate) fn find_real_ssh() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    std::env::split_paths(&path)
        .map(|dir| dir.join("ssh"))
        .chain([PathBuf::from("/usr/bin/ssh")])
        .filter(|candidate| is_executable(candidate))
        .find(|candidate| !install::is_foretype(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn lifetime() -> Duration {
    std::env::var("FORETYPE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| (50..=10_000).contains(ms))
        .map_or(predict::DEFAULT_LIFETIME, Duration::from_millis)
}

fn run(real: &Path, args: &[String]) -> io::Result<ExitCode> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stdin_fd = stdin.as_fd();
    let stdout_fd = stdout.as_fd();

    let size = pty::winsize(stdout_fd).or_else(|| pty::winsize(stdin_fd));
    let (rows, cols) = size.map_or((24, 80), |size| (size.ws_row, size.ws_col));
    let raw = pty::RawMode::enable(stdin_fd)?;
    let mut child = pty::spawn(real, args, size, Some(raw.original()))?;
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
    let mut io = session::Io::default();
    session.sync_cursor(Instant::now(), &mut io);
    write_all(stdout_fd, &io.terminal)?;
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
    Ok(match status.code() {
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        None => ExitCode::from(255),
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
