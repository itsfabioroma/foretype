//! Which ssh invocations are interactive terminal sessions.

/// OpenSSH options that take an argument.
const WITH_ARGUMENT: &[u8] = b"BbcDEeFIiJLlmOoPpQRSWw";

/// Whether ssh with these arguments opens an interactive terminal session. Anything
/// that is a remote command, a forward, a control request or a query is not.
pub fn interactive_ssh(args: &[String]) -> bool {
    let mut force_tty = false;
    let mut remote_command_option = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            index += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            break;
        };
        let bytes = flags.as_bytes();
        for (offset, &flag) in bytes.iter().enumerate() {
            match flag {
                b'N' | b'f' | b'G' | b'V' | b'T' | b's' => return false,
                b't' => force_tty = true,
                _ => {}
            }
            if WITH_ARGUMENT.contains(&flag) {
                let value = if offset + 1 < bytes.len() {
                    flags[offset + 1..].to_owned()
                } else {
                    index += 1;
                    args.get(index).cloned().unwrap_or_default()
                };
                match flag {
                    b'W' | b'O' | b'Q' => return false,
                    b'o' => match ssh_option(&value) {
                        Some(("requesttty", "no")) => return false,
                        Some(("requesttty", "yes" | "force")) => force_tty = true,
                        Some(("remotecommand", value)) if value != "none" => {
                            remote_command_option = true;
                        }
                        Some(("sessiontype", "none" | "subsystem")) => return false,
                        _ => {}
                    },
                    _ => {}
                }
                break;
            }
        }
        index += 1;
    }
    // The first operand is the destination; anything after it is a remote command,
    // which ssh runs without a terminal unless asked for one.
    let Some(_destination) = args.get(index) else {
        return false;
    };
    let remote_command = index + 1 < args.len() || remote_command_option;
    !remote_command || force_tty
}

/// Splits `Key=value` or `Key value`, lowercased.
fn ssh_option(option: &str) -> Option<(&'static str, &'static str)> {
    let (key, value) = option
        .split_once('=')
        .or_else(|| option.split_once(char::is_whitespace))?;
    let key = key.trim().to_ascii_lowercase();
    let value = value.trim().to_ascii_lowercase();
    // Leak-free static matching for the handful of options we care about.
    let key = match key.as_str() {
        "requesttty" => "requesttty",
        "remotecommand" => "remotecommand",
        "sessiontype" => "sessiontype",
        _ => return None,
    };
    let value = match value.as_str() {
        "no" => "no",
        "yes" => "yes",
        "force" => "force",
        "auto" => "auto",
        "none" => "none",
        "subsystem" => "subsystem",
        "default" => "default",
        _ => "other",
    };
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interactive(line: &str) -> bool {
        let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        interactive_ssh(&args)
    }

    #[test]
    fn interactive_logins_are_wrapped() {
        assert!(interactive("i"));
        assert!(interactive("-p 2222 user@host"));
        assert!(interactive("-p2222 -A -J jump host"));
        assert!(interactive("-o ServerAliveInterval=30 host"));
        assert!(interactive("-t host tmux attach"));
        assert!(interactive("-o RequestTTY=force host htop"));
        assert!(interactive("-- host"));
    }

    #[test]
    fn everything_else_goes_straight_to_ssh() {
        assert!(!interactive(""));
        assert!(!interactive("host uptime"));
        assert!(!interactive("-N -L 8080:localhost:80 host"));
        assert!(!interactive("-fN host"));
        assert!(!interactive("-T git@github.com"));
        assert!(!interactive("-W %h:%p jump"));
        assert!(!interactive("-O check host"));
        assert!(!interactive("-G host"));
        assert!(!interactive("-V"));
        assert!(!interactive("-s host sftp"));
        assert!(!interactive("-o RequestTTY=no host"));
        assert!(!interactive("-o RemoteCommand=uptime host"));
        assert!(!interactive(
            "-x -oBatchMode=yes host git-upload-pack 'repo'"
        ));
    }
}
