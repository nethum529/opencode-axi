import json, threading, time, urllib.request, queue

BASE = "http://127.0.0.1:4733"
FLASH = {"providerID": "opencode", "modelID": "deepseek-v4-flash-free"}


def req(method, path, body=None, timeout=180):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(BASE + path, data=data, method=method,
                               headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            raw = resp.read().decode()
            return resp.status, (json.loads(raw) if raw.strip() else None)
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, raw


class Events(threading.Thread):
    """Background SSE reader on /event. Records (monotonic_ms, type, payload)."""

    def __init__(self, session_filter=None):
        super().__init__(daemon=True)
        self.frames = []
        self.q = queue.Queue()
        self.session_filter = session_filter
        self.ready = threading.Event()
        self.t0 = None
        self._stop = False

    def run(self):
        r = urllib.request.Request(BASE + "/event")
        resp = urllib.request.urlopen(r, timeout=600)
        self.t0 = time.monotonic()
        self.ready.set()
        buf = b""
        for chunk in resp:
            if self._stop:
                break
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                line = line.decode(errors="replace").strip()
                if not line.startswith("data:"):
                    continue
                try:
                    ev = json.loads(line[5:].strip())
                except Exception:
                    continue
                ms = round((time.monotonic() - self.t0) * 1000)
                sid = self._sid(ev)
                if self.session_filter and sid and sid != self.session_filter:
                    continue
                rec = (ms, ev.get("type"), sid, ev)
                self.frames.append(rec)
                self.q.put(rec)

    @staticmethod
    def _sid(ev):
        p = ev.get("properties") or {}
        for probe in (p, p.get("info") or {}, p.get("data") or {}):
            if isinstance(probe, dict) and probe.get("sessionID"):
                return probe["sessionID"]
        info = p.get("info") or {}
        if isinstance(info, dict) and str(info.get("id", "")).startswith("ses"):
            return info["id"]
        return None

    def wait_for(self, pred, timeout):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                rec = self.q.get(timeout=max(0.05, end - time.monotonic()))
            except queue.Empty:
                return None
            if pred(rec):
                return rec
        return None

    def summary(self, types=None):
        return [(ms, t, sid) for ms, t, sid, _ in self.frames
                if types is None or t in types]


def start_events(session_filter=None, settle=1.0):
    ev = Events(session_filter)
    ev.start()
    ev.ready.wait(10)
    time.sleep(settle)
    return ev
