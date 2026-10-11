//! Installing foretype as `ssh`.
//!
//! foretype is installed next to a symlink named `ssh` in a directory that comes before the
//! real ssh on the system PATH (`/usr/local/bin` by default). Every program that runs
//! `ssh` then runs foretype, which execs the real ssh untouched unless the session is an
//! interactive terminal.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const SYSTEM_BIN: &str = "/usr/local/bin";

struct Options {
    bin: PathBuf,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut bin = PathBuf::from(SYSTEM_BIN);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--user" => {
                bin = home().ok_or("HOME is not set")?.join(".local/bin");
            }
            "--bin-dir" => {
                bin = PathBuf::from(args.next().ok_or("--bin-dir needs a directory")?);
            }
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    Ok(Options { bin })
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn current_exe() -> io::Result<PathBuf> {
    std::env::current_exe()?.canonicalize()
}

/// Whether `path` is (a link to) a foretype binary.
pub fn is_foretype(path: &Path) -> bool {
    path.canonicalize().is_ok_and(|real| {
        real.file_name().is_some_and(|name| name == "foretype")
            || current_exe().is_ok_and(|me| me == real)
    })
}

fn permission_hint(error: &io::Error, verb: &str) -> String {
    if error.kind() == io::ErrorKind::PermissionDenied {
        let exe = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "foretype".into());
        format!(
            "{error}\n  {verb} {SYSTEM_BIN} needs root: sudo {exe} {verb}\n  (or `foretype {verb} --user` for ~/.local/bin, your account only)"
        )
    } else {
        error.to_string()
    }
}

pub fn install(args: &[String]) -> ExitCode {
    let options = match options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("foretype: {error}");
            return ExitCode::from(2);
        }
    };
    match install_into(&options) {
        Ok(()) => {
            report(&options.bin);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!(
                "foretype: install failed: {}",
                permission_hint(&error, "install")
            );
            ExitCode::from(1)
        }
    }
}

fn install_into(options: &Options) -> io::Result<()> {
    let me = current_exe()?;
    std::fs::create_dir_all(&options.bin)?;
    let binary = options.bin.join("foretype");
    let shim = options.bin.join("ssh");

    // Refuse to replace an ssh that is not ours before changing anything.
    if let Ok(meta) = std::fs::symlink_metadata(&shim)
        && !(meta.file_type().is_symlink() && is_foretype(&shim))
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not foretype; not touching it",
                shim.display()
            ),
        ));
    }

    if binary.canonicalize().ok() != Some(me.clone()) {
        // Copy then rename, so a running foretype is never replaced half written.
        let partial = options.bin.join(".foretype.partial");
        std::fs::copy(&me, &partial)?;
        set_mode(&partial, 0o755)?;
        std::fs::rename(&partial, &binary)?;
        println!("foretype: installed {}", binary.display());
    }
    if std::fs::symlink_metadata(&shim).is_err() {
        std::os::unix::fs::symlink("foretype", &shim)?;
        println!("foretype: linked {} -> foretype", shim.display());
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

pub fn uninstall(args: &[String]) -> ExitCode {
    let options = match options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("foretype: {error}");
            return ExitCode::from(2);
        }
    };
    let shim = options.bin.join("ssh");
    let binary = options.bin.join("foretype");
    let mut result = Ok(());
    if std::fs::symlink_metadata(&shim).is_ok_and(|meta| meta.file_type().is_symlink())
        && is_foretype(&shim)
    {
        result = std::fs::remove_file(&shim).map(|()| {
            println!("foretype: removed {}", shim.display());
        });
    }
    if result.is_ok() && binary.exists() {
        result = std::fs::remove_file(&binary).map(|()| {
            println!("foretype: removed {}", binary.display());
        });
    }
    match result {
        Ok(()) => {
            println!("foretype: ssh is plain ssh again");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!(
                "foretype: uninstall failed: {}",
                permission_hint(&error, "uninstall")
            );
            ExitCode::from(1)
        }
    }
}

/// Where `ssh` resolves on a PATH.
fn resolve(path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join("ssh"))
        .find(|candidate| candidate.is_file())
}

/// The PATH login shells and terminals get from `/etc/paths` (macOS, also typical Linux).
fn system_path() -> std::ffi::OsString {
    let listed = std::fs::read_to_string("/etc/paths").ok().map(|paths| {
        paths
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(":")
    });
    listed
        .unwrap_or_else(|| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into())
        .into()
}

fn describe(label: &str, path: Option<PathBuf>) -> bool {
    match path {
        Some(path) if is_foretype(&path) => {
            println!("  {label:<14} ssh -> {} (foretype)", path.display());
            true
        }
        Some(path) => {
            println!("  {label:<14} ssh -> {} (not foretype)", path.display());
            false
        }
        None => {
            println!("  {label:<14} ssh not found");
            false
        }
    }
}

fn report(bin: &Path) {
    println!("foretype: ssh resolution");
    let current = std::env::var_os("PATH").unwrap_or_default();
    let here = describe("this shell", resolve(&current));
    let system = describe("system PATH", resolve(&system_path()));
    match crate::find_real_ssh() {
        Some(real) => println!("  {:<14} {}", "real ssh", real.display()),
        None => println!("  {:<14} not found", "real ssh"),
    }
    if !here || !system {
        let on_path = std::env::split_paths(&current).any(|dir| dir == bin);
        if !on_path {
            println!(
                "foretype: {} is not on this PATH; add it before {} to activate",
                bin.display(),
                crate::find_real_ssh()
                    .and_then(|real| real.parent().map(Path::to_path_buf))
                    .map_or_else(|| "/usr/bin".to_owned(), |dir| dir.display().to_string())
            );
        } else if !here {
            println!("foretype: another ssh comes first on this PATH; foretype is not active here");
        }
    }
    println!("foretype: open a new terminal tab; programs started there get it too.");
}

pub fn status() -> ExitCode {
    let current = std::env::var_os("PATH").unwrap_or_default();
    println!("foretype {}", env!("CARGO_PKG_VERSION"));
    let here = describe("this shell", resolve(&current));
    let system = describe("system PATH", resolve(&system_path()));
    match crate::find_real_ssh() {
        Some(real) => println!("  {:<14} {}", "real ssh", real.display()),
        None => println!("  {:<14} not found", "real ssh"),
    }
    if here || system {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("foretype-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn installs_a_shim_and_refuses_foreign_ssh() {
        let bin = scratch("shim");
        let options = Options { bin: bin.clone() };
        install_into(&options).unwrap();
        assert!(is_foretype(&bin.join("ssh")));
        assert!(bin.join("foretype").is_file());
        // Installing again is a no-op.
        install_into(&options).unwrap();

        let foreign = scratch("foreign");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("ssh"), "#!/bin/sh\n").unwrap();
        let error = install_into(&Options {
            bin: foreign.clone(),
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!foreign.join("foretype").exists());
        let _ = std::fs::remove_dir_all(bin);
        let _ = std::fs::remove_dir_all(foreign);
    }
}
