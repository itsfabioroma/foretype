<div align="center">
  <h1>foretype</h1>
  <p><b>Remote made local.</b></p>
  <p>Type on a server across the world like it's your laptop.</p>
  <img alt="macOS and Linux" src="https://img.shields.io/badge/platform-macOS%20%C2%B7%20Linux-000000?style=for-the-badge&labelColor=000000">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-000000?style=for-the-badge&logo=rust&labelColor=000000">
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/github/license/itsfabioroma/foretype?style=for-the-badge&labelColor=000000&color=000000"></a>
</div>

<br>

<p align="center">
  <img src="demo/side-by-side.gif" alt="Plain ssh on top, foretype below, same keystrokes at the same time">
  <br>
  <sub>Top: plain ssh. Bottom: foretype. Same keys at the same moment, to a server ~440 ms away.</sub>
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

foretype times how long each key takes to show up. If it's slow, foretype shows your
key right away. If it guessed wrong, the key disappears.

It only guesses after the program has shown a key on that line, so passwords stay
hidden. It hides typing lag, not network lag.

## License

Apache-2.0
