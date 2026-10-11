#!/usr/bin/env python3
"""Measure keystroke-to-screen latency over a real ssh session, with and without foretype.

Drives a private tmux server (its own socket; your tmux is untouched) as the local
terminal, types into the remote side one key at a time and records how long each
character takes to appear on screen. Nothing is submitted: the draft is cleared at the
end and the session exits.

    python3 scripts/latency.py --host myhost
    python3 scripts/latency.py --host myhost --remote 'cd /tmp && pi' --ready 'pi'

Exits nonzero if foretype's median is not well below plain ssh, or if any prediction is
left on screen after the remote has caught up.
"""

import argparse
import os
import shlex
import statistics
import subprocess
import sys
import time
import uuid

SOCKET = f"foretype-latency-{os.getpid()}"
WIDTH, HEIGHT = 140, 40


def tmux(*args, check=True):
    return subprocess.run(
        ["tmux", "-L", SOCKET, *args], capture_output=True, text=True, check=check
    ).stdout


def screen(escapes=False):
    args = ["capture-pane", "-p", "-t", "t"]
    if escapes:
        args.append("-e")
    return tmux(*args)


def wait_for(predicate, timeout, poll=0.002):
    start = time.monotonic()
    while time.monotonic() - start < timeout:
        text = screen()
        if predicate(text):
            return time.monotonic() - start, text
        time.sleep(poll)
    return None, screen()


def session(command, args):
    tmux("kill-server", check=False)
    env_args = []
    for key in ("FORETYPE_LOG", "FORETYPE_TIMEOUT_MS"):
        if os.environ.get(key):
            env_args += ["-e", f"{key}={os.environ[key]}"]
    tmux("new-session", "-d", "-s", "t", "-x", str(WIDTH), "-y", str(HEIGHT), *env_args, command)
    tmux("set-option", "-g", "status", "off")
    marker = f"R{uuid.uuid4().hex[:6]}"
    if args.via_shell:
        # A fresh interactive login shell: exactly what a user types, hook included.
        time.sleep(2)
        tmux("send-keys", "-t", "t", "-l", command_line(args, args.via_shell_mode))
        tmux("send-keys", "-t", "t", "Enter")
        time.sleep(1)
    elapsed, text = wait_for(lambda s: "$" in s or "#" in s or "%" in s, 20, poll=0.1)
    if elapsed is None:
        raise SystemExit(f"no shell prompt from {command!r}:\n{text}")
    tmux("send-keys", "-t", "t", "-l", f" export PS1='{marker}$ '; clear")
    tmux("send-keys", "-t", "t", "Enter")
    elapsed, _ = wait_for(lambda s: f"{marker}$" in s, 10, poll=0.05)
    if elapsed is None:
        raise SystemExit(f"remote prompt never appeared:\n{screen()}")
    if args.remote:
        tmux("send-keys", "-t", "t", "-l", args.remote)
        tmux("send-keys", "-t", "t", "Enter")
        elapsed, text = wait_for(lambda s: args.ready in s, 30, poll=0.1)
        if elapsed is None:
            raise SystemExit(f"{args.remote!r} never showed {args.ready!r}:\n{text}")
        time.sleep(args.settle)
    return marker


def command_line(args, mode):
    host = shlex.quote(args.host)
    return f"FORETYPE=off ssh {host}" if mode == "off" else f"ssh {host}"


def measure(label, command, args):
    session(command, args)
    text = args.text
    latencies = []
    for index in range(1, len(text) + 1):
        prefix = text[:index]
        tmux("send-keys", "-t", "t", "-l", text[index - 1])
        if prefix.endswith(" "):
            # capture-pane trims trailing blanks; spaces are checked by the next key.
            time.sleep(args.gap / 1000)
            continue
        elapsed, _ = wait_for(lambda s: prefix in s, 3)
        if elapsed is None:
            raise SystemExit(f"{label}: {prefix!r} never appeared:\n{screen()}")
        latencies.append(elapsed * 1000)
        time.sleep(args.gap / 1000)
    # Let the remote catch up, then check nothing tentative is left behind.
    time.sleep(1.5)
    final = screen(escapes=True)
    plain = screen()
    leftovers = text in plain and "\x1b[4m" in final
    # Clear the unsubmitted draft and leave. The session may already be gone.
    keys = ["C-u"] + (["C-c", "C-c", "C-d"] if args.remote else [])
    for key in keys:
        tmux("send-keys", "-t", "t", key, check=False)
        time.sleep(0.3)
    tmux("send-keys", "-t", "t", "-l", "exit", check=False)
    tmux("send-keys", "-t", "t", "Enter", check=False)
    time.sleep(0.5)
    tmux("kill-server", check=False)
    median = statistics.median(latencies)
    if args.verbose:
        print(f"{label:>8}: " + " ".join(f"{value:.0f}" for value in latencies))
    print(
        f"{label:>8}: median {median:6.1f} ms  p90 {sorted(latencies)[int(len(latencies) * 0.9)]:6.1f} ms"
        f"  first {latencies[0]:6.1f} ms  max {max(latencies):6.1f} ms"
    )
    return median, latencies, leftovers, plain


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", required=True)
    parser.add_argument("--foretype", default=os.path.join(os.path.dirname(__file__), "..", "target", "release", "foretype"))
    parser.add_argument("--remote", help="command to start on the remote host before typing")
    parser.add_argument("--ready", default="", help="text that shows the remote command is ready")
    parser.add_argument("--settle", type=float, default=2.0, help="seconds to wait after --ready")
    parser.add_argument("--text", default="the quick brown fox jumps over")
    parser.add_argument("--gap", type=float, default=90, help="ms between keystrokes")
    parser.add_argument("--verbose", action="store_true", help="print every latency")
    parser.add_argument(
        "--via-shell",
        action="store_true",
        help="type `ssh host` into a fresh interactive $SHELL, using the installed hook",
    )
    args = parser.parse_args()

    foretype = os.path.abspath(args.foretype)
    if args.via_shell:
        shell = os.environ.get("SHELL", "/bin/zsh")
        args.via_shell_mode = "off"
        base, _, _, _ = measure("ssh", f"{shell} -il", args)
        args.via_shell_mode = "on"
        fast, _, leftovers, plain = measure("foretype", f"{shell} -il", args)
    else:
        ssh = f"FORETYPE=off ssh -tt {shlex.quote(args.host)}"
        base, _, _, _ = measure("ssh", ssh, args)
        fast, _, leftovers, plain = measure("foretype", f"{shlex.quote(foretype)} ssh {shlex.quote(args.host)}", args)
    print(f"speedup: {base / fast:.1f}x")
    if leftovers:
        print("FAIL: underlined prediction left on screen:\n" + plain)
        return 1
    if fast > base / 2:
        print("FAIL: foretype was not at least twice as fast")
        return 1
    print("ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
