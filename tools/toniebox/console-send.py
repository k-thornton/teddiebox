#!/usr/bin/env python3
"""Send one console line to the box and print what it answers for a second:
console-send.py <port> <line> [baud]

The port takes one owner, so stop serial-log.py first and restart it after.
"""
import os, select, sys, termios, time, tty

port, line = sys.argv[1], sys.argv[2]
baud = getattr(termios, f"B{sys.argv[3] if len(sys.argv) > 3 else 115200}")

fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
tty.setraw(fd)
attrs = termios.tcgetattr(fd)
attrs[4] = attrs[5] = baud
termios.tcsetattr(fd, termios.TCSANOW, attrs)
os.write(fd, (line + "\n").encode())
deadline = time.time() + 1.0
out = b""
while time.time() < deadline:
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            out += os.read(fd, 4096)
        except BlockingIOError:
            pass
os.close(fd)
sys.stdout.write(out.decode("ascii", "replace"))
