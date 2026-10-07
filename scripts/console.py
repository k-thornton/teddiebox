#!/usr/bin/env python3
"""An interactive console on the box's UART0: type a command, watch it answer.

`scripts/bench-console.py` sends one line and captures until a marker, for
scripts. This is for interactive use, when a person needs to act between
commands, such as plugging in headphones between `t` and `stop`.

Line-based on purpose. A raw-mode terminal would send every keystroke, which
the firmware's parser does not need, and would leave the terminal broken if
this crashed.

**Only one program may use the port at a time.** Who holds it is found from
`/proc`, or from `lsof` where there is no `/proc` (macOS), not by matching
command lines: a name match would block on an editor that has this file open,
and miss a `cat /dev/ttyUSB0` or a stray picocom.

`--self-test` runs the logic below without a box attached.
"""

import argparse
import contextlib
import os
import select
import subprocess
import sys
import termios
import tty

# `PORT`, as flash.sh reads it, so one setting serves both. On macOS the
# adapter is a /dev/cu.usbserial-* device.
DEFAULT_PORT = os.environ.get("PORT", "/dev/ttyUSB0")


def frame(line):
    """A typed line as the firmware's parser wants it: CR-terminated, no LF.

    `teddiebox_console` ends a line on CR and never on LF, so passing a
    terminal's line through unchanged produces a command the box ignores in
    silence.
    """
    return line.rstrip("\r\n").encode() + b"\r"


def command_of(proc_root, pid):
    """A process's command line, for naming it in a message."""
    try:
        with open(os.path.join(proc_root, pid, "cmdline"), "rb") as f:
            raw = f.read()
    except OSError:
        return "?"
    return " ".join(raw.decode("utf-8", "replace").split("\0")).strip() or "?"


def port_owners(port, proc_root="/proc", my_pid=None):
    """Every process holding `port` open, as `pid command` lines.

    Processes belonging to another user are invisible here — their `fd`
    directory cannot be read — so an empty answer means "nothing of yours",
    which is the case that matters at a bench.
    """
    if not os.path.isdir(proc_root):
        return lsof_owners(port, my_pid)
    target = os.path.realpath(port)
    owners = []
    for pid in sorted((e for e in os.listdir(proc_root) if e.isdigit()), key=int):
        if my_pid is not None and int(pid) == my_pid:
            continue
        fd_dir = os.path.join(proc_root, pid, "fd")
        try:
            fds = os.listdir(fd_dir)
        except OSError:
            continue
        for fd in fds:
            try:
                link = os.readlink(os.path.join(fd_dir, fd))
            except OSError:
                continue
            if os.path.realpath(link) == target:
                owners.append(f"{pid} {command_of(proc_root, pid)}")
                break
    return owners


def lsof_owners(port, my_pid=None):
    """`port_owners` for a system without `/proc`, from `lsof`.

    `lsof -F pc` prints a `p<pid>` line, then a `c<command>` line, per
    process. It exits 1 when nothing holds the file, which is not an error.
    """
    result = subprocess.run(
        ["lsof", "-F", "pc", "--", port], capture_output=True, text=True, check=False
    )
    owners = []
    pid = None
    for line in result.stdout.splitlines():
        if line.startswith("p"):
            pid = int(line[1:])
        elif line.startswith("c") and pid is not None and pid != my_pid:
            owners.append(f"{pid} {line[1:]}")
    return owners


def configure(fd):
    """Sets the port to what the box's console speaks: 115200, raw, no echo.

    On the open descriptor rather than with an earlier `stty`: macOS resets a
    serial port's settings when its last descriptor closes, and `stty -F` is
    GNU-only.
    """
    tty.setraw(fd)
    attrs = termios.tcgetattr(fd)
    attrs[4] = attrs[5] = termios.B115200
    termios.tcsetattr(fd, termios.TCSANOW, attrs)


def self_test():
    import tempfile

    assert frame("t") == b"t\r"
    assert frame("t\n") == b"t\r"
    assert frame("net insecure no\r\n") == b"net insecure no\r"
    assert frame("") == b"\r"

    with tempfile.TemporaryDirectory() as root:
        port = os.path.join(root, "ttyUSB0")
        elsewhere = os.path.join(root, "ttyUSB1")
        open(port, "w").close()
        open(elsewhere, "w").close()
        proc = os.path.join(root, "proc")

        def fake(pid, command, links):
            fd_dir = os.path.join(proc, pid, "fd")
            os.makedirs(fd_dir)
            with open(os.path.join(proc, pid, "cmdline"), "wb") as f:
                f.write("\0".join(command).encode() + b"\0")
            for n, path in enumerate(links):
                os.symlink(path, os.path.join(fd_dir, str(n)))

        fake("123", ["cat", port], [port])
        fake("124", ["python3", "scripts/bench-console.py"], [elsewhere])
        # An editor with this file open names it and holds nothing.
        fake("125", ["vim", "scripts/console.py"], [])
        # A directory where a pid would be, and a pid with no readable fds:
        # both exist in a real /proc and neither is an owner.
        os.makedirs(os.path.join(proc, "self"))
        os.makedirs(os.path.join(proc, "126"))

        owners = port_owners(port, proc_root=proc)
        assert len(owners) == 1, owners
        assert owners[0].startswith("123 cat "), owners
        assert port_owners(port, proc_root=proc, my_pid=123) == []
        assert port_owners(elsewhere, proc_root=proc) == ["124 python3 scripts/bench-console.py"]
        # A second holder of the same port is reported too, rather than the
        # first one found standing in for all of them.
        fake("127", ["picocom", port], [port])
        assert len(port_owners(port, proc_root=proc)) == 2

        # A real process holding a file is found the way this system finds
        # one: from /proc on Linux, from lsof on macOS.
        held = os.path.join(root, "held")
        open(held, "w").close()
        with open(held) as f:
            holder = subprocess.Popen(["sleep", "30"], stdin=f)
        try:
            owners = port_owners(held)
            assert any(o.startswith(f"{holder.pid} ") for o in owners), owners
            assert port_owners(held, my_pid=holder.pid) == [], "own pid reported"
        finally:
            holder.kill()
            holder.wait()
        assert port_owners(elsewhere) == [], "an unheld file has an owner"

    # A port left at 9600 and cooked, as a freshly plugged adapter comes up,
    # ends at 115200 and raw.
    parent, child = os.openpty()
    try:
        attrs = termios.tcgetattr(child)
        attrs[4] = attrs[5] = termios.B9600
        attrs[3] |= termios.ICANON | termios.ECHO
        termios.tcsetattr(child, termios.TCSANOW, attrs)

        configure(child)

        _iflag, _oflag, _cflag, lflag, ispeed, ospeed, _cc = termios.tcgetattr(child)
        assert ispeed == ospeed == termios.B115200, (ispeed, ospeed)
        assert not lflag & (termios.ICANON | termios.ECHO), "still cooked"
    finally:
        os.close(parent)
        os.close(child)

    print("self-test ok")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", default=DEFAULT_PORT)
    ap.add_argument("--log", help="tee the session to a file, so it is still a record")
    ap.add_argument(
        "--check-owner",
        action="store_true",
        help="print whatever already holds the port and exit 3; for flash.sh",
    )
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        self_test()
        return

    owners = port_owners(args.port, my_pid=os.getpid()) if os.path.exists(args.port) else []

    if args.check_owner:
        # Lets `flash.sh` use the same check instead of its own. Matching
        # command lines misses a reader started with the default port, which
        # does not appear on its command line.
        for owner in owners:
            print(owner)
        sys.exit(3 if owners else 0)

    if not os.path.exists(args.port):
        sys.exit(f"console: no {args.port} — is the box plugged in?")
    if owners:
        held = "\n  ".join(owners)
        sys.exit(
            f"console: something is already reading {args.port}; "
            f"the port takes one owner — stop it first:\n  {held}"
        )

    with open(args.log, "wb") if args.log else contextlib.nullcontext() as log:
        fd = os.open(args.port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        # A port in the wrong mode shows nothing, which looks like a dead box.
        configure(fd)
        print(
            f"--- {args.port} at 115200. Type a command; Ctrl-C or Ctrl-D leaves.",
            file=sys.stderr,
        )
        try:
            while True:
                ready, _, _ = select.select([fd, sys.stdin], [], [], 0.2)
                if fd in ready:
                    try:
                        chunk = os.read(fd, 4096)
                    except BlockingIOError:
                        chunk = b""
                    if chunk:
                        sys.stdout.write(chunk.decode("utf-8", "replace"))
                        sys.stdout.flush()
                        if log:
                            log.write(chunk)
                            log.flush()
                if sys.stdin in ready:
                    line = sys.stdin.readline()
                    if not line:
                        break
                    os.write(fd, frame(line))
        except KeyboardInterrupt:
            pass
        finally:
            os.close(fd)
    print("\n--- port released", file=sys.stderr)


if __name__ == "__main__":
    main()
