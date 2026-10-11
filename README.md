# foretype

**Remote made local.**

foretype is a Rust tool that makes remote typing instant with optimistic echo.

![Typing a prompt into a coding agent on a far server: plain ssh on top, foretype below](demo/foretype.gif)

<sub>The same prompt typed into a coding agent on a server ~440 ms away. Top: plain ssh.
Bottom: with foretype. Nothing is sent to the agent. Rebuild with `demo/render.sh`.</sub>

```
                       without     with foretype
bash over ssh           160 ms        10 ms
pi (agent) over ssh     148 ms        11 ms
any laggy console       158 ms        10 ms
```

<sub>Median keystroke-to-screen time to a server ~150 ms away, measured with
`scripts/latency.py`.</sub>

## Install

```sh
git clone https://github.com/itsfabioroma/foretype && cd foretype
./install.sh
```

## Usage

Open a new terminal tab. That's it. Typing into anything slow now feels instant: ssh,
`kubectl exec`, `docker exec`, a coding agent on a far box. Your shell stays the same.

```sh
foretype status             # is this tab boosted?
sudo foretype uninstall     # undo it
```

## How it works

foretype times how long each key takes to show up. If it's slow, foretype shows your
key right away. If it guessed wrong, the key disappears.

## License

Apache-2.0
