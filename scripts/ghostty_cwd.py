#!/usr/bin/env python3
"""Check that a shell started by foretype inside Ghostty reports its directory.

Ghostty loads its zsh integration only into a command named `zsh`; foretype has to load
it into the zsh it wraps. This runs the real binary as `foretype shell` the way Ghostty
starts it, in a pty with a throwaway HOME, cds into a directory with a space in its name
and looks for Ghostty's OSC 7 cwd report and for hooks installed once only. With
FORETYPE_GHOSTTY=off the report must be gone.

    cargo build && python3 scripts/ghostty_cwd.py target/debug/foretype

Needs zsh and Ghostty's installed shell integration (the macOS app, or --resources).
"""

import argparse
import os
import pty
import select
import shutil
import sys
import tempfile
import time

APP = "/Applications/Ghostty.app/Contents/Resources/ghostty"
REPORT = b"\x1b]7;kitty-shell-cwd://"


def run(binary, env, script, seconds=20):
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(binary, [binary, "shell"], env)
    output = b""
    sent = False
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.2)
        if not ready:
            # Type only once foretype and zsh are both up and waiting.
            if not sent and output:
                os.write(fd, script.encode())
                sent = True
            continue
        try:
            chunk = os.read(fd, 4096)
        except OSError:
            break
        if not chunk:
            break
        output += chunk
        # Stand in for the terminal: answer foretype's cursor position queries.
        for _ in range(chunk.count(b"\x1b[6n")):
            os.write(fd, b"\x1b[1;1R")
    os.close(fd)
    os.waitpid(pid, 0)
    return output.decode("utf-8", "replace")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", help="the foretype binary to test")
    parser.add_argument("--resources", default=APP, help="Ghostty's resources directory")
    parser.add_argument("--zsh", default=shutil.which("zsh") or "/bin/zsh")
    args = parser.parse_args()
    for path in (".zshenv", "ghostty-integration"):
        if not os.path.isfile(os.path.join(args.resources, "shell-integration/zsh", path)):
            sys.exit(f"no Ghostty zsh integration in {args.resources}")

    root = tempfile.mkdtemp(prefix="foretype-ghostty-")
    try:
        home = os.path.join(root, "home")
        spaced = os.path.join(root, "a b")
        os.makedirs(home)
        os.makedirs(spaced)
        with open(os.path.join(home, ".zshenv"), "w") as file:
            file.write("export USER_ZSHENV=yes\n")
        with open(os.path.join(home, ".zshrc"), "w") as file:
            file.write("PS1='> '\n")
        env = {
            "PATH": "/usr/bin:/bin",
            "HOME": home,
            "TERM": "xterm-256color",
            "TERM_PROGRAM": "ghostty",
            "GHOSTTY_RESOURCES_DIR": args.resources,
            "GHOSTTY_SHELL_FEATURES": "cursor:blink,path,title",
            "FORETYPE_SHELL": args.zsh,
        }
        script = (
            f"cd '{spaced}'\n"
            'print -r -- "STATE env=$USER_ZSHENV zd=${ZDOTDIR-unset}'
            ' pre=${(j:,:)precmd_functions} cwd=${(j:,:)chpwd_functions} END"\n'
            "exit\n"
        )
        failures = []

        text = run(args.binary, env, script)
        reported = f"{spaced}\x07" in "".join(text.split(REPORT.decode())[1:])
        state = text.rsplit("STATE ", 1)[-1]
        if not reported:
            failures.append(f"no OSC 7 for {spaced!r}")
        if "env=yes zd=unset " not in state:
            failures.append("the user's .zshenv or ZDOTDIR was not restored")
        for hook in ("_ghostty_precmd", "_ghostty_report_pwd"):
            if state.count(hook) != 1:
                failures.append(f"{hook} installed {state.count(hook)} times")

        text = run(args.binary, {**env, "FORETYPE_GHOSTTY": "off"}, script)
        if REPORT.decode() in text:
            failures.append("FORETYPE_GHOSTTY=off still loaded the integration")
        if "STATE env=yes zd=unset pre= cwd= END" not in text:
            failures.append("FORETYPE_GHOSTTY=off did not start a plain zsh")

        for failure in failures:
            print(f"FAIL {failure}")
        if failures:
            print(repr(text[-2000:]))
            sys.exit(1)
        print("ok: OSC 7 for a directory with a space, hooks once, opt-out works")
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
