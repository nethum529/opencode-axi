import os, pty, select, signal, subprocess, threading, time, re

OPENCODE = "/home/nethum/.opencode/bin/opencode"
BASE = "http://127.0.0.1:4733"
CWD = "/home/nethum/opencode-axi-worktrees/T03"
ANSI = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07|\x1b[()][A-Z0-9]|\x1b[=><]")


class Tui:
    """Attach `opencode attach <url> -s <sid>` under a real PTY and capture the buffer."""

    def __init__(self, sid, cols=200, rows=50):
        self.sid = sid
        self.buf = bytearray()
        self.lock = threading.Lock()
        self.master, slave = pty.openpty()
        import fcntl, struct, termios
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        env = dict(os.environ)
        env["TERM"] = "xterm-256color"
        env.pop("CI", None)
        self.proc = subprocess.Popen(
            [OPENCODE, "attach", BASE, "-s", sid],
            stdin=slave, stdout=slave, stderr=slave,
            cwd=CWD, env=env, preexec_fn=os.setsid, close_fds=True,
        )
        os.close(slave)
        self.t0 = time.monotonic()
        self._stop = False
        self.thread = threading.Thread(target=self._pump, daemon=True)
        self.thread.start()

    def _pump(self):
        while not self._stop:
            try:
                r, _, _ = select.select([self.master], [], [], 0.2)
            except (OSError, ValueError):
                return
            if not r:
                continue
            try:
                data = os.read(self.master, 65536)
            except OSError:
                return
            if not data:
                return
            with self.lock:
                self.buf += data

    def text(self):
        with self.lock:
            raw = bytes(self.buf)
        return ANSI.sub("", raw.decode("utf-8", errors="replace"))

    def send(self, s):
        os.write(self.master, s.encode())

    def alive(self):
        return self.proc.poll() is None

    def close(self):
        self._stop = True
        try:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGTERM)
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except Exception:
            try:
                os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
            except Exception:
                pass
        try:
            os.close(self.master)
        except Exception:
            pass
