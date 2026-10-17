"""Backups and point-in-time restore, end to end: a server keeps the change history of a
repository; it is backed up (full), written to some more, backed up again (incremental), and the
backup is restored into another data directory as it was at chosen moments.

    python3 tests/prod/pitr.py
"""
import json, os, shutil, subprocess, sys, tempfile, time, urllib.parse

sys.path.insert(0, os.path.dirname(__file__))
from common import BIN, Server  # noqa: E402

checks = 0


def check(cond, what):
    global checks
    checks += 1
    if not cond:
        print(f"  FAIL {what}")
        sys.exit(1)


def update(srv, text):
    st, b = srv.req("POST", "/repositories/r/statements", urllib.parse.urlencode({"update": text}).encode(), "application/x-www-form-urlencoded")
    check(st == 204, f"update: {st} {b[:200]!r}")
    time.sleep(0.02)
    return int(time.time() * 1000)


def names(data):
    out = subprocess.run([BIN, "query", "--data", data, "--repo", "r", "--format", "csv", "SELECT ?n WHERE { ?s <http://e/name> ?n } ORDER BY ?n"], capture_output=True, text=True, check=True).stdout
    return out.split()[1:]


def run(*args):
    r = subprocess.run([BIN, *args], capture_output=True, text=True)
    check(r.returncode == 0, f"graphrd {' '.join(args)}: {r.stderr}")
    return r.stderr


def main():
    print("=== backups and point-in-time restore")
    work = tempfile.mkdtemp(prefix="graphrd-pitr-")
    data, backup = os.path.join(work, "data"), os.path.join(work, "backup")
    srv = Server(data=data).start()
    cfg = {"id": "r", "params": {"historyRetention": {"value": "unlimited"}, "ruleset": {"value": "empty"}}}
    st, _ = srv.req("POST", "/rest/repositories", json.dumps(cfg).encode(), "application/json")
    check(st in (200, 201), "create repository")
    t1 = update(srv, 'INSERT DATA { <http://e/a> <http://e/name> "ann" }')
    t2 = update(srv, 'INSERT DATA { <http://e/b> <http://e/name> "bob" }')
    srv.stop()
    out = run("backup", "--data", data, backup)
    check("full backup" in out, out)
    srv.start()
    t3 = update(srv, 'DELETE DATA { <http://e/a> <http://e/name> "ann" } ; INSERT DATA { <http://e/c> <http://e/name> "cy" }')
    for i in range(200):  # enough to checkpoint and write new run files
        update(srv, f'INSERT DATA {{ <http://e/x{i}> <http://e/p> {i} }}') if i % 50 == 0 else None
    t4 = update(srv, 'INSERT DATA { <http://e/d> <http://e/name> "dee" }')
    srv.stop()
    out = run("backup", "--data", data, "--incremental", backup)
    check("incremental backup" in out, out)
    log = open(os.path.join(backup, "backup.log")).read().split("\n")
    check(len([l for l in log if l]) == 2, f"backup.log: {log}")
    for at, want in [(t1, ["ann"]), (t2, ["ann", "bob"]), (t3, ["bob", "cy"]), (t4, ["bob", "cy", "dee"])]:
        target = os.path.join(work, f"restored-{at}")
        run("restore", "--from", backup, "--data", target, "--to-time", str(at))
        got = names(target)
        check(got == [f'{w}' for w in want], f"state at {at}: {got} != {want}")
        print(f"  restored to {at}: {got}")
    # without --to-time: the latest state; a restored directory serves and keeps its history
    target = os.path.join(work, "restored-latest")
    run("restore", "--from", backup, "--data", target)
    check(names(target) == ["bob", "cy", "dee"], "latest")
    again = os.path.join(work, "restored-again")
    run("restore", "--from", backup, "--data", again, "--to-time", str(t2))
    srv2 = Server(data=again).start()
    st, b = srv2.req("POST", "/repositories/r", urllib.parse.urlencode({"query": "SELECT ?n WHERE { ?s <http://e/name> ?n }", "as-of": str(t1)}).encode(), "application/x-www-form-urlencoded", "text/csv")
    check(st == 200 and b.decode().split()[1:] == ["ann"], f"time travel after a restore: {st} {b!r}")
    srv2.stop()
    check(subprocess.run([BIN, "restore", "--from", backup, "--data", again], capture_output=True).returncode != 0, "restore into a non-empty directory is refused")
    shutil.rmtree(work, ignore_errors=True)
    print(f"--- backups and point-in-time restore: PASS, {checks} checks")


if __name__ == "__main__":
    main()
