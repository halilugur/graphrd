"""Cluster under failure: three voting members and a learner (read replica), a client writing
all along, the leader killed with SIGKILL in the middle, and brought back after the log was
compacted, so it catches up from a snapshot. Every acknowledged write must be on every node,
the learner included, and no write may be refused by the cluster (a client only skips a node it
cannot connect to, as a load balancer would).

    python3 tests/prod/cluster2.py
"""
import json, os, socket, sys, tempfile, threading, time, urllib.error, urllib.request

sys.path.insert(0, os.path.dirname(__file__))
from common import Server  # noqa: E402

checks = 0


def check(cond, what):
    global checks
    checks += 1
    if not cond:
        print(f"  FAIL {what}")
        sys.exit(1)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def get(url, timeout=2):
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return r.read().decode()


def main():
    print("=== cluster: failover, learner, snapshot catch-up")
    os.environ["GRAPHRD_RAFT_COMPACT"] = "50"
    work = tempfile.mkdtemp(prefix="graphrd-cluster2-")
    ids = ["n1", "n2", "n3", "n4"]
    ports = {n: free_port() for n in ids}
    voters = ",".join(f"{n}=127.0.0.1:{ports[n]}" for n in ids[:3])
    learners = f"n4=127.0.0.1:{ports['n4']}"
    nodes = {n: Server(data=os.path.join(work, n), port=ports[n],
                       args=["--cluster", voters, "--cluster-learners", learners, "--node-id", n, "--cluster-secret", "s3cret"]) for n in ids}
    for s in nodes.values():
        s.start()

    def status(n):
        return json.loads(get(f"{nodes[n].url}/rest/cluster/status"))

    def leader():
        for n in ids[:3]:
            try:
                if status(n)["role"] == "leader":
                    return n
            except Exception:
                pass
        return None

    deadline = time.time() + 10
    while leader() is None and time.time() < deadline:
        time.sleep(0.1)
    first = leader()
    check(first is not None, "a leader is elected")
    check(status("n4")["role"] == "learner", "n4 is a learner")
    req = urllib.request.Request(f"{nodes['n2'].url}/rest/repositories", data=b'{"id":"w","params":{"ruleset":{"value":"empty"}}}', headers={"Content-Type": "application/json"})
    check(urllib.request.urlopen(req).status in (200, 201), "create a repository through a follower")

    alive = set(ids)
    acked, refused, gaps = [], [], []
    stop = threading.Event()

    def writer():
        k, last_ok = 0, time.time()
        order = ["n2", "n3", "n4", "n1"]
        while not stop.is_set():
            body = f"update=INSERT DATA {{ <http://e/w{k}> <http://e/p> {k} }}".encode()
            for n in order:
                if n not in alive:
                    continue
                try:
                    r = urllib.request.urlopen(urllib.request.Request(f"{nodes[n].url}/repositories/w/statements", data=body, headers={"Content-Type": "application/x-www-form-urlencoded"}), timeout=15)
                    if r.status == 204:
                        acked.append(k)
                        gaps.append(time.time() - last_ok)
                        last_ok = time.time()
                    break
                except urllib.error.HTTPError as e:
                    refused.append((k, n, e.code, e.read()[:120]))
                    break
                except Exception:
                    continue  # cannot connect: the next node
            k += 1
            order = order[1:] + order[:1]

    t = threading.Thread(target=writer)
    t.start()
    time.sleep(2)
    print(f"  killing the leader {first} after {len(acked)} writes")
    alive.discard(first)
    nodes[first].kill9()
    time.sleep(3)
    second = leader()
    check(second is not None and second != first, f"a new leader after the kill: {second}")
    time.sleep(2)  # more writes, past the compaction threshold
    print(f"  restarting {first} after {len(acked)} writes")
    nodes[first].start()
    alive.add(first)
    time.sleep(2)
    stop.set()
    t.join()
    check(not refused, f"no write refused: {refused[:3]}")
    check(len(acked) > 150, f"enough writes: {len(acked)}")
    print(f"  {len(acked)} writes acknowledged; longest pause between acknowledgements {max(gaps):.2f} s")
    expected = str(len(set(acked)))
    for n in ids:
        got = None
        for _ in range(100):
            got = get(f"{nodes[n].url}/repositories/w/size")
            if got == expected:
                break
            time.sleep(0.1)
        check(got == expected, f"{n} has {got} statements, {expected} acknowledged")
    st = status(leader())
    check(st["snapshotIndex"] > 0, f"the log was compacted: {st['snapshotIndex']}")
    log = open(nodes[first].log_path, errors="replace").read()
    check("installed a snapshot" in log, f"{first} caught up from a snapshot")
    check(all(p.get("lastContactMs") is not None for p in st["peers"]), "the leader hears from every member")
    for s in nodes.values():
        s.cleanup()
    print(f"--- cluster failover, learner and snapshots: PASS, {checks} checks")


if __name__ == "__main__":
    main()
