#!/usr/bin/env python3
"""Exercise viewport paging through the real TUI with a read-only fake P4 CLI."""
import fcntl
import json
import os
from pathlib import Path
import pty
import selectors
import struct
import subprocess
import sys
import tempfile
import termios
import time


def main():
    binary = Path(__file__).resolve().parents[1] / "target/debug/p4ui"
    with tempfile.TemporaryDirectory(prefix="p4ui-history-pty-") as directory:
        root = Path(directory)
        log = root / "calls.jsonl"
        cli = root / "p4-readonly"
        cli.write_text(f"#!{sys.executable}\n" + '''import json,sys
args=sys.argv[1:]
with open(__CALLS__,'a') as output: output.write(json.dumps(args)+'\\n')
if 'info' in args:
 print(json.dumps({'userName':'tester','clientName':'test','clientRoot':'/workspace'}))
elif 'changes' in args:
 assert not any(arg.startswith('-zmaxScanRows=') for arg in args)
 scope=args[-1];before=int(scope[4:]) if scope!='...' else 2**63
 maximum=int(args[args.index('-m')+1]);assert 1<=maximum<=100
 changes=[100000000-index*1009 for index in range(240)]
 for change in [change for change in changes if change<=before][:maximum]:
  print(json.dumps({'change':str(change),'desc':'History entry','user':'tester','client':'test','status':'submitted'}))
elif 'describe' in args: print('Change summary')
else: raise AssertionError('Unexpected P4 command: '+repr(args))
'''.replace("__CALLS__", repr(str(log))))
        cli.chmod(0o755)
        config = root / "config.toml"
        config.write_text("refresh_seconds=1\n")
        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 140, 0, 0))
        process = subprocess.Popen(
            [str(binary), "--p4", str(cli), "--config", str(config), "--tab", "log"],
            env=dict(os.environ, TERM="xterm-256color"), stdin=slave, stdout=slave,
            stderr=slave, start_new_session=True,
        )
        selector = selectors.DefaultSelector()
        selector.register(master, selectors.EVENT_READ)
        captured = bytearray()

        def pump(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                for key, _ in selector.select(0.03):
                    data = os.read(key.fd, 65536)
                    captured.extend(data)
                    if b"\x1b[6n" in data:
                        os.write(master, b"\x1b[1;1R")

        def calls():
            rows = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return [args for args in rows if "changes" in args]

        def wait_for(count, label):
            end = time.monotonic() + 5
            while time.monotonic() < end:
                pump(0.1)
                if len(calls()) >= count:
                    pump(0.3)
                    assert len(calls()) == count, f"Unexpected extra page after {label}"
                    return
            raise AssertionError(f"No history page after {label}: {captured[-1200:]!r}")

        def maximum(args):
            return int(args[args.index("-m") + 1])

        try:
            wait_for(1, "initial load")
            assert maximum(calls()[0]) == 28, "36-row terminal has 28 visible list rows"
            assert calls()[0][-1] == "..."
            assert b"100000000" in captured
            pump(1.2)
            assert len(calls()) == 1, "Idle history must not poll or prefetch"
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 48, 140, 0, 0))
            wait_for(2, "resize")
            assert maximum(calls()[1]) == 12, "Resize should fill only the missing rows"
            os.write(master, b"\x1b[6~")  # PageDown
            wait_for(3, "PageDown")
            assert maximum(calls()[2]) == 40
            for _ in range(14):
                os.write(master, b"\x1b[<65;2;6M")  # SGR wheel down over the list
                pump(0.02)
            wait_for(4, "mouse wheel")
            assert maximum(calls()[3]) == 40
            expected = [28, 12, 40, 40]
            consumed = 0
            for index, args in enumerate(calls()):
                assert maximum(args) == expected[index]
                if index:
                    assert args[-1] == f"...@{100000000-(consumed-1)*1009-1}"
                consumed += expected[index]
            pump(1.2)
            assert len(calls()) == 4, "Scrolling should not cause an idle page chain"
            os.write(master, b"q")
            process.wait(timeout=5)
            assert process.returncode == 0
            assert termios.tcgetattr(slave) == original
            print("History PTY passed: visible rows → resize → PageDown → mouse wheel; no idle polling; terminal restored")
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            selector.close()
            os.close(master)
            os.close(slave)


if __name__ == "__main__":
    main()
