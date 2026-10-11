<div align="center">
  <br>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img alt="foretype" src="assets/logo-light.svg" width="420">
  </picture>
  <br>
  <br>
  <p><b>Remote made local.</b></p>
  <p>foretype is a Rust tool that makes remote typing instant with optimistic echo.</p>
  <img alt="macOS and Linux" src="https://img.shields.io/badge/platform-macOS%20%C2%B7%20Linux-000000?style=for-the-badge&labelColor=000000">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-000000?style=for-the-badge&logo=rust&labelColor=000000">
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/github/license/itsfabioroma/foretype?style=for-the-badge&labelColor=000000&color=000000"></a>
</div>

<br>

<p align="center">
  <img src="demo/instant.gif" alt="foretype on top, plain ssh below, same keystrokes at the same time">
  <br>
  <sub>Top: foretype. Bottom: plain ssh. Same keys at the same moment, to a server ~440 ms away.</sub>
</p>

```
                       without     with foretype
bash over ssh           160 ms        10 ms
pi (agent) over ssh     148 ms        11 ms
any laggy console       158 ms        10 ms
```

<sub>Median time from keypress to screen, server ~150 ms away. Measured with `scripts/latency.py`.</sub>

## Install

```sh
git clone https://github.com/itsfabioroma/foretype && cd foretype
./install.sh
```

## Usage

Open a new terminal tab. That's it. Typing into anything slow now feels instant: ssh,
`kubectl exec`, `docker exec`, a coding agent on a far box. Nothing to install on the
server.

foretype becomes your login shell and runs your real shell inside it, so your prompt and
config stay the same.

```sh
foretype status             # is this tab boosted?
sudo foretype uninstall     # put your old shell back
```

## How it works

foretype becomes your login shell and runs your real shell in a pseudo-terminal,
forwarding every byte unchanged. It mirrors the screen with a terminal emulator
([vt100](https://crates.io/crates/vt100)) and times how long each key takes to echo.
When the program in front is slower than 30 ms, foretype draws your keys itself, then
checks them against the real echo.

- Predicts only after the program has echoed on that line, so passwords stay hidden.
- Asks the terminal for its cursor position (`ESC [6n`) before drawing.
- Restores the cursor before forwarding output, so the program's bytes land untouched.
- Repaints from its screen copy if the echo differs or takes over 750 ms.
- Pauses on Enter, editing keys, paste and resize.
- Hides typing lag, not network lag.

## License

Apache-2.0
