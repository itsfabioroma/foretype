#!/usr/bin/env python3
"""A pretend remote console: a line editor that echoes every key after a delay.

Stands in for anything slow that is not ssh (a cloud shell, a serial console,
`kubectl exec`) so foretype can be tested without a network:

    python3 scripts/laggy.py 150     # 150 ms echo; Ctrl-D quits
"""

import os
import select
import sys
import termios
import time
import tty


def main():
    delay = float(sys.argv[1]) / 1000 if len(sys.argv) > 1 else 0.15
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    tty.setraw(fd)
    out = sys.stdout.buffer
    pending = []  # (due, bytes)
    line = bytearray()
    out.write(b"laggy$ ")
    out.flush()
    try:
        while True:
            now = time.monotonic()
            timeout = max(0.0, pending[0][0] - now) if pending else None
            ready, _, _ = select.select([fd], [], [], timeout)
            if ready:
                data = os.read(fd, 1024)
                if not data or b"\x04" in data:
                    break
                pending.append((time.monotonic() + delay, data))
            now = time.monotonic()
            while pending and pending[0][0] <= now:
                _, data = pending.pop(0)
                for byte in data:
                    if byte in (13, 10):
                        out.write(b"\r\n" + (b"you said: " + bytes(line) + b"\r\n" if line else b"") + b"laggy$ ")
                        line.clear()
                    elif byte == 127:
                        if line:
                            line.pop()
                            out.write(b"\b \b")
                    elif 32 <= byte < 127:
                        line.append(byte)
                        out.write(bytes([byte]))
                out.flush()
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)
        out.write(b"\r\n")
        out.flush()


if __name__ == "__main__":
    main()
