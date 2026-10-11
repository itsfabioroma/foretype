<div align="center">
  <br>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img alt="foretype" src="assets/logo-light.svg" width="420">
  </picture>
  <br>
  <br>
  <p><b>Type on remote machines at local speed.</b></p>
  <p>A Rust tool that forecasts your typing, drawing each key before the server echoes it back.</p>
  <img alt="macOS and Linux" src="https://img.shields.io/badge/platform-macOS%20%C2%B7%20Linux-000000?style=for-the-badge&labelColor=000000">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-000000?style=for-the-badge&logo=rust&labelColor=000000">
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/github/license/itsfabioroma/foretype?style=for-the-badge&labelColor=000000&color=000000"></a>
</div>

<br>

<p align="center">
  <img src="demo/plain-vs-foretype.gif" alt="plain ssh on top, foretype below, same keystrokes at the same time">
  <br>
  <sub>Top: plain ssh. Bottom: foretype. Same keys at the same moment, to a deliberately laggy server ~440 ms away.</sub>
</p>

```
                       without     with foretype
bash over ssh           160 ms        10 ms
coding agent over ssh   148 ms        11 ms
any laggy console       158 ms        10 ms
```

<sub>A separate measurement, server ~150 ms away. Median time from keypress until the
character is on screen; with foretype that is the locally drawn prediction, and the real
echo still arrives at network speed. Measured with `scripts/latency.py`.</sub>

## Install

```sh
git clone https://github.com/itsfabioroma/foretype && cd foretype
./install.sh
```

To try it without installing, run `cargo build --release`, then
`FORETYPE_SHELL=$SHELL ./target/release/foretype shell`. That runs your shell inside
foretype in the current terminal; `exit` leaves.

## Usage

Open a new terminal tab. That's it. Typing into anything slow now feels instant: ssh,
`kubectl exec`, `docker exec`, a coding agent on a far box. Nothing to install on the
server, and your shell, prompt and config stay the same.

```sh
foretype status             # is this tab boosted?
sudo foretype uninstall     # put your old shell back
```

## How it works

foretype is a single Rust binary. `foretype install` copies it to `/usr/local/bin`, adds
`foretype-shell` to `/etc/shells`, makes that your login shell with `chsh`, and saves
your old shell in `~/.config/foretype/shell`. Every new terminal tab, tmux pane and
editor terminal then starts foretype, which runs your real shell in a pseudo-terminal
and forwards every byte unchanged. It mirrors the screen with
[vt100](https://crates.io/crates/vt100) and times how long each key takes to echo. When
the program in front is slower than 30 ms, foretype draws your keys itself, then checks
them against the real echo.

- Only interactive terminals are wrapped. `-c` commands, scripts and pipes get your real
  shell directly.
- ssh, your shell config and the server are untouched. Root is only needed to install
  and uninstall.
- Predicts only after the program has echoed on that line, so passwords stay hidden.
- Asks the terminal for its cursor position (`ESC [6n`) before drawing.
- Restores the cursor before forwarding output, so the program's bytes land untouched.
- Takes a prediction back if the echo differs, or if none arrives within about three
  round trips (750 ms at least, 5 s at most).
- Pauses on Enter, editing keys, paste and resize.
- Hides typing lag, not network lag.

## License

Apache-2.0
