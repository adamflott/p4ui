#!/usr/bin/env python3
"""Drive the actual binary through a PTY and submit to a disposable p4d.

Requires Unix, Python 3, p4, p4d, and `cargo build` first. Does not open a port.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import selectors
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
import time


def main():
    binary = Path(__file__).resolve().parents[1] / "target/debug/p4ui"
    with tempfile.TemporaryDirectory(prefix="p4ui-pty-") as tmp:
        root = Path(tmp)
        server, ws = root / "server", root / "workspace"
        server.mkdir()
        ws.mkdir()
        daemon = shutil.which("p4d")
        assert daemon, "p4d must be installed"
        port = f"rsh:{daemon} -r {server} -L {server}/log -i"
        env = {k: v for k, v in os.environ.items() if not k.startswith("P4")}
        env.update(P4CONFIG=".p4ui-no-config", P4ENVIRO=str(root / "env"),
                   P4TICKETS=str(root / "tickets"), P4TRUST=str(root / "trust"),
                   TERM="xterm-256color", P4EDITOR="true", VISUAL="true", EDITOR="true")
        args = ["-p", port, "-u", "tester", "-c", "test", "-d", str(ws)]

        def p4(*cmd, form=None):
            return subprocess.run(["p4", *args, *cmd], input=form, env=env,
                                  text=True, capture_output=True, check=True).stdout

        p4("info")  # Initialize before overriding secure defaults in this private fixture.
        for setting in ("security=0", "dm.user.noautocreate=0",
                        "dm.user.setinitialpasswd=1", "dm.user.hideinvalid=0"):
            subprocess.run([daemon, "-r", str(server), f"-cset {setting}"], check=True)
        p4("client", "-i", form=f"Client: test\nOwner: tester\nRoot: {ws}\nView:\n\t//depot/... //test/...\n")
        (ws / "hello.txt").write_text("hello from a real terminal\n")
        config = root / "config.toml"
        config.write_text("refresh_seconds = 0\n")
        master, slave = pty.openpty()
        before = termios.tcgetattr(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 140, 0, 0))
        process = subprocess.Popen([str(binary), *args, "--config", str(config)], env=env,
                                   stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
        selector = selectors.DefaultSelector()
        selector.register(master, selectors.EVENT_READ)
        captured = bytearray()

        def pump(seconds=0.5):
            end = time.monotonic() + seconds
            chunk = bytearray()
            while time.monotonic() < end:
                for key, _ in selector.select(min(0.05, max(0, end - time.monotonic()))):
                    try:
                        data = os.read(key.fd, 65536)
                        chunk.extend(data)
                        if b"\x1b[6n" in data:
                            os.write(master, b"\x1b[1;1R")
                    except OSError:
                        return bytes(chunk)
            captured.extend(chunk)
            return bytes(chunk)

        def send(keys, delay=0.5):
            os.write(master, keys)
            return pump(delay)

        try:
            pump(1)
            assert b"Offline / untracked" in captured, captured[-1000:]
            assert b"hello.txt" in captured, captured[-1000:]
            # Reconcile all, accept confirmation, dismiss result.
            send(b"A")
            result = send(b"y", 0.8)
            assert b"Command result" in result, result[-1000:]
            send(b"\r")
            # Open configured editor. It runs `true` and must restore the TUI.
            result = send(b"o", 0.8)
            assert b"\x1b[?1049l" in result and b"\x1b[?1049h" in result
            send(b"c")
            send(b"First submit through p4ui")
            send(b"\x13")  # Ctrl-S accepts multiline input.
            result = send(b"y", 0.8)
            assert b"Submitted" in result and b"change" in result, result[-1500:]
            send(b"\r")
            send(b"2")
            result = send(b"\r")
            assert b"hello.txt" in result, result[-1000:]
            send(b"\x1b")
            result = send(b"?", 0.3)
            assert b"Help" in result
            send(b"\x1b")
            send(b"q", 0.2)
            process.wait(timeout=5)
            assert process.returncode == 0
            assert termios.tcgetattr(slave) == before, "terminal settings were not restored"
            history = p4("-ztag", "-Mj", "changes", "-s", "submitted")
            assert "First submit through p4ui" in history
            status = subprocess.run([str(binary), *args, "--status", "--config", str(config)],
                                    env=env, text=True, capture_output=True, check=True)
            assert json.loads(status.stdout)["items"] == []
            # Handled termination must restore raw mode as well as a normal quit.
            process = subprocess.Popen([str(binary), *args, "--config", str(config)], env=env,
                                       stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
            pump(0.6)
            process.send_signal(signal.SIGTERM)
            pump(0.3)
            process.wait(timeout=5)
            assert process.returncode == 0
            assert termios.tcgetattr(slave) == before, "SIGTERM did not restore terminal settings"
            print("PTY workflow passed: reconcile → editor → submit → history/help → quit; terminal restored")
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            selector.close()
            os.close(master)
            os.close(slave)


if __name__ == "__main__":
    main()
