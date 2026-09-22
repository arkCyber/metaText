#!/usr/bin/env python3
"""Phase 2: the network side of the interface, driven by a second instance.

The full screen interface is one of the two peers; the other is the line oriented
front-end, scripted over a pipe. That way each assertion is about what the user
of the interface sees while a real peer connects, sends, disconnects and comes
back.
"""

import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import (  # noqa: E402
    BINARY, CONFIG, ROOT, TuiSession, ask, check, expect, report,
)

ALICE_PORT = "34601"
BOB_PORT = "34602"
CAROL_PORT = "34603"
PASSPHRASE = "correct horse battery staple"


class CliPeer:
    """A second instance of metaText, scripted over a pipe."""

    def __init__(self, name: str, port: str, nick: str, passphrase: str,
                 peers: tuple = ()):
        self.name = name
        self.dir = f"{ROOT}/phase2/{name}"
        os.makedirs(f"{self.dir}/data", exist_ok=True)
        argv = [BINARY, "--mode", "cli", "-c", CONFIG, "-d", f"{self.dir}/data",
                "--port", port, "--nick", nick, "--passphrase", passphrase]
        for peer in peers:
            argv += ["--peer", peer]
        self.process = subprocess.Popen(argv, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE,
                                        stderr=subprocess.STDOUT, text=True,
                                        cwd=f"{self.dir}")
        self.output = ""

    def line(self, command: str, settle: float = 1.2) -> None:
        try:
            self.process.stdin.write(command + "\n")
            self.process.stdin.flush()
        except (BrokenPipeError, ValueError):
            pass
        time.sleep(settle)

    def finish(self, timeout: float = 30.0) -> str:
        try:
            self.process.stdin.write("/quit\n")
            self.process.stdin.flush()
        except (BrokenPipeError, ValueError):
            pass
        try:
            self.output = self.process.communicate(timeout=timeout)[0] or ""
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.output = self.process.communicate()[0] or ""
        return self.output

    def output_now(self) -> str:
        """Whatever the peer has printed so far, without waiting for it to end."""
        import fcntl
        import os as _os
        fd = self.process.stdout.fileno()
        flags = fcntl.fcntl(fd, fcntl.F_GETFL)
        fcntl.fcntl(fd, fcntl.F_SETFL, flags | _os.O_NONBLOCK)
        try:
            chunk = self.process.stdout.read()
        except (BlockingIOError, TypeError):
            chunk = ""
        finally:
            fcntl.fcntl(fd, fcntl.F_SETFL, flags)
        self.output += chunk or ""
        return self.output

    def kill(self) -> None:
        self.process.kill()
        self.output = self.process.communicate()[0] or ""


def log_file(directory: str) -> str:
    """The interface's own log file, which is where warnings are recorded."""
    logs = os.path.join(directory, "logs")
    if not os.path.isdir(logs):
        return ""
    names = sorted(os.listdir(logs))
    return os.path.join(logs, names[-1]) if names else ""


def read(path: str) -> str:
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            return handle.read()
    except OSError:
        return ""


def start_peer(name: str, port: str, nick: str, passphrase: str = PASSPHRASE,
               dial: bool = True):
    """Start the other side of the connection and let it announce itself."""
    peer = CliPeer(name, port, nick, passphrase,
                   peers=(f"127.0.0.1:{ALICE_PORT}",) if dial else ())
    peer.line(f"/nick {nick}", 2.0)
    peer.line("/add Alice")
    peer.line("/chat 1")
    return peer


def log_file(directory: str) -> str:
    """The interface's own log file, which is where warnings are recorded."""
    logs = os.path.join(directory, "logs")
    if not os.path.isdir(logs):
        return ""
    names = sorted(os.listdir(logs))
    return os.path.join(logs, names[-1]) if names else ""


def read(path: str) -> str:
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            return handle.read()
    except OSError:
        return ""


def main() -> int:
    os.system(f"rm -rf {ROOT}/phase2")
    alice = TuiSession("phase2/alice", port=ALICE_PORT, nick="Alice",
                       passphrase=PASSPHRASE).start()
    ask(alice, "/nick Alice", "nickname set to 'Alice'")
    ask(alice, "/add Bob", "added friend #1 : Bob")
    ask(alice, "/chat 1", "now chatting with #1 Bob")

    # A message to a peer that is not connected yet is buffered, and delivered
    # once a peer announces the matching nickname.
    ask(alice, "/msg Bob queued hello", "buffered as #",
        label="a message to an offline peer is buffered")

    bob = start_peer("bob", BOB_PORT, "Bob")
    expect(alice, "the interface sees the peer connect", "Bob connected")
    bob.line("/history 10", 3.0)
    queued = bob.finish(30.0)
    check("the buffered message reached the peer once it connected",
          "queued hello" in queued, queued[-400:])

    # A live exchange in both directions, driven from the interface side.
    bob = start_peer("bob", BOB_PORT, "Bob")
    expect(alice, "the peer reconnects", "Bob connected", timeout=12.0)

    ask(alice, "hello from the pane", "delivered to Bob",
        label="the interface's message is acknowledged")
    bob.line("hello from the cli", 3.0)
    expect(alice, "the peer's message is rendered in the pane",
           "\U0001f4e5 Bob: hello from the cli")

    bob.line("/me waves at the pane", 3.0)
    expect(alice, "the peer's third-person action is rendered", "waves at the pane")

    ask(alice, "/bin 00ff10", "delivered to Bob",
        label="a binary payload is sent and acknowledged")
    bob.line("/history 5", 3.0)
    exchanged = bob.output_now()
    check("the peer received the interface's text and binary payloads",
          "hello from the pane" in exchanged and "[binary, 3 B]" in exchanged,
          exchanged[-500:])

    ask(alice, "/peers", "1 peer(s) connected",
        label="/peers counts the connected peer")
    check("/peers shows the peer's pinned fingerprint",
          "1. bob \u2014" in alice.screen(), alice.tail())

    # A peer that leaves is reported, and a peer that returns is noticed.
    bob.finish(20.0)
    expect(alice, "the interface reports the disconnect", "disconnected")

    bob = start_peer("bob", BOB_PORT, "Bob")
    expect(alice, "a peer that comes back is noticed again", "Bob connected",
           timeout=12.0)

    # A peer with a different passphrase connects but its frames are unreadable.
    carol = start_peer("carol", CAROL_PORT, "Carol", "the wrong passphrase entirely")
    expect(alice, "a peer with another passphrase still connects", "Carol connected")
    carol.line("top secret from carol", 4.0)
    check("a frame sealed with another passphrase is not displayed",
          "top secret from carol" not in alice.screen(), alice.tail())
    carol.finish(15.0)

    status = alice.close()
    check("the interface exited cleanly after the network phase", status == 0, str(status))
    alice_log = read(log_file(alice.dir))
    check("the interface logged that it could not decrypt the frame",
          "Could not decrypt a message from" in alice_log, alice_log[-300:])

    # Persistence: the same data directory remembers nickname, friends and log.
    # No `--nick` here on purpose: a command line nickname would override the
    # persisted one, and this check is about what the session file remembered.
    alice2 = TuiSession("phase2/alice", port=ALICE_PORT).start()
    ask(alice2, "/nick", "nickname: Alice", label="the nickname survives a restart")
    ask(alice2, "/list", "1. Bob", label="the friend list survives a restart")
    ask(alice2, "/history 10", "stored message(s)",
        label="the stored history survives a restart")
    check("the stored history still holds the earlier conversation",
          "hello from the pane" in alice2.screen(), alice2.tail())
    alice2.close()
    return report("phase 2 (peers, buffering, restart, persistence)")


if __name__ == "__main__":
    raise SystemExit(main())

