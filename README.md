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
  <img src="demo/remote-made-local.gif" alt="foretype on top, plain ssh below, same keystrokes at the same time">
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

foretype runs your shell inside it and passes every byte through untouched. It keeps
its own copy of the screen and times how long each key takes to come back.

- **Slow programs get boosted.** If the program in front takes over 30 ms to echo,
  foretype draws your keys right away, in the same style the real echo will have. Fast
  ones, like your local shell, are left alone.
- **Wrong guesses vanish.** If the real echo doesn't match, or nothing comes back in
  750 ms, the guess is erased.
- **Passwords stay hidden.** foretype only guesses on a line after the program has
  echoed a key there. Password prompts never echo.
- **Output is never changed.** Before drawing, foretype asks the terminal where its
  cursor is and only draws if that matches its copy of the screen. It never writes in
  the middle of the program's output.

It hides typing lag, not network lag.

## License

Apache-2.0
