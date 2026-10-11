//! Installing foretype as the login shell.
//!
//! `foretype-shell` (a link to the foretype binary) becomes the user's login shell, so
//! every terminal, tmux pane and editor terminal starts inside foretype, which runs the
//! user's real shell in a pty. ssh itself is never replaced.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::shell::{SHELL_NAME, config_file};

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
fn account() -> Result<nix::unistd::User, String> {
    if !nix::unistd::geteuid().is_root() {
        let exe = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "foretype".into());
        return Err(format!(
            "this changes your login shell and /etc/shells, so it needs root:\n  sudo {exe} {}",
            std::env::args().nth(1).unwrap_or_default()
        ));
    }
    let name = std::env::var("SUDO_USER")
        .ok()
        .filter(|name| name != "root")
        .ok_or("run it with sudo from your own account, not as root")?;
    nix::unistd::User::from_name(&name)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("no such user `{name}`"))
}

pub fn install() -> ExitCode {
    let user = match account() {
        Ok(user) => user,
        Err(error) => {
            eprintln!("foretype: {error}");
            return ExitCode::from(1);
        }
    };
    match install_for(&user) {
        Ok(()) => {
            println!(
                "foretype: done. Open a new terminal tab; anything remote you type into there feels local."
            );
            println!("foretype: undo with `sudo foretype uninstall`.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("foretype: install failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn install_for(user: &nix::unistd::User) -> io::Result<()> {
    let me = std::env::current_exe()?.canonicalize()?;
    std::fs::create_dir_all(BIN)?;
    if binary().canonicalize().ok() != Some(me.clone()) {
        // Copy then rename, so a running foretype is never replaced half written.
        let partial = Path::new(BIN).join(".foretype.partial");
        std::fs::copy(&me, &partial)?;
        set_mode(&partial, 0o755)?;
        std::fs::rename(&partial, binary())?;
        println!("foretype: installed {}", binary().display());
    }
    if std::fs::symlink_metadata(shell_link()).is_err() {
        std::os::unix::fs::symlink("foretype", shell_link())?;
        println!("foretype: linked {} -> foretype", shell_link().display());
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
        println!("foretype: your shell stays {}", current.display());
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
            "foretype: registered {} in {SHELLS}",
            shell_link().display()
        );
    }
    if !is_foretype(&current) {
        set_login_shell(&user.name, &shell_link())?;
        println!(
            "foretype: login shell for {} is now {}",
            user.name,
            shell_link().display()
        );
    }
    Ok(())
}

pub fn uninstall() -> ExitCode {
    let user = match account() {
        Ok(user) => user,
        Err(error) => {
            eprintln!("foretype: {error}");
            return ExitCode::from(1);
        }
    };
    match uninstall_for(&user) {
        Ok(()) => {
            println!("foretype: removed. New terminal tabs start your shell directly again.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("foretype: uninstall failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn uninstall_for(user: &nix::unistd::User) -> io::Result<()> {
    let config = config_file(&user.dir);
    if is_foretype(&user.shell) {
        let real = std::fs::read_to_string(&config)
            .ok()
            .and_then(|text| text.lines().next().map(|line| PathBuf::from(line.trim())))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
        set_login_shell(&user.name, &real)?;
        println!(
            "foretype: login shell for {} is {} again",
            user.name,
            real.display()
        );
    }
    let shells = std::fs::read_to_string(SHELLS).unwrap_or_default();
    let updated = without_shell(&shells, &shell_link());
    if updated != shells {
        std::fs::write(SHELLS, updated)?;
        println!("foretype: removed {} from {SHELLS}", shell_link().display());
    }
    for path in [shell_link(), binary(), config] {
        if std::fs::symlink_metadata(&path).is_ok() {
            std::fs::remove_file(&path)?;
            println!("foretype: removed {}", path.display());
        }
    }
    Ok(())
}

pub fn status() -> ExitCode {
    println!("foretype {}", env!("CARGO_PKG_VERSION"));
    let user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten();
    let installed = user.as_ref().is_some_and(|user| is_foretype(&user.shell));
    if let Some(user) = &user {
        println!(
            "  login shell    {}{}",
            user.shell.display(),
            if installed { " (foretype)" } else { "" }
        );
    }
    println!("  real shell     {}", crate::shell::real_shell().display());
    let stdin = std::io::stdin();
    let wrapped = std::env::var_os(crate::shell::TTY_VAR).is_some_and(|tty| {
        nix::unistd::ttyname(std::os::fd::AsFd::as_fd(&stdin))
            .is_ok_and(|current| current.as_os_str() == tty)
    });
    println!(
        "  this terminal  {}",
        if wrapped {
            "active: slow programs get instant typing"
        } else {
            "not running inside foretype"
        }
    );
    if installed {
        ExitCode::SUCCESS
    } else {
        println!("  install with   sudo foretype install");
        ExitCode::from(1)
    }
}

fn set_login_shell(user: &str, shell: &Path) -> io::Result<()> {
    let status = Command::new("chsh").arg("-s").arg(shell).arg(user).status();
    if status.as_ref().is_ok_and(|status| status.success()) {
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
    Err(io::Error::other(format!(
        "could not set the login shell of {user}"
    )))
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
