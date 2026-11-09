#!/usr/bin/env python3
"""Log a serial port to a file until killed: serial-log.py <port> <logfile> [baud]

Each line is prefixed with the local time it arrived. Unreadable bytes are kept
as escapes, so a wrong baud rate still shows up as data. Survives the adapter
being unplugged: it waits for the port to come back.
"""
import os, select, sys, termios, time, tty

port, path = sys.argv[1], sys.argv[2]
baud = getattr(termios, f"B{sys.argv[3] if len(sys.argv) > 3 else 115200}")


def printable(raw):
    """ASCII text, with every other byte (NULs from line noise included) as
    a \\xNN escape, so the log stays a text file grep reads."""
    return "".join(
        chr(c) if 32 <= c < 127 or c == 9 else f"\\x{c:02x}" for c in raw
    )


def stamp(log, text):
    log.write(f"{time.strftime('%H:%M:%S')} {text}\n")
    log.flush()


with open(path, "a") as log:
    stamp(log, f"--- logging {port}")
    while True:
        try:
            fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        except OSError:
            time.sleep(1)
            continue
        tty.setraw(fd)
        attrs = termios.tcgetattr(fd)
        attrs[4] = attrs[5] = baud
        termios.tcsetattr(fd, termios.TCSANOW, attrs)
        buf = b""
        try:
            while True:
                if not select.select([fd], [], [], 0.5)[0]:
                    if buf:  # flush a partial line after a pause
                        stamp(log, printable(buf))
                        buf = b""
                    continue
                chunk = os.read(fd, 4096)
                if not chunk:
                    raise OSError("port closed")
                buf += chunk
                *lines, buf = buf.split(b"\n")
                for line in lines:
                    stamp(log, printable(line.rstrip(b"\r")))
        except OSError as e:
            if buf:
                stamp(log, printable(buf))
            stamp(log, f"--- port lost ({e}); waiting for it")
            os.close(fd)
            time.sleep(1)
