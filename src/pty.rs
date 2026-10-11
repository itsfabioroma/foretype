//! Running the real ssh on a pseudo-terminal, and the local terminal's raw mode.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use nix::pty::{Winsize, openpty};
use nix::sys::termios::{self, SetArg, Termios};

#[cfg(target_os = "macos")]
const TIOCSCTTY: libc::c_ulong = 0x2000_7461;
#[cfg(not(target_os = "macos"))]
const TIOCSCTTY: libc::Ioctl = libc::TIOCSCTTY;

pub fn winsize(fd: BorrowedFd<'_>) -> Option<Winsize> {
    let mut size = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes one winsize into the provided, properly sized struct.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    (result == 0 && size.ws_row > 0 && size.ws_col > 0).then_some(size)
}

pub fn set_winsize(fd: BorrowedFd<'_>, size: &Winsize) {
    // SAFETY: TIOCSWINSZ only reads the provided struct.
    unsafe {
        libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, size);
    }
}

pub struct Pty {
    pub master: OwnedFd,
    pub child: Child,
}

/// Starts `program` (as `arg0`) as the session leader of a new pty.
pub fn spawn(
    program: &std::path::Path,
    arg0: &str,
    args: &[String],
    size: Option<Winsize>,
    attrs: Option<&Termios>,
) -> io::Result<Pty> {
    let pty = openpty(size.as_ref(), attrs).map_err(io::Error::from)?;
    // Neither end may leak into the shell or its jobs: a job holding the master keeps
    // the terminal from ever hanging up, so closing the window would not end it.
    set_cloexec(pty.master.as_fd())?;
    set_cloexec(pty.slave.as_fd())?;
    let tty = nix::unistd::ttyname(&pty.slave).map_err(io::Error::from)?;
    let mut command = Command::new(program);
    command
        .arg0(arg0)
        .args(args)
        // Programs that read $SHELL see the real shell, not foretype.
        .env("SHELL", program)
        .env(crate::shell::TTY_VAR, tty)
        .stdin(Stdio::from(pty.slave.try_clone()?))
        .stdout(Stdio::from(pty.slave.try_clone()?))
        .stderr(Stdio::from(pty.slave));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    // `command` (and with it every slave descriptor) is dropped here, so the master
    // sees end-of-file once the child and its descendants exit.
    Ok(Pty {
        master: pty.master,
        child,
    })
}

fn set_cloexec(fd: BorrowedFd<'_>) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(io::Error::from)?;
    Ok(())
}

pub fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::from)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::from)?;
    Ok(())
}

/// Puts the local terminal in raw mode and restores it on drop.
pub struct RawMode {
    fd: OwnedFd,
    original: Termios,
}

impl RawMode {
    pub fn enable(fd: BorrowedFd<'_>) -> io::Result<Self> {
        let original = termios::tcgetattr(fd).map_err(io::Error::from)?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, SetArg::TCSADRAIN, &raw).map_err(io::Error::from)?;
        Ok(Self {
            fd: fd.try_clone_to_owned()?,
            original,
        })
    }

    pub fn original(&self) -> &Termios {
        &self.original
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.fd.as_fd(), SetArg::TCSADRAIN, &self.original);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_inherits_only_its_terminal() {
        let script =
            "for f in 3 4 5 6 7 8 9; do [ -e /dev/fd/$f ] && echo leaked-$f; done; echo checked";
        let mut pty = spawn(
            std::path::Path::new("/bin/sh"),
            "sh",
            &["-c".to_owned(), script.to_owned()],
            None,
            None,
        )
        .unwrap();
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        while let Ok(n) = nix::unistd::read(&pty.master, &mut buffer) {
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..n]);
            if output.windows(7).any(|window| window == b"checked") {
                break;
            }
        }
        let _ = pty.child.wait();
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("checked"), "{text}");
        assert!(!text.contains("leaked"), "{text}");
    }
}
