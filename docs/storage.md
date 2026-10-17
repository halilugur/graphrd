# Storage

This page describes how a repository keeps its data: the files, the indexes, the dictionary,
durability and concurrency.

## The data directory

```
graphrd-data/
  repositories/
    zoo/
      config.json           repository configuration (ruleset, FTS, SHACL, …)
      namespaces.json       prefixes
      storage/
        MANIFEST            which run files are live, dictionary size, statement counts
        wal.log             write-ahead log
        history.log         change history for time travel, with commit times (historyRetention)
        terms.dat           dictionary: encoded terms, appended
        terms.idx           dictionary: offset of each term id
        terms.hash          dictionary: open-addressing hash table, term -> id
        spog-00000012.run   index runs (spog, posg, ospg, gspo, fts, geo)
        …
  users.json                users (with --auth)
  saved-queries.json
  cluster/                  Raft term, vote and log (cluster nodes)
  import/                   files offered for server-side import
```

A data directory is locked by the process that uses it (`.lock`), so a server and an offline tool
cannot open it at the same time.

## Dictionary

Every distinct term (IRI, blank node, literal, triple term) gets a 32-bit id. Statements are stored
as four ids `[subject, predicate, object, graph]`; graph id 0 is the default graph, and a reserved
id marks inferred statements. New terms are kept in memory until a checkpoint appends them to
`terms.dat` / `terms.idx` and inserts them into `terms.hash` (rebuilt into a new file when it grows,
so a crash leaves either the old or the new table). Decoded terms and lookups are cached in sharded
caches of 200,000 entries each.

## Indexes

Each repository has four indexes over the same statements, in the orders SPOG, POSG, OSPG and GSPO,
so every triple pattern is a range scan of one of them. The full-text index is a fifth index of the
same kind ([fts.md](fts.md)).

Each index is a log-structured merge tree:

- **Memtable**: new entries go to a sorted in-memory map (key → present or deleted). Readers share
  frozen, immutable sorted levels of it.
- **Runs**: a checkpoint writes the memtable as an immutable run file: a header, fixed-size
  entries (16-byte key plus a flag), a sparse index of the first key of every 256-entry block, and
  a footer. Runs are written to a temporary file, fsynced and renamed.
- **Merging**: runs of similar size are merged (size-tiered), so their number stays logarithmic in
  the data size. `graphrd optimize` merges everything into one run per index.
- **Reads** merge the memtable levels and the runs, newest first. Blocks are read on demand through
  a shared LRU cache of about 16,000 decoded blocks (~70 MB at most).

## Writes and durability

1. A transaction (an update request, an upload, a delete) changes the memtables and appends its
   statements to `wal.log`, followed by a commit marker.
2. The log is fsynced before the client gets its answer (`--sync on`, the default). Concurrent
   writers share one fsync (group commit). The log file is zero-filled ahead of its end so an fsync
   writes data only, not file-size metadata.
3. A checkpoint (when the memtables hold 2 million entries, the log reaches 256 MB, or writes have
   stopped for a few seconds) writes run files, appends new terms to the dictionary, writes a new
   `MANIFEST` atomically, and empties the log.
4. On start, the log is replayed after the last checkpoint. A transaction without its commit marker
   (torn by a crash) is dropped whole.

With `--sync off`, commits are not fsynced: they survive a crash of the process (the log is in the
operating system's cache) but not a power loss. `graphrd load` skips the log altogether and makes
each batch live with one manifest write ([cli.md](cli.md#load)).

A SPARQL update request is all or nothing: its changes are recorded while it runs and undone if any
operation fails or SHACL rejects the result.

## Concurrency: snapshots

Readers never wait for writers, and writers never wait for readers. Each query takes a snapshot of
the repository: the current memtable levels and the list of runs, shared by reference counting. The
writer works on its own memtable head and publishes a new snapshot when its transaction is done.
Writers are serialized among themselves by a lock that is released before the fsync, so the next
writer can proceed while the previous one waits for the disk.

Run files that merging replaced stay readable by older snapshots; they are deleted after the
manifest no longer names them.

## Inferred statements

Inferred statements are stored in the same indexes under the reserved graph id, so queries read
them like explicit ones, and they survive restarts without being derived again
([inference.md](inference.md)).

## Memory

Memory use is bounded by the memtables (checkpointed at 2 million entries), the block cache
(~70 MB) and the dictionary caches, not by the data size. Snapshots share structure, so many
concurrent queries cost little extra. After a burst of writes, the background checkpoint empties
the memtables and returns freed memory to the operating system.

## In-memory repositories

`serve --memory` keeps everything in memory with the same index and query code: no files, no log,
and no checkpoints. The data ends with the process.
