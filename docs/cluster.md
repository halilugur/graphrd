# Cluster

Several GraphRD servers form a cluster with the Raft consensus protocol. Every node holds all the
data; writes are replicated to every node in the same order; the cluster keeps working while a
majority of its nodes is up (2 of 3, 3 of 5).

## Starting a cluster

Each node gets the full member list, its own id and a shared secret:

```bash
MEMBERS="n1=10.0.0.1:7200,n2=10.0.0.2:7200,n3=10.0.0.3:7200"
graphrd serve --host 0.0.0.0 --data /var/lib/graphrd --cluster "$MEMBERS" --node-id n1 --cluster-secret "$SECRET"   # on 10.0.0.1
graphrd serve --host 0.0.0.0 --data /var/lib/graphrd --cluster "$MEMBERS" --node-id n2 --cluster-secret "$SECRET"   # on 10.0.0.2
graphrd serve --host 0.0.0.0 --data /var/lib/graphrd --cluster "$MEMBERS" --node-id n3 --cluster-secret "$SECRET"   # on 10.0.0.3
```

`--cluster-secret` can come from `GRAPHRD_CLUSTER_SECRET` instead. Nodes talk to each other over
the same HTTP port (`/cluster/raft/vote`, `/cluster/raft/append`), authenticated by the secret. A
cluster cannot run with `--memory`.

## Reads and writes

- **Writes** (updates, uploads, imports, repository changes) can be sent to any node. A follower
  forwards them to the leader. The leader appends the change to its log, replicates it, and answers
  once a majority has stored it; every node then applies it. If a majority cannot be reached, the
  write is undone and answered with `503`.
- **Reads** are answered by the node that receives them, from its own copy, which may lag the
  leader by a moment. A read with the header `X-GraphRD-Consistency: leader` is forwarded to the
  leader and sees every acknowledged write.

```bash
curl -H 'X-GraphRD-Consistency: leader' -G http://10.0.0.2:7200/repositories/zoo \
  --data-urlencode 'query=SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }'
```

Put the nodes behind a load balancer for reads; any node accepts writes.

## Read replicas (learners)

`--cluster-learners id=host:port,...` adds members that receive every change but do not vote and
do not count for a majority: read replicas. Give every node the same `--cluster` and
`--cluster-learners` lists; a learner starts with its own id in the learner list. Learners answer
reads from their copy and forward writes to the leader like followers; they never become leader,
and adding or losing them never affects whether the cluster can commit.

```bash
graphrd serve --data /var/lib/graphrd --cluster "$MEMBERS" --cluster-learners "r1=10.0.0.9:7200" \
  --node-id r1 --cluster-secret "$SECRET"
```

## Log compaction and snapshots

Applied entries are folded away once 10,000 of them have accumulated: the data holds their effect,
the log keeps only what follows, and a restart does not apply them again. A member that needs
entries the leader has compacted (one that was down for long, or a new member) receives a snapshot
instead: a backup archive of every repository, which replaces its repositories, after which it
continues from the log. `GRAPHRD_RAFT_COMPACT=<n>` changes the threshold.

## Status and failover

`GET /rest/cluster/status` shows the node's role (`leader`, `follower`, `candidate`, `learner`),
term, leader, log length, compaction index (`snapshotIndex`), commit and applied indexes and peers;
on the leader (or with `X-GraphRD-Consistency: leader`) each peer also has the index it has
replicated to and when it last answered. The UI's **Cluster** page draws a map of the members (the
leader in the middle, links coloured by health and labelled with how far behind each member is,
learners dashed), refreshed every two seconds; `/metrics` has `graphrd_cluster_*` gauges.

When the leader stops, the others elect a new one within about a second, and writes continue
through it. A follower that receives a write while the leader is being replaced holds it until the
new leader is known (up to five seconds) instead of refusing it, so clients see a short pause, not
an error. A node that was down catches up from the leader's log when it returns. The term, vote
and log are stored in `DATA/cluster/`.

`bench/cluster-test.sh` runs a three-node cluster through election, forwarded writes, replication,
leader failover and catch-up after a restart. `tests/prod/cluster2.py` runs three voters and a
learner with a client writing all along: the leader is killed with SIGKILL and later restarted after
the log was compacted. About 700 writes are acknowledged, none refused, the longest pause is under
a second, the restarted node catches up from a snapshot, and every node, the learner included,
ends with exactly the acknowledged statements.

## Limits

- Membership is fixed by `--cluster`; adding or replacing a node means restarting the members with
  the new list.
- Every node holds all the data (no sharding).
