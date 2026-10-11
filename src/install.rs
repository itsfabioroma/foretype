//! Installing foretype as the login shell.
//!
//! `foretype-shell` (a link to the foretype binary) becomes the user's login shell, so
//! every terminal, tmux pane and editor terminal starts inside foretype, which runs the
//! user's real shell in a pty. ssh itself is never replaced.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::shell::{SHELL_NAME, config_file};
use crate::ui::{Mark, Problem, Style};

const BIN: &str = "/usr/local/bin";
const SHELLS: &str = "/etc/shells";

fn binary() -> PathBuf {
    Path::new(BIN).join("foretype")
}

fn shell_link() -> PathBuf {
    Path::new(BIN).join(SHELL_NAME)
}

/// Whether `path` is (a link to) a foretype binary.
pub fn is_foretype(path: &Path) -> bool {
    path.canonicalize().is_ok_and(|real| {
        real.file_name().is_some_and(|name| name == "foretype")
            || std::env::current_exe()
                .and_then(|exe| exe.canonicalize())
                .is_ok_and(|me| me == real)
    })
}

/// The account foretype is installed for: the user who ran sudo.
fn account() -> Result<nix::unistd::User, Problem> {
    let command = std::env::args().nth(1).unwrap_or_default();
    if !nix::unistd::geteuid().is_root() {
        let exe = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "foretype".into());
        return Err(Problem::new(format!("{command} needs root"))
            .note("it changes your login shell and /etc/shells")
            .fix(format!("sudo {exe} {command}")));
    }
    let name = std::env::var("SUDO_USER")
        .ok()
        .filter(|name| name != "root")
        .ok_or_else(|| {
            Problem::new(format!("{command} needs your account"))
                .note("run it with sudo from your own account, not as root")
        })?;
    nix::unistd::User::from_name(&name)
        .map_err(|error| Problem::new(format!("could not look up `{name}`")).note(error))?
        .ok_or_else(|| Problem::new(format!("no such user `{name}`")))
}

pub fn install() -> ExitCode {
    let user = match account() {
        Ok(user) => user,
        Err(problem) => {
            problem.print();
            return ExitCode::from(1);
        }
    };
    let style = Style::stdout();
    println!();
    match install_for(&user, style) {
        Ok(()) => {
            println!();
            println!(
                "  {} Open a new terminal tab; anything remote you type there feels local.",
                style.bold("Done.")
            );
            println!(
                "  {} {}",
                style.dim("Undo with"),
                style.bold("sudo foretype uninstall")
            );
            println!();
            ExitCode::SUCCESS
        }
        Err(error) => {
            Problem::new("install failed").note(error).print();
            ExitCode::from(1)
        }
    }
}

fn install_for(user: &nix::unistd::User, style: Style) -> io::Result<()> {
    let me = std::env::current_exe()?.canonicalize()?;
    std::fs::create_dir_all(BIN)?;
    if binary().canonicalize().ok() != Some(me.clone()) {
        // Copy then rename, so a running foretype is never replaced half written.
        let partial = Path::new(BIN).join(".foretype.partial");
        std::fs::copy(&me, &partial)?;
        set_mode(&partial, 0o755)?;
        std::fs::rename(&partial, binary())?;
        println!("{}", style.done("installed", binary().display()));
    }
    if std::fs::symlink_metadata(shell_link()).is_err() {
        std::os::unix::fs::symlink("foretype", shell_link())?;
        println!(
            "{}",
            style.done(
                "linked",
                format!("{} {} foretype", shell_link().display(), style.dim("→"))
            )
        );
    }

    // Remember the real shell before the login shell changes.
    let current = user.shell.clone();
    let config = config_file(&user.dir);
    if !is_foretype(&current) {
        if let Some(dir) = config.parent() {
            std::fs::create_dir_all(dir)?;
            chown(dir, user)?;
        }
        std::fs::write(&config, format!("{}\n", current.display()))?;
        chown(&config, user)?;
        println!(
            "{}",
            style.done(
                "kept",
                format!("{} {}", current.display(), style.dim("as your shell"))
            )
        );
    }
    let real = std::fs::read_to_string(&config)
        .ok()
        .and_then(|text| text.lines().next().map(|line| PathBuf::from(line.trim())))
        .unwrap_or_else(|| current.clone());

    // Prove the wrapper starts the real shell before anyone depends on it.
    let status = Command::new(shell_link())
        .args(["-c", "exit 7"])
        .env("FORETYPE_SHELL", &real)
        .status()?;
    if status.code() != Some(7) {
        return Err(io::Error::other(format!(
            "{} -c failed self-test ({status}); login shell left unchanged",
            shell_link().display()
        )));
    }

    let shells = std::fs::read_to_string(SHELLS).unwrap_or_default();
    if let Some(updated) = with_shell(&shells, &shell_link()) {
        std::fs::write(SHELLS, updated)?;
        println!(
            "{}",
            style.done(
                "registered",
                format!("{} {} {SHELLS}", shell_link().display(), style.dim("in"))
            )
        );
    }
    if !is_foretype(&current) {
        set_login_shell(&user.name, &shell_link())?;
        println!(
            "{}",
            style.done(
                "switched",
                format!(
                    "{} {}",
                    style.dim(format!("login shell of {} to", user.name)),
                    shell_link().display()
                )
            )
        );
    }
    Ok(())
}

pub fn uninstall() -> ExitCode {
    let user = match account() {
        Ok(user) => user,
        Err(problem) => {
            problem.print();
            return ExitCode::from(1);
        }
    };
    let style = Style::stdout();
    println!();
    match uninstall_for(&user, style) {
        Ok(()) => {
            println!();
            println!(
                "  {} New terminal tabs start your shell directly again.",
                style.bold("Removed.")
            );
            println!();
            ExitCode::SUCCESS
        }
        Err(error) => {
            Problem::new("uninstall failed").note(error).print();
            ExitCode::from(1)
        }
    }
}

fn uninstall_for(user: &nix::unistd::User, style: Style) -> io::Result<()> {
    let config = config_file(&user.dir);
    if is_foretype(&user.shell) {
        let real = std::fs::read_to_string(&config)
            .ok()
            .and_then(|text| text.lines().next().map(|line| PathBuf::from(line.trim())))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
        set_login_shell(&user.name, &real)?;
        println!(
            "{}",
            style.done(
                "restored",
                format!(
                    "{} {}",
                    style.dim(format!("login shell of {} to", user.name)),
                    real.display()
                )
            )
        );
    }
    let shells = std::fs::read_to_string(SHELLS).unwrap_or_default();
    let updated = without_shell(&shells, &shell_link());
    if updated != shells {
        std::fs::write(SHELLS, updated)?;
        println!(
            "{}",
            style.done(
                "removed",
                format!("{} {} {SHELLS}", shell_link().display(), style.dim("from"))
            )
        );
    }
    for path in [shell_link(), binary(), config] {
        if std::fs::symlink_metadata(&path).is_ok() {
            std::fs::remove_file(&path)?;
            println!("{}", style.done("removed", path.display()));
        }
    }
    Ok(())
}

pub fn status() -> ExitCode {
    let style = Style::stdout();
    println!();
    println!("  {}", style.title());
    println!();
    let user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten();
    let installed = user.as_ref().is_some_and(|user| is_foretype(&user.shell));
    if let Some(user) = &user {
        let mark = if installed { Mark::Good } else { Mark::Bad };
        println!("{}", style.check(mark, "login shell", user.shell.display()));
    }
    println!(
        "{}",
        style.check(
            Mark::None,
            "real shell",
            crate::shell::real_shell().display()
        )
    );
    let stdin = std::io::stdin();
    let wrapped = std::env::var_os(crate::shell::TTY_VAR).is_some_and(|tty| {
        nix::unistd::ttyname(std::os::fd::AsFd::as_fd(&stdin))
            .is_ok_and(|current| current.as_os_str() == tty)
    });
    println!(
        "{}",
        if wrapped {
            style.check(
                Mark::Good,
                "this terminal",
                format!("active {}", style.dim("· slow programs get instant typing")),
            )
        } else if installed {
            style.check(
                Mark::Idle,
                "this terminal",
                format!(
                    "not running inside foretype {}",
                    style.dim("· open a new tab")
                ),
            )
        } else {
            style.check(Mark::Idle, "this terminal", "not running inside foretype")
        }
    );
    println!();
    if installed {
        ExitCode::SUCCESS
    } else {
        println!(
            "  {} {}",
            style.dim("Install with"),
            style.bold("sudo foretype install")
        );
        println!();
        ExitCode::from(1)
    }
}

fn set_login_shell(user: &str, shell: &Path) -> io::Result<()> {
    // chsh narrates ("Changing shell for ..."); keep it for when it fails.
    let chsh = Command::new("chsh").arg("-s").arg(shell).arg(user).output();
    if chsh.as_ref().is_ok_and(|output| output.status.success()) {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("dscl")
            .args([".", "-create", &format!("/Users/{user}"), "UserShell"])
            .arg(shell)
            .status()?;
        if status.success() {
            return Ok(());
        }
    }
    let said = chsh
        .map(|output| String::from_utf8_lossy(&output.stderr).trim().to_owned())
        .unwrap_or_else(|error| error.to_string());
    Err(io::Error::other(if said.is_empty() {
        format!("could not set the login shell of {user}")
    } else {
        format!("could not set the login shell of {user}: {said}")
    }))
}

fn chown(path: &Path, user: &nix::unistd::User) -> io::Result<()> {
    nix::unistd::chown(path, Some(user.uid), Some(user.gid)).map_err(io::Error::from)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// `/etc/shells` with `shell` appended, or None if it is already listed.
fn with_shell(shells: &str, shell: &Path) -> Option<String> {
    let line = shell.display().to_string();
    if shells.lines().any(|existing| existing.trim() == line) {
        return None;
    }
    let mut updated = shells.to_owned();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&line);
    updated.push('\n');
    Some(updated)
}

fn without_shell(shells: &str, shell: &Path) -> String {
    let line = shell.display().to_string();
    shells
        .split_inclusive('\n')
        .filter(|existing| existing.trim() != line)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etc_shells_edits_are_idempotent() {
        let shell = Path::new("/usr/local/bin/foretype-shell");
        let original = "# List of acceptable shells\n/bin/bash\n/bin/zsh\n";
        let added = with_shell(original, shell).unwrap();
        assert_eq!(added, format!("{original}/usr/local/bin/foretype-shell\n"));
        assert!(with_shell(&added, shell).is_none());
        assert_eq!(without_shell(&added, shell), original);
        assert_eq!(without_shell(original, shell), original);
        assert_eq!(
            with_shell("/bin/sh", shell).unwrap(),
            "/bin/sh\n/usr/local/bin/foretype-shell\n"
        );
    }

    #[test]
    fn installing_needs_root() {
        if !nix::unistd::geteuid().is_root() {
            assert!(account().is_err());
        }
    }
}
