# foretype

Optimistic typing for interactive ssh. Install it once, keep typing `ssh host` as
always, and what you type shows up immediately instead of one network round trip later.

```
             plain ssh    with foretype
bash prompt     146 ms          11 ms
pi (agent)      151 ms          11 ms
```

<sub>Median keystroke-to-screen time to a VPS ~150 ms away, measured with
`scripts/latency.py`.</sub>

Built for typing into coding agents on a remote box, where every keystroke in the prompt
otherwise waits for the server. It works for shells and editors too. Measured so far with
bash and Pi; the engine it is ported from was validated in herdr against Pi, Claude Code,
Codex and OpenCode, including editors that hide the cursor and paint their own caret.

## Install

```sh
git clone https://github.com/itsfabioroma/foretype && cd foretype
./install.sh            # system-wide: /usr/local/bin, asks for sudo
./install.sh --user     # or just your account: ~/.local/bin
```

foretype installs itself as `ssh`: the binary plus an `ssh` symlink in a directory that
comes before `/usr/bin` on PATH. From then on every program that runs `ssh` runs foretype:
your shell, scripts, tmux, editors' terminals, tools like herdr. There is nothing to
configure and no new command to learn; it delegates to the real `/usr/bin/ssh`.

```sh
foretype status             # which ssh do programs get?
sudo foretype uninstall     # remove it (foretype uninstall --user for a --user install)
FORETYPE=off ssh host       # skip it once
```

Nothing is installed on the server. No UDP ports, no daemon, no ssh config changes.

## What it does

foretype runs your real `ssh` on a local pseudo-terminal and sits between it and your
terminal:

- **Output is forwarded byte for byte.** foretype never re-renders the remote screen. It
  keeps a shadow model of the screen (`vt100`) fed with the same bytes, to know where the
  cursor is and what the prompt row looks like.
- **Echo is learned before anything is predicted.** The first character on each line goes
  out normally. Only when the remote side visibly echoes it at the cursor does foretype
  start drawing the next characters itself. Password prompts and other non-echoing
  input never light up.
- **Predictions are tentative and underlined.** They are drawn in blank cells after the
  cursor, then replaced by the real echo when it arrives. If the remote shows anything
  else, or nothing within 750 ms, they are withdrawn.
- **The real terminal is checked first.** foretype asks your terminal where its cursor is
  when it starts (it usually starts below your shell prompt) and after a resize. Before
  drawing on a line it asks again and compares the answer with its model. If they
  disagree, for example because of emoji width differences or reflow, that line is never
  predicted and the model is corrected for the next one.
- **The remote stream is never disturbed.** foretype only writes between escape
  sequences, outside the application's synchronized updates, and never while the
  application relies on its own saved cursor. Its drawing is bracketed with save/restore
  cursor, so every remote byte lands exactly where the remote application put it.

Prediction stops on Enter, editing and control keys, paste, mouse clicks, focus loss,
resize, and non-ASCII input. It resumes once the next character has been echoed.

## When it steps in

Only for interactive terminal sessions: stdin and stdout are a terminal, `TERM` is not
`dumb`, and ssh would open a login shell or was asked for a tty (`-t`). Everything else is
handed straight to the real ssh with `exec`, so there is no extra process, no overhead
and no change in behavior:

- `scp`, `rsync`, `git`, VS Code Remote, and anything else that runs ssh without a
  terminal or through pipes
- remote commands (`ssh host uptime`), unless `-t` is given
- `-N`, `-f`, `-T`, `-W`, `-O`, `-G`, `-V`, `-s`, `-o RequestTTY=no`, `RemoteCommand`
- ssh started by ssh itself (`ProxyCommand`, `ProxyJump`, `LocalCommand`)

If the terminal never answers foretype's cursor query (a pty driven by `expect` or
`script` rather than a terminal emulator), foretype becomes a plain pipe for the rest of
the session and writes nothing of its own.

Programs that run `/usr/bin/ssh` by absolute path, or that are started with a PATH
lacking `/usr/local/bin` (macOS GUI apps launched from the Dock, cron), get the real ssh.

## Limits

- It hides input latency. Command output, completions and the agent's answers still take
  as long as the network and the server do.
- It predicts printable ASCII typed at the end of a line. Mid-line editing, line wrap,
  and Unicode input still go to the server for their echo.
- Like mosh's prediction, it can't know whether a remote application has disabled echo.
  It waits for real echo on every line before predicting, so ordinary password prompts
  stay dark. An application that switches echo off in the middle of a line could briefly
  show a character.
- It is not mosh: there is no roaming and no recovery from a dropped TCP connection.

## Settings

| Variable | Default | |
|---|---|---|
| `FORETYPE` | on | `off` runs plain ssh |
| `FORETYPE_TIMEOUT_MS` | `750` | how long an unconfirmed character may stay on screen |
| `FORETYPE_STATS` | | print a one-line summary when the session ends |
| `FORETYPE_LOG` | | write a debug log to this file |

## Testing

```sh
cargo test                                   # unit and randomized session tests
python3 scripts/latency.py --host myhost     # keystroke latency, plain ssh vs foretype
python3 scripts/latency.py --host myhost --remote 'pi' --ready '(auto)'
python3 scripts/latency.py --host myhost --via-shell   # through the installed ssh
python3 scripts/fidelity.py --host myhost    # final screens must be identical
```

Both scripts drive a private tmux server as the local terminal and never submit
anything to the remote side.

## Origin

The prediction engine started as an opt-in remote-typing feature in a fork of
[herdr](https://github.com/herdrdev/herdr). foretype extracts it into a drop-in for ssh
itself, so it works with any terminal and any program that uses ssh.

## License

Apache-2.0
