# 31. One writer per index, by an operating-system lock

- Status: accepted
- Date: 2026-10-29
- Deciders: project owner
- Applies to: v1.22.0 for `groove index` and the read-only commands (PR #408);
  `groove serve` takes part from a later release

## Context and problem

The watcher reads what is on disk and writes the index while it holds the
in-process mutex, so two writes inside one `groove serve` cannot interleave.
A second **process** can: a `groove index` run while a server watches the same
knowledge base reads a row, and the other process can change that row before the
write lands. [behavior.md](../behavior.md) has said so until now and left the
rule "run one writer at a time" to the user.

The widest window is the prune at the end of a full rebuild. It deletes every
row its walk did not visit, so a row the other process wrote during the run for
a file that is really there is deleted, and no file event comes to put it back:
the document drops out of search silently. The order of harm the fix was held
to:

1. Worst: a document that exists disappears from search without a word.
2. Next: a deleted document keeps its row.
3. Acceptable: a writer waits, or stops with an error that says why.

## Decision drivers

- Turn "one writer at a time" from a rule the user has to keep into a mechanism.
- Close the cross-process windows without reworking every write point, in
  particular the mutex sections the watcher was rebuilt around.
- Stopping and saying so is better than fixing up silently.
- A crash or a kill needs no manual cleanup.
- Works the same on Windows, macOS and Linux; no new dependency.

## Options considered

1. **Two correct writers: look again under `BEGIN IMMEDIATE` at every write
   point.** Scored highest on not losing documents and on completeness, and
   lowest on cost: about five changes, a three-phase rework of the file rename,
   and a copy of the walk's "would it collect this path" rule for the prune,
   which leaves rows that are never pruned once the copy and the walk drift
   apart. The watcher would wait for another process's SQLite lock (up to the
   30 s busy timeout) while holding the mutex that searches also take, and the
   meaning of every transaction would change at once. Rejected.
2. **Optimistic concurrency: a row generation (`row_gen`) kept by a trigger,
   and compare-and-swap writes.** Admits silent loss in one case (a file made
   again with the same content after the walk missed it), has a ghost
   counterexample, does not see metadata-only updates, and depends on reading
   the token before the look at 30-45 places (the option's own estimate),
   where one place in the wrong order passes silently. A schema and a trigger
   are expensive to take back. Rejected; recorded here, with its score and
   weaknesses, as a possible later layer of defence.
3. **A lease row in `index_meta` kept alive by a heartbeat.** A crashed holder
   leaves its row behind, so taking over needs a timeout that can be wrong both
   ways, and claiming the lease is itself a write to the database. Rejected as
   the way to decide who holds the lease. A heartbeat is kept only to tell how
   fresh the display file is (below).
4. **The `fs4` crate.** std has had `File::try_lock` and `File::unlock` since
   Rust 1.89. Rejected: no new dependency.
5. **An exclusive operating-system lock on a file beside the database.**
   Taken. The operating system drops it with the process, crash and kill
   included.

Variants of option 5 that were not taken:

- **A server that could not take the lock takes over once the holder leaves**
  (with a full catch-up). Needs a check that its configuration matches the
  writer's, a full catch-up, and waiting for its watch to register; left to a
  later decision.
- **A read-only server's `rebuild_index` takes the lock once the holder is
  gone.** Its configuration (parsers, knowledge base, excludes, size caps) can
  differ from the writer that built the index, and the prune deletes every row
  its own walk does not collect, which includes rows of files that exist. The
  likeliest caller is an agent acting on the server's notice, not a decision
  by the user.
- **A server takes the lock after the model download.** A `groove index` from
  a hook or a timer during the first download would then turn the server into a
  reader until restart without anyone noticing. Before the download, that run
  exits 3 and says why.
- **Checking whether the holder is gone by taking the lock and releasing it.**
  A server that starts at that instant becomes a reader until restart, and a
  `groove index` at that instant exits 3.

The three designs were scored by three reviewers on five axes, 10 points each
(150 in all):

| Design | Reviewer 1 (loss/ghost/completeness/cost/tests) | Reviewer 2 | Reviewer 3 | Total | Mean |
|---|---|---|---|---|---|
| **single-writer-enforced (taken)** | 8/8/8/6/8 = 38 | 7/8/7/7/8 = 37 | 8/6/7/8/8 = 37 | **112** | 37.3 |
| per-point-immediate (look-under-IMMEDIATE) | 8/7/8/3/7 = 33 | 8/7/8/3/6 = 32 | 9/8/9/2/6 = 34 | 99 | 33.0 |
| optimistic-cas-epoch (OCC, `row_gen`) | 5/6/6/4/7 = 28 | 6/6/8/5/7 = 32 | 7/7/8/4/7 = 33 | 93 | 31.0 |

| Axis (mean) | lease | look-under-IMMEDIATE | OCC |
|---|---|---|---|
| no silent loss | 7.67 | **8.33** | 6.00 |
| no ghost | 7.33 | 7.33 | 6.33 |
| completeness | 7.33 | **8.33** | 7.33 |
| cost and risk | **7.00** | 2.67 | 4.33 |
| testability | **8.00** | 6.33 | 7.00 |

## Decision

**What v1.22.0 ships.** Only `groove index` takes the lock, and only the
read-only commands (`status`, `search`, `graph`, `doctor`, `eval`, `tune`) open
the database with `query_only`. `groove serve` does not take part yet: it opens
the database writable without the lock, as before, so a `groove index` can
still run beside it on the same index and both can write. Where the bullets
and consequences below describe a server, they are the design for the release
in which `groove serve` takes the lock, not the behaviour of v1.22.0.

- **The lock.** `.groove.db.writer-lock` beside the database (the database's
  path plus `.writer-lock`; when `.groove.db` is a symlink, this file and the
  two below are named after its target and sit beside it, because SQLite opens
  the target and a lock per alias would put two writers on one database; the
  whole path is resolved once, when the lease is taken -- a symlink at the file
  and any directory symlink or junction above it -- and the lock, the holder
  json and the holder's open all use that resolved path, so repointing a link
  later cannot move a writer off its lock. A hard link
  that gives one database two names is not handled: SQLite itself calls opening
  a database under two names undefined behaviour, since each name gets its own
  journal and WAL ([How To Corrupt An SQLite Database File, section
  2.6](https://www.sqlite.org/howtocorrupt.html))), 0
  bytes, locked exclusively with std's `File::try_lock` (`flock` on Unix,
  `LockFileEx` on Windows). It is **never deleted**: if it were, a process waiting on the old file and a new process
  creating a new one would each hold a lock, and there would be two writers.
- **The holder's details live in a second file**, `.groove.db.writer.json`
  (pid, role, start time, version, the knowledge base as an absolute path),
  written to `.groove.db.writer.json.tmp-<pid>` and renamed over, and removed
  on release on a best-effort basis. Two files, because on Windows
  `LockFileEx` keeps other handles from reading the locked range (std locks the
  whole file; inferred), so a refused process could not read details stored in
  the lock file. The json is **for display only**: whether to write, wait or
  rebuild is never decided from it, and it can be stale.
- **If the lock call itself fails** (including "unsupported"), the writer
  stops. Nothing writes without the lock.
- **`groove index`** takes the lock before it touches the database, its WAL or
  the embedding model. If another process holds it, the run prints who holds
  it and what to do next on stderr and exits with code `3`. `--wait <SECONDS>`
  retries every 250 ms up to that long (a blocking lock could not honour the
  limit). The advice depends on who holds the lock: a server watching the same
  knowledge base (call its `rebuild_index`), a one-off run (wait for it), a
  writer of another knowledge base that shares the database file (do not use
  its `rebuild_index`), or unknown. The body of the run lives in one function
  that receives the lock, so the writable connection is dropped, with its
  counter flush and checkpoint, before the lock is: the function boundary fixes
  the order, where a `drop(lease)` added to the caller later would quietly
  break it.
- **Readers.** `status`, `search`, `graph`, `doctor`, `eval`, `tune` (and, once
  `groove serve` takes part, a server that is not the writer) open the database
  with `PRAGMA query_only`, so
  an unintended write fails instead of landing. A one-time write a reader needs
  (a schema upgrade, recording the embedding model, the context mode) runs under
  a short lock with the role `migration`, waiting up to 2 s; if the lock is
  held, the reader reads once more before it gives up, since the holder may have
  done the same write meanwhile. A write refused by `query_only` is told apart
  from every other error in one place: the extended code is exactly
  `SQLITE_READONLY` (8) and the connection itself is not read-only.
- **Writable connections take the held lock as proof** (`open_writer`,
  `open_or_replace_corrupt_with`), so a scan of the source can check that the
  binary opens a writer nowhere else. The library's `Database::open` keeps its
  meaning: writable, no lock, one writer is the caller's responsibility.
- **`groove serve` with its watcher on** (from a later release; see the start
  of this section) takes the lock at startup, before the
  model download, and keeps it until shutdown. A server that cannot take it
  stays a reader until it is restarted: no watcher, it refuses the MCP tool
  `rebuild_index`, and it says so on every `search` and `rebuild_index` reply
  (`writer_notice`) and in `/api/admin/status`. A server with `--no-watch`
  takes the lock only for each `rebuild_index` call. At shutdown the watcher is
  not aborted: it stops at a batch boundary, is awaited, the connection goes
  back to `query_only`, and then the lock is released.

## Consequences

The consequences that involve a server holding or waiting for the lock begin
with the release in which `groove serve` takes part; in v1.22.0 a server does
neither.

- **`groove index` has exit code `3`**: another groove process is writing the
  index, or `--wait` ran out. Once `groove serve` takes the lock, a hook or
  timer that runs `groove index` beside a watching server, which used to run
  alongside it, stops with `3`; in v1.22.0 it still runs beside the server.
- **A server that started while another process held the lock does not write
  until it is restarted.** Files deleted meanwhile keep their rows until the
  restart and a rebuild after it.
- **During a watching server's first model download** (BGE-M3 is about
  2.3 GB), a `groove index` on the same index exits `3`.
- **Writers that do not take part are not stopped**: an older binary, a program
  that calls the library's `Database::open`, or `sqlite3` by hand.
- **A hung holder keeps the lock**; only stopping it releases it. The operating
  system drops the lock when the process exits or is killed. The lock file and
  a stale json may stay behind and need no cleanup.
- **The advice follows who holds the lock**: only when the holder is a server
  watching the same knowledge base does the text say it keeps the index current
  and point to its `rebuild_index`. For a one-off run, another knowledge base,
  or an unknown holder it does not. The current holder is read from the json
  again for each message.
- **A writing server whose watcher stopped still accepts its own
  `rebuild_index`**: its configuration is the one that wrote the index before
  the watcher stopped, and refusing would take away the only remedy short of a
  restart. Whether to refuse is left to the same later decision as the takeover.
- **The json carries `heartbeat_at` and `watching`** for a writing server, which
  rewrites them every 5 s; a reader treats a heartbeat older than 15 s as stale.
  The shipped ways to stop a server (the tray's Stop and Restart, which
  terminate the process, and systemd's SIGTERM) skip the cleanup, so the json is
  left behind; without a heartbeat a reader would keep reporting that a server
  that is gone keeps the index current. Both fields are for display only.
- **The one-writer guarantee holds only for a database on a local disk.** On a
  network file system, whether `flock` or `LockFileEx` works depends on the file
  system. A known example: on Linux NFS, `flock` is emulated with `fcntl` locks,
  under which two handles in the same process no longer conflict.
- **Building needs Rust 1.89 or later**, for `File::try_lock`;
  `rust-version = "1.89"` is declared.
- The file names (`.groove.db.writer-lock`, `.groove.db.writer.json`, and its
  temporary file) and exit code `3` are frozen by [stability.md](../stability.md).
  `groove service uninstall --purge` removes `.groove.db` only and leaves the
  lock file.
- **Revisit when** a server must take over from a writer that left, the
  database must work on a network file system, or writers that do not take part
  turn out to be common.
- **Tests hold it**: `grooveseek/src/writer_lease.rs` (the lock, the json, the
  advice), `grooveseek/src/db/lease.rs` (the writer opens, `query_only` and its
  refusals, the one-time writes), `grooveseek/tests/writer_lease_cli.rs`
  (`groove index`, `--wait`, exit 3, a killed holder) and
  `grooveseek/tests/writer_lease_sources.rs` (writers are opened only under the
  lock).

## References

- [behavior.md](../behavior.md), where "run one writer at a time" was the rule;
  [usage.md](../usage.md) for `groove index --wait` and the exit codes.
- Rust std, [`File::try_lock`](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock).
- SQLite, [`PRAGMA query_only`](https://www.sqlite.org/pragma.html#pragma_query_only).
- Japanese version:
  [0031-one-writer-per-index-by-an-os-lock.ja.md](0031-one-writer-per-index-by-an-os-lock.ja.md)
