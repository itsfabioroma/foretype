//! Running as the login shell. The user's real shell runs inside foretype's pty; only
//! interactive terminal sessions are wrapped, everything else becomes the real shell.

use std::os::fd::AsFd;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// The name foretype answers to when it is the login shell.
pub const SHELL_NAME: &str = "foretype-shell";
/// Set for the wrapped shell: the pty it runs on. A foretype started on that same
/// terminal (`exec $SHELL`, `zsh -l`) must not wrap it a second time.
pub const TTY_VAR: &str = "FORETYPE_TTY";

pub fn config_file(home: &Path) -> PathBuf {
    home.join(".config/foretype/shell")
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).or_else(|| {
        nix::unistd::User::from_uid(nix::unistd::getuid())
            .ok()
            .flatten()
            .map(|user| user.dir)
    })
}

fn usable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.is_absolute()
        && path
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        && !crate::install::is_foretype(path)
}

/// The shell the user had before installing foretype.
pub fn real_shell() -> PathBuf {
    let configured = std::env::var_os("FORETYPE_SHELL")
        .map(PathBuf::from)
        .into_iter()
        .chain(home().and_then(|home| {
            std::fs::read_to_string(config_file(&home))
                .ok()
                .and_then(|text| text.lines().next().map(|line| PathBuf::from(line.trim())))
        }));
    configured
        .chain(["/bin/zsh", "/bin/bash", "/bin/sh"].map(PathBuf::from))
        .find(|path| usable(path))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Only plain interactive invocations are wrapped. Commands (`-c`), scripts and
/// anything unusual go straight to the real shell.
pub fn interactive_invocation(args: &[String]) -> bool {
    args.iter().all(|arg| {
        matches!(
            arg.as_str(),
            "-l" | "--login" | "-i" | "--interactive" | "-il" | "-li"
        )
    })
}

pub fn main(argv0: &str, args: &[String]) -> ExitCode {
    let shell = real_shell();
    let name = shell.file_name().map_or_else(
        || "sh".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    // Terminals start login shells with a leading dash in argv[0].
    let arg0 = if argv0.starts_with('-') {
        format!("-{name}")
    } else {
        shell.display().to_string()
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let terminal = nix::unistd::isatty(stdin.as_fd()).unwrap_or(false)
        && nix::unistd::isatty(stdout.as_fd()).unwrap_or(false);
    let nested = std::env::var_os(TTY_VAR).is_some_and(|wrapped| {
        nix::unistd::ttyname(stdin.as_fd()).is_ok_and(|tty| tty.as_os_str() == wrapped)
    });
    let dumb = std::env::var("TERM").map_or(true, |term| term.is_empty() || term == "dumb");
    let disabled = std::env::var("FORETYPE").is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    });

    if terminal && !nested && !dumb && !disabled && interactive_invocation(args) {
        if let Ok(path) = std::env::var("FORETYPE_LOG") {
            let _ = crate::log::open(Path::new(&path));
        }
        match crate::run(&shell, &arg0, args) {
            Ok(code) => return code,
            // Nothing was started yet: never leave a terminal without a shell.
            Err(crate::RunError::Setup(error)) => {
                crate::log!("setup failed, running the shell directly: {error}");
            }
            Err(crate::RunError::Session(error)) => {
                crate::ui::Problem::new("foretype stopped")
                    .note(error)
                    .print();
                return ExitCode::from(1);
            }
        }
    }
    let error = Command::new(&shell)
        .arg0(&arg0)
        .args(args)
        .env("SHELL", &shell)
        .exec();
    crate::ui::Problem::new(format!("foretype could not start {}", shell.display()))
        .note(error)
        .print();
    // Last resort so a broken configuration still yields a usable terminal.
    let error = Command::new("/bin/sh").arg0("-sh").exec();
    crate::ui::Problem::new("foretype could not start /bin/sh")
        .note(error)
        .print();
    ExitCode::from(127)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn only_plain_interactive_shells_are_wrapped() {
        assert!(interactive_invocation(&strings(&[])));
        assert!(interactive_invocation(&strings(&["-l"])));
        assert!(interactive_invocation(&strings(&["--login", "-i"])));
        assert!(!interactive_invocation(&strings(&["-c", "echo hi"])));
        assert!(!interactive_invocation(&strings(&["-lc", "make"])));
        assert!(!interactive_invocation(&strings(&["script.sh"])));
        assert!(!interactive_invocation(&strings(&["-s"])));
    }

    #[test]
    fn the_real_shell_is_never_foretype() {
        let shell = real_shell();
        assert!(shell.is_absolute());
        assert!(!crate::install::is_foretype(&shell));
    }
}
