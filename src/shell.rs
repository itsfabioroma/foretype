//! Running as the login shell. The user's real shell runs inside foretype's pty; only
//! interactive terminal sessions are wrapped, everything else becomes the real shell.

use std::ffi::{OsStr, OsString};
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

/// Set to `off` to keep foretype from loading Ghostty's zsh integration.
pub const GHOSTTY_VAR: &str = "FORETYPE_GHOSTTY";

fn off(value: &OsStr) -> bool {
    matches!(
        value.to_string_lossy().to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// Ghostty injects its zsh integration (cwd reports, prompt marks, titles) only when
/// the command it starts is named `zsh`, so a zsh started by `foretype-shell` goes
/// without. This does for that zsh what Ghostty does for its own: point `ZDOTDIR` at
/// the installed integration, keeping any previous `ZDOTDIR` in `GHOSTTY_ZSH_ZDOTDIR`.
/// Ghostty's `.zshenv` there restores it, sources the user's `.zshenv` and loads the
/// integration. Ghostty sets no variable telling whether its integration is turned
/// off, hence `FORETYPE_GHOSTTY=off`.
pub fn ghostty_zsh(
    shell: &Path,
    var: impl Fn(&str) -> Option<OsString>,
) -> Vec<(OsString, OsString)> {
    let ghostty = var("TERM_PROGRAM").is_some_and(|program| program == "ghostty");
    let already = var("GHOSTTY_ZSH_ZDOTDIR").is_some();
    if shell.file_name() != Some(OsStr::new("zsh"))
        || !ghostty
        || already
        || var(GHOSTTY_VAR).is_some_and(|value| off(&value))
    {
        return Vec::new();
    }
    let Some(resources) = var("GHOSTTY_RESOURCES_DIR").filter(|dir| !dir.is_empty()) else {
        return Vec::new();
    };
    let integration = Path::new(&resources).join("shell-integration/zsh");
    let zdotdir = var("ZDOTDIR");
    // Without both files zsh would skip the user's `.zshenv` or load nothing at all.
    let readable = |name: &str| {
        let path = integration.join(name);
        path.is_file() && std::fs::File::open(path).is_ok()
    };
    if !integration.is_absolute()
        || !readable(".zshenv")
        || !readable("ghostty-integration")
        || zdotdir.as_deref() == Some(integration.as_os_str())
    {
        return Vec::new();
    }
    zdotdir
        .map(|old| ("GHOSTTY_ZSH_ZDOTDIR".into(), old))
        .into_iter()
        .chain([("ZDOTDIR".into(), integration.into_os_string())])
        .collect()
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
    let disabled = std::env::var_os("FORETYPE").is_some_and(|value| off(&value));

    if terminal && !nested && !dumb && !disabled && interactive_invocation(args) {
        if let Ok(path) = std::env::var("FORETYPE_LOG") {
            let _ = crate::log::open(Path::new(&path));
        }
        let env = ghostty_zsh(&shell, |name| std::env::var_os(name));
        match crate::run(&shell, &arg0, args, &env) {
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

    /// A Ghostty resources directory holding just the files foretype checks for.
    fn resources(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("foretype-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("shell-integration/zsh")).unwrap();
        for file in [".zshenv", "ghostty-integration"] {
            std::fs::write(dir.join("shell-integration/zsh").join(file), "").unwrap();
        }
        dir
    }

    fn ghostty_env<'a>(shell: &str, vars: &'a [(&'a str, &'a str)]) -> Vec<(String, String)> {
        ghostty_zsh(Path::new(shell), |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
    }

    #[test]
    fn zsh_under_ghostty_gets_its_integration_like_ghostty_does_it() {
        let dir = resources("inject");
        let resources = dir.to_str().unwrap();
        let integration = format!("{resources}/shell-integration/zsh");
        let ghostty = [
            ("TERM_PROGRAM", "ghostty"),
            ("GHOSTTY_RESOURCES_DIR", resources),
        ];
        let with = |extra: &[(&'static str, &'static str)]| -> Vec<(&str, &str)> {
            ghostty.iter().chain(extra).copied().collect()
        };
        let zdotdir = |value: &str| ("ZDOTDIR".to_owned(), value.to_owned());
        let saved = |value: &str| ("GHOSTTY_ZSH_ZDOTDIR".to_owned(), value.to_owned());

        // No ZDOTDIR: nothing to restore, the integration unsets its own.
        assert_eq!(ghostty_env("/bin/zsh", &with(&[])), [zdotdir(&integration)]);
        // A custom or even empty ZDOTDIR comes back exactly as it was.
        assert_eq!(
            ghostty_env("/bin/zsh", &with(&[("ZDOTDIR", "/u/my zsh")])),
            [saved("/u/my zsh"), zdotdir(&integration)]
        );
        assert_eq!(
            ghostty_env("/bin/zsh", &with(&[("ZDOTDIR", "")])),
            [saved(""), zdotdir(&integration)]
        );

        let none = Vec::<(String, String)>::new();
        // Other shells: Ghostty's zsh files mean nothing to them.
        assert_eq!(ghostty_env("/bin/bash", &with(&[])), none);
        assert_eq!(ghostty_env("/bin/zsh-5.9", &with(&[])), none);
        // Not Ghostty, or not directly (tmux sets its own TERM_PROGRAM).
        assert_eq!(
            ghostty_env("/bin/zsh", &[("GHOSTTY_RESOURCES_DIR", resources)]),
            none
        );
        let tmux = [
            ("TERM_PROGRAM", "tmux"),
            ("GHOSTTY_RESOURCES_DIR", resources),
        ];
        assert_eq!(ghostty_env("/bin/zsh", &tmux), none);
        // Already injected, by Ghostty or an outer foretype.
        assert_eq!(
            ghostty_env("/bin/zsh", &with(&[("GHOSTTY_ZSH_ZDOTDIR", "/u")])),
            none
        );
        let injected = [ghostty[0], ghostty[1], ("ZDOTDIR", integration.as_str())];
        assert_eq!(ghostty_env("/bin/zsh", &injected), none);
        // Turned off.
        assert_eq!(
            ghostty_env("/bin/zsh", &with(&[("FORETYPE_GHOSTTY", "off")])),
            none
        );
        assert_eq!(
            ghostty_env("/bin/zsh", &with(&[("FORETYPE_GHOSTTY", "0")])),
            none
        );
        // Nothing usable installed there.
        let missing = [
            ("TERM_PROGRAM", "ghostty"),
            ("GHOSTTY_RESOURCES_DIR", "/nonexistent"),
        ];
        assert_eq!(ghostty_env("/bin/zsh", &missing), none);
        let relative = [
            ("TERM_PROGRAM", "ghostty"),
            ("GHOSTTY_RESOURCES_DIR", "ghostty"),
        ];
        assert_eq!(ghostty_env("/bin/zsh", &relative), none);
        let empty = [("TERM_PROGRAM", "ghostty"), ("GHOSTTY_RESOURCES_DIR", "")];
        assert_eq!(ghostty_env("/bin/zsh", &empty), none);
        // Half installed: neither file may be missing or unreadable.
        use std::os::unix::fs::PermissionsExt as _;
        let files = dir.join("shell-integration/zsh");
        for file in [".zshenv", "ghostty-integration"] {
            let path = files.join(file);
            std::fs::remove_file(&path).unwrap();
            assert_eq!(ghostty_env("/bin/zsh", &with(&[])), none, "without {file}");
            std::fs::create_dir(&path).unwrap();
            assert_eq!(
                ghostty_env("/bin/zsh", &with(&[])),
                none,
                "{file} a directory"
            );
            std::fs::remove_dir(&path).unwrap();
            std::fs::write(&path, "").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads anything; the check is only meaningful for everyone else.
            if std::fs::File::open(&path).is_err() {
                assert_eq!(
                    ghostty_env("/bin/zsh", &with(&[])),
                    none,
                    "{file} unreadable"
                );
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(ghostty_env("/bin/zsh", &with(&[])), [zdotdir(&integration)]);
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    fn zsh() -> Option<PathBuf> {
        [
            "/bin/zsh",
            "/usr/bin/zsh",
            "/usr/local/bin/zsh",
            "/opt/homebrew/bin/zsh",
        ]
        .map(PathBuf::from)
        .into_iter()
        .find(|path| usable(path))
    }

    /// The installed Ghostty integration, if this machine has one. A directory named
    /// in `FORETYPE_TEST_GHOSTTY_RESOURCES` must hold one, so the test cannot skip itself.
    fn installed_ghostty() -> Option<PathBuf> {
        let usable = |dir: &Path| {
            dir.join("shell-integration/zsh/ghostty-integration")
                .is_file()
        };
        if let Some(dir) = std::env::var_os("FORETYPE_TEST_GHOSTTY_RESOURCES") {
            let dir = PathBuf::from(dir);
            assert!(
                usable(&dir),
                "no Ghostty zsh integration in {}",
                dir.display()
            );
            return Some(dir);
        }
        Some(PathBuf::from(
            "/Applications/Ghostty.app/Contents/Resources/ghostty",
        ))
        .filter(|dir| usable(dir))
    }

    /// Runs an interactive zsh the way foretype does inside Ghostty, with a home of its
    /// own, types `input` and returns everything it wrote.
    fn wrapped_zsh(
        zsh: &Path,
        resources: &Path,
        home: &Path,
        zdotdir: Option<&Path>,
        input: &str,
    ) -> String {
        let mut vars = vec![
            ("TERM_PROGRAM", OsString::from("ghostty")),
            ("GHOSTTY_RESOURCES_DIR", resources.into()),
        ];
        if let Some(zdotdir) = zdotdir {
            vars.push(("ZDOTDIR", zdotdir.into()));
        }
        let injected = ghostty_zsh(zsh, |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        });
        assert!(!injected.is_empty());
        // A clean environment: nothing from the terminal running the tests leaks in.
        let mut args: Vec<String> = ["-i", "PATH=/usr/bin:/bin", "TERM=xterm-256color"]
            .map(str::to_owned)
            .into();
        args.push(format!("HOME={}", home.display()));
        // Later assignments win, as foretype's own variables do over inherited ones.
        let assignments = vars
            .iter()
            .map(|(name, value)| (OsString::from(name), value.clone()));
        for (name, value) in assignments.chain(injected) {
            args.push(format!(
                "{}={}",
                name.to_string_lossy(),
                value.to_string_lossy()
            ));
        }
        args.push(zsh.display().to_string());
        args.push("-i".to_owned());
        let mut pty =
            crate::pty::spawn(Path::new("/usr/bin/env"), "env", &args, &[], None, None).unwrap();
        nix::unistd::write(&pty.master, input.as_bytes()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        while std::time::Instant::now() < deadline {
            let mut fds = [nix::poll::PollFd::new(
                pty.master.as_fd(),
                nix::poll::PollFlags::POLLIN,
            )];
            if nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(200u16)).unwrap_or(0) == 0 {
                if pty.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                continue;
            }
            match nix::unistd::read(&pty.master, &mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => output.extend_from_slice(&buffer[..n]),
            }
        }
        let _ = pty.child.kill();
        let _ = pty.child.wait();
        String::from_utf8_lossy(&output).into_owned()
    }

    fn count(text: &str, needle: &str) -> usize {
        text.matches(needle).count()
    }

    /// Against Ghostty's own installed scripts (macOS, or `FORETYPE_TEST_GHOSTTY_RESOURCES`).
    #[test]
    fn the_wrapped_zsh_reports_its_cwd_to_ghostty() {
        let (Some(zsh), Some(resources)) = (zsh(), installed_ghostty()) else {
            eprintln!("skipped: needs zsh and an installed Ghostty");
            return;
        };
        let root = std::env::temp_dir().join(format!("foretype-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let spaced = root.join("a b");
        std::fs::create_dir_all(&spaced).unwrap();
        let report = |text: &str| {
            text.split("\x1b]7;kitty-shell-cwd://").skip(1).any(|osc| {
                osc.split('\x07')
                    .next()
                    .is_some_and(|url| url.ends_with(&format!("{}", spaced.display())))
            })
        };
        let probe = format!(
            "cd '{}'\nprint -r -- \"STATE env=$USER_ZSHENV zd=${{ZDOTDIR-unset}} saved=${{GHOSTTY_ZSH_ZDOTDIR-unset}} pre=${{(j:,:)precmd_functions}} cwd=${{(j:,:)chpwd_functions}} END\"\nexit\n",
            spaced.display()
        );

        // Ghostty-like: no ZDOTDIR, the user's dotfiles in HOME, no workaround in them.
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".zshenv"), "export USER_ZSHENV=yes\n").unwrap();
        std::fs::write(home.join(".zshrc"), "PS1='> '\n").unwrap();
        let text = wrapped_zsh(&zsh, &resources, &home, None, &probe);
        assert!(report(&text), "no OSC 7 for {}: {text:?}", spaced.display());
        assert!(
            text.contains("STATE env=yes zd=unset saved=unset "),
            "{text:?}"
        );
        let state = text.split("STATE ").last().unwrap();
        assert_eq!(count(state, "_ghostty_precmd"), 1, "{text:?}");
        assert_eq!(count(state, "_ghostty_report_pwd"), 1, "{text:?}");
        assert_eq!(count(state, "_ghostty_deferred_init"), 0, "{text:?}");

        // Sourcing the integration again (the manual setup Ghostty documents) adds nothing.
        std::fs::write(
            home.join(".zshrc"),
            "PS1='> '\nsource \"$GHOSTTY_RESOURCES_DIR\"/shell-integration/zsh/ghostty-integration\n",
        )
        .unwrap();
        let text = wrapped_zsh(&zsh, &resources, &home, None, &probe);
        assert!(report(&text), "{text:?}");
        let state = text.split("STATE ").last().unwrap();
        assert_eq!(count(state, "_ghostty_precmd"), 1, "{text:?}");
        assert_eq!(count(state, "_ghostty_report_pwd"), 1, "{text:?}");

        // A custom ZDOTDIR with a space is where the dotfiles come from, and stays set.
        let custom = root.join("my zsh");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join(".zshenv"), "export USER_ZSHENV=custom\n").unwrap();
        std::fs::write(custom.join(".zshrc"), "PS1='> '\n").unwrap();
        let text = wrapped_zsh(&zsh, &resources, &home, Some(&custom), &probe);
        assert!(report(&text), "{text:?}");
        assert!(
            text.contains(&format!(
                "STATE env=custom zd={} saved=unset ",
                custom.display()
            )),
            "{text:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_real_shell_is_never_foretype() {
        let shell = real_shell();
        assert!(shell.is_absolute());
        assert!(!crate::install::is_foretype(&shell));
    }
}
