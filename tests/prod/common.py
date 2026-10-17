"""Shared helpers for the production test suite: start/stop/kill servers and talk to them."""
import atexit, json, os, shutil, signal, socket, subprocess, sys, tempfile, time, urllib.error, urllib.parse, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target", "release", "graphrd")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Server:
    def __init__(self, data=None, port=None, args=()):
        self.data = data or tempfile.mkdtemp(prefix="graphrd-prod-")
        self.port = port or free_port()
        self.args = list(args)
        self.proc = None
        self.log_path = self.data.rstrip("/") + ".log"
        self.log = open(self.log_path, "ab")
        if data is None and not os.environ.get("KEEP_DATA"):
            atexit.register(self.cleanup)

    def cleanup(self):
        self.stop()
        shutil.rmtree(self.data, ignore_errors=True)
        try:
            os.remove(self.log_path)
        except OSError:
            pass

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}"

    def start(self, timeout=60):
        self.proc = subprocess.Popen([BIN, "serve", "--port", str(self.port), "--data", self.data, *self.args], stdout=self.log, stderr=self.log)
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"server exited with {self.proc.returncode}")
            try:
                urllib.request.urlopen(self.url + "/protocol", timeout=1).read()
                return self
            except Exception:
                time.sleep(0.05)
        raise RuntimeError("server did not start")

    def kill9(self):
        self.proc.send_signal(signal.SIGKILL)
        self.proc.wait()

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.kill9()

    def alive(self):
        return self.proc.poll() is None

    def rss_mb(self):
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.proc.pid)], capture_output=True, text=True).stdout.strip()
        return int(out) / 1024 if out else 0

    # --- HTTP -------------------------------------------------------------
    def req(self, method, path, body=None, ctype=None, accept=None, timeout=60, auth=None):
        r = urllib.request.Request(self.url + path, data=body, method=method)
        if ctype:
            r.add_header("Content-Type", ctype)
        if accept:
            r.add_header("Accept", accept)
        if auth:
            r.add_header("Authorization", auth)
        try:
            with urllib.request.urlopen(r, timeout=timeout) as resp:
                return resp.status, resp.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    def create_repo(self, repo, ruleset="empty", extra=None):
        params = {"ruleset": {"value": ruleset}}
        params.update(extra or {})
        st, b = self.req("POST", "/rest/repositories", json.dumps({"id": repo, "params": params}).encode(), "application/json")
        assert st in (200, 201, 204), (st, b)

    def load(self, repo, data, ctype="application/n-quads"):
        st, b = self.req("POST", f"/repositories/{repo}/statements", data.encode() if isinstance(data, str) else data, ctype)
        assert st in (200, 204), (st, b[:300])

    def update(self, repo, q, timeout=60):
        st, b = self.req("POST", f"/repositories/{repo}/statements", urllib.parse.urlencode({"update": q}).encode(), "application/x-www-form-urlencoded", timeout=timeout)
        return st, b

    def query(self, repo, q, accept="application/sparql-results+json", timeout=60):
        st, b = self.req("POST", f"/repositories/{repo}", urllib.parse.urlencode({"query": q}).encode(), "application/x-www-form-urlencoded", accept, timeout=timeout)
        if st != 200:
            raise RuntimeError(f"HTTP {st}: {b[:500].decode(errors='replace')}")
        return json.loads(b) if "json" in accept else b

    def size(self, repo):
        return int(self.req("GET", f"/repositories/{repo}/size")[1])


class Report:
    def __init__(self, name):
        self.name, self.failures, self.checks = name, [], 0
        print(f"\n=== {name} ===", flush=True)

    def check(self, ok, what):
        self.checks += 1
        if not ok:
            self.failures.append(what)
            print(f"  FAIL {what}", flush=True)
        return ok

    def done(self):
        status = "PASS" if not self.failures else f"FAIL ({len(self.failures)})"
        print(f"--- {self.name}: {status}, {self.checks} checks", flush=True)
        sys.exit(1 if self.failures else 0)
