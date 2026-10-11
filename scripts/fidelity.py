#!/usr/bin/env python3
"""Check that foretype never changes what ends up on screen.

Runs the same keystroke script through plain ssh and through foretype in a private tmux
server, then compares the final screens including colors and attributes. The script
covers fast typing, editing keys, Enter, colored output, a full-screen editor, and a
window resize in the middle of typing.

    python3 scripts/fidelity.py --host myhost
"""

import argparse
import os
import shlex
import subprocess
import sys
import time

SOCKET = f"foretype-fidelity-{os.getpid()}"


def tmux(*args, check=True):
    return subprocess.run(
        ["tmux", "-L", SOCKET, *args], capture_output=True, text=True, check=check
    ).stdout


def keys(*items, literal=True):
    for item in items:
        if literal:
            tmux("send-keys", "-t", "t", "-l", item)
        else:
            tmux("send-keys", "-t", "t", item)


def wait_for(text, timeout=20):
    start = time.monotonic()
    while time.monotonic() - start < timeout:
        if text in tmux("capture-pane", "-p", "-t", "t"):
            return True
        time.sleep(0.05)
    raise SystemExit(f"timed out waiting for {text!r}:\n" + tmux("capture-pane", "-p", "-t", "t"))


def burst(text, gap=0.015):
    for character in text:
        keys(character)
        time.sleep(gap)


def scenario(command, host):
    tmux("kill-server", check=False)
    tmux("new-session", "-d", "-s", "t", "-x", "120", "-y", "32", command)
    tmux("set-option", "-g", "status", "off")
    # A fresh terminal tab: local shell first, then ssh.
    time.sleep(2)
    keys(f"ssh {host}")
    keys("Enter", literal=False)
    wait_for("$")
    keys(" export PS1='FID$ '; cd /tmp; clear")
    keys("Enter", literal=False)
    wait_for("FID$")
    # Fast typing with corrections.
    burst("echo hello wrold")
    keys("BSpace", "BSpace", "BSpace", "BSpace", literal=False)
    burst("orld again")
    keys("Left", "Left", "Left", literal=False)
    burst("XY")
    keys("End", literal=False)
    burst(" tail")
    time.sleep(0.6)
    keys("Enter", literal=False)
    wait_for("tail\n")
    # Colors and attributes.
    burst("printf '\\e[1;31mred\\e[0m \\e[4munder\\e[0m \\e[7minv\\e[0m\\n'")
    keys("Enter", literal=False)
    wait_for("inv")
    # Resize while a prediction may be on screen.
    burst("echo resize")
    tmux("resize-window", "-t", "t", "-x", "100", "-y", "30")
    time.sleep(0.4)
    burst(" done")
    time.sleep(0.6)
    keys("Enter", literal=False)
    wait_for("resize done\n")
    # Full-screen editor on the alternate screen.
    burst("vi -u NONE -N /tmp/foretype-fidelity.txt")
    keys("Enter", literal=False)
    time.sleep(1.5)
    keys("i")
    burst("typed inside vi")
    keys("Escape", literal=False)
    time.sleep(0.3)
    burst(":q!")
    keys("Enter", literal=False)
    wait_for("FID$", timeout=10)
    time.sleep(0.5)
    # Ctrl-C on a half typed line, then an unfinished draft left on screen.
    burst("this line is abandoned")
    # The remote tty discards unread input on SIGINT, so let the echo land first;
    # otherwise the result depends on network timing, not on foretype.
    time.sleep(0.6)
    keys("C-c", literal=False)
    time.sleep(0.5)
    burst("final draft line")
    time.sleep(1.5)
    screen = tmux("capture-pane", "-p", "-e", "-t", "t")
    keys("C-u", literal=False)
    keys("exit")
    keys("Enter", literal=False)
    time.sleep(0.5)
    tmux("kill-server", check=False)
    # The login banner differs between sessions (load, last login); compare from our
    # first prompt on.
    start = screen.find("FID$")
    return screen[start:] if start >= 0 else screen


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", required=True)
    parser.add_argument("--foretype", default=os.path.join(os.path.dirname(__file__), "..", "target", "release", "foretype"))
    args = parser.parse_args()
    host = shlex.quote(args.host)
    shell = os.environ.get("SHELL", "/bin/zsh")
    if os.path.basename(shell).startswith("foretype"):
        shell = "/bin/zsh"
    foretype = shlex.quote(os.path.abspath(args.foretype))
    plain = scenario(f"{shell} -il", host)
    predicted = scenario(f"env FORETYPE_SHELL={shell} {foretype} shell -l", host)
    if plain == predicted:
        print("ok: final screens are identical, attributes included")
        return 0
    import difflib

    print("FAIL: screens differ")
    for line in difflib.unified_diff(
        plain.splitlines(), predicted.splitlines(), "ssh", "foretype", lineterm=""
    ):
        print(repr(line))
    return 1


if __name__ == "__main__":
    sys.exit(main())
