"""Run the curl examples of docs/*.md against a fresh GraphRD server and report those that fail.

    cargo build --release && python3 tests/docs/check_examples.py

Each page runs in order against its own server (with `--auth admin:secret` when the page's examples
sign in). `localhost:7200` is rewritten to the test server; Turtle blocks of a page are saved as the
files its examples upload (animals.ttl, shapes.ttl). Lines that need something this check cannot
provide (another host, a file path on a server, a stopped data directory) are skipped."""
import os, re, shlex, subprocess, sys, tempfile

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "prod"))
from common import Server  # noqa: E402

DOCS = os.path.join(os.path.dirname(__file__), "..", "..", "docs")
SKIP = ("10.0.0.", "/srv/", "graphrd-backup.tar", "/connectors/", "/nl-query", "--llm-", "nlq_eval")  # other hosts, server paths, a search engine, a language model


def blocks(text, lang):
    return re.findall(r"```" + lang + r"\n(.*?)```", text, re.S)


def commands(block):
    """Shell commands of a block, with continuation lines (and quotes spanning lines) joined;
    comments dropped."""
    out, cur = [], ""
    for line in block.splitlines():
        line = line.rstrip()
        if line.endswith("\\"):
            cur += line[:-1] + " "
            continue
        cur += line
        try:
            shlex.split(cur, comments=True)
        except ValueError:
            cur += "\n"  # a quote still open: the command goes on
            continue
        if cur.strip() and not cur.strip().startswith("#"):
            out.append(cur.strip())
        cur = ""
    return out


def run_page(name, text, shared):
    cmds = [c for b in blocks(text, "bash") for c in commands(b) if c.startswith(("curl", "A="))]
    if not cmds:
        return 0, []
    auth = any("-u " in c or "$A" in c for c in cmds)
    server = Server(args=["--auth", "admin:secret"] if auth else []).start()
    work = tempfile.mkdtemp(prefix="graphrd-docs-")
    for t in shared + blocks(text, "turtle"):
        fname = "shapes.ttl" if "sh:" in t else "animals.ttl"
        open(os.path.join(work, fname), "w").write(t)
    # repositories the examples use
    repos = sorted(set(re.findall(r"/repositories/([a-z]+)", " ".join(cmds))) - {"default"})
    for r in repos:
        created = any(f'"id":"{r}"' in c for c in cmds)
        if not created:
            u = ["-u", "admin:secret"] if auth else []
            subprocess.run(["curl", "-s", *u, "-X", "POST", f"{server.url}/rest/repositories", "-H", "Content-Type: application/json",
                            "-d", f'{{"id":"{r}"}}'], capture_output=True)
            if os.path.exists(os.path.join(work, "animals.ttl")):
                subprocess.run(["curl", "-s", *u, "-X", "POST", f"{server.url}/repositories/{r}/statements", "-H", "Content-Type: text/turtle",
                                "--data-binary", "@" + os.path.join(work, "animals.ttl")], capture_output=True)
    env = dict(os.environ, A="admin:secret")
    failures, ran = [], 0
    for c in cmds:
        if any(s in c for s in SKIP) or c.startswith("A="):
            continue
        c = c.replace("localhost:7200", server.url.replace("http://", "")).replace("@animals.ttl", "@" + os.path.join(work, "animals.ttl")).replace("@shapes.ttl", "@" + os.path.join(work, "shapes.ttl"))
        # status code only; the body goes nowhere
        if "$(" in c:
            # a command substitution needs a shell
            argv = ["bash", "-c", "curl -s -o /dev/null -w '%{http_code}'" + c[len("curl"):]]
        else:
            argv = [a.replace("$A", env["A"]) for a in shlex.split(c, comments=True, posix=True)]
            argv[1:1] = ["-s", "-o", "/dev/null", "-w", "%{http_code}"]
        code = subprocess.run(argv, capture_output=True, text=True, env=env).stdout.strip()
        ran += 1
        if not code.startswith("2"):
            failures.append(f"{code}: {c[:140]}")
    server.cleanup()
    return ran, failures


def main():
    total, bad = 0, 0
    # the Turtle of getting-started (animals.ttl) is what the other pages' examples upload
    shared = blocks(open(os.path.join(DOCS, "getting-started.md")).read(), "turtle")
    for name in sorted(os.listdir(DOCS)):
        if not name.endswith(".md"):
            continue
        ran, failures = run_page(name, open(os.path.join(DOCS, name)).read(), shared)
        total += ran
        bad += len(failures)
        if ran:
            print(f"{name}: {ran - len(failures)}/{ran} ok")
        for f in failures:
            print("   FAIL", f)
    print(f"docs examples: {total - bad}/{total} ok")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
