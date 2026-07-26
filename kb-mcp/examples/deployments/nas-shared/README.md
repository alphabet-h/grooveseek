# Deployment recipe — NAS-shared knowledge base

> **日本語版**: [README.ja.md](./README.ja.md)

The KB and the SQLite index live on a NAS (NFS / SMB / CIFS). One machine
acts as the **authoritative indexer**; other machines mount the same share
and run kb-mcp serve locally against the mounted path, searching only.
Search-only is an operating rule here, not a mount flag — the mount has to
stay writable (see the client setup below).

> **⚠️ Read this first.** SQLite over a network filesystem is supported
> only with extreme care. The lock primitives that SQLite WAL relies on
> (`fcntl` byte-range locks) are unreliable on SMB and unimplementable
> on most NFS configurations. The recipe below works around this by
> designating one machine the sole writer; **multi-host concurrent
> writes to the same `.kb-mcp.db` will corrupt the index**.

## Target environment

- KB lives on a NAS exported to multiple workstations.
- One workstation (the "indexer host") owns the write path: it runs
  `kb-mcp index` (or has the watcher), and it is the only one that
  touches `.kb-mcp.db` for writes.
- Other workstations mount the same share and run `kb-mcp serve`
  locally to **search only** (never index) against the indexer-produced DB.
- Everyone is on the same LAN (latency matters; cross-WAN NFS will be
  miserable for SQLite).

## What's in this directory

| File | Purpose |
| --- | --- |
| [`kb-mcp.toml.indexer`](./kb-mcp.toml.indexer) | Used **only** on the indexer host. Watcher off (NFS/SMB inotify is unreliable); index runs on a cron / systemd timer. |
| [`kb-mcp.toml.client`](./kb-mcp.toml.client) | Used on every other workstation. Watcher off (no writes), reranker / quality filter to taste. |
| [`.mcp.json`](./.mcp.json) | Client-side: stdio + the shared kb_path. |

## Setup

### Indexer host (one machine)

1. Mount the NAS share with **read+write** for the user that runs kb-mcp.
   Pick the most reliable protocol you have available:
   - NFSv4 with `noac` is generally the safest for SQLite.
   - SMB with `cache=strict,actimeo=0` works on Linux clients but is
     fragile under heavy concurrency.
2. Copy `kb-mcp.toml.indexer` to `kb-mcp.toml` somewhere on the discovery
   path. Edit `kb_path` to the mounted directory.
3. Run an initial full index (this can take minutes — embedding generation
   over an NFS read can be slower than local disk):

   ```bash
   kb-mcp index --kb-path /mnt/nas/knowledge-base --force
   ```

4. Schedule incremental rebuilds on a timer (cron / systemd). Example
   systemd timer fragment:

   ```ini
   # /etc/systemd/system/kb-mcp-index.service
   [Service]
   Type=oneshot
   ExecStart=/usr/local/bin/kb-mcp index --kb-path /mnt/nas/knowledge-base
   User=kbmcp

   # /etc/systemd/system/kb-mcp-index.timer
   [Timer]
   OnBootSec=2min
   OnUnitActiveSec=5min  # adjust to your edit cadence
   Unit=kb-mcp-index.service

   [Install]
   WantedBy=timers.target
   ```

5. **Do not** run `kb-mcp serve` on the indexer host with the watcher
   enabled if other machines are reading the DB. The watcher's
   incremental writes can race with reader connections opening the WAL
   on another host. Either:
   - Run only the timer-driven `kb-mcp index`, no serve.
   - Or run `serve` with `--no-watch` and rely on the timer.

### Search-only clients (every other machine)

1. Mount the same NAS share. **The directory holding `.kb-mcp.db` must be
   writable by the client**, so mount read-write:

   ```bash
   # Linux NFSv4 example
   sudo mount -t nfs4 -o noac nas:/exports/kb /mnt/nas/knowledge-base
   ```

   A read-only mount does not work, however appealing it sounds. kb-mcp
   opens the database read-write and runs `PRAGMA journal_mode = WAL` plus
   `CREATE TABLE IF NOT EXISTS` on every start, and a WAL database cannot
   even be *read* unless SQLite can create the `-shm` / `-wal` sidecar
   files next to it. Measured with the directory made non-writable, plain
   `kb-mcp status` fails before printing anything:

   ```
   Error: unable to open database file
   Caused by:
       Error code 14: unable to open database file
   ```

   What keeps clients from corrupting the index is therefore **not** the
   mount flags but the discipline below: no `kb-mcp index` anywhere except
   the indexer host, and watcher off everywhere (`kb-mcp.toml.client` ships
   with `[watch].enabled = false`). Searching never writes rows; the writes
   kb-mcp does here are schema-level and idempotent.
2. Copy `kb-mcp.toml.client` to `kb-mcp.toml` on the discovery path,
   edit `kb_path` to the mounted directory.
3. Drop `.mcp.json` into the project root (or wherever Claude Code
   reads it).
4. Confirm the client can read the index:

   ```bash
   kb-mcp status --kb-path /mnt/nas/knowledge-base
   ```

   Should report a non-zero document count. If it reports `unable to open
   database file`, the mount is read-only (see step 1).

## Operational notes

- **The DB lives on the NAS too**. `.kb-mcp.db` lands at
  `/mnt/nas/.kb-mcp.db` (parent of `kb_path`). All hosts see the same
  file, and each opens it read-write (WAL needs that even to read — see
  the client setup). Searching issues no row writes, so concurrent
  searches are safe; concurrent *indexing* from multiple hosts is not.
- **Watcher is OFF for clients**. inotify (Linux) and ReadDirectoryChangesW
  (Windows) do not propagate over network filesystems. The watcher would
  silently miss events anyway. Trust the indexer's timer.
- **Watcher on the indexer is also OFF** in the recipe above — change
  detection runs on a timer because incremental writes mid-search from
  remote readers is a stress case the SQLite WAL is not designed for
  over network FS. If your edit volume is low and your NAS is fast,
  you can experiment with `[watch].enabled = true` on the indexer
  alone — quiesce remote readers when re-indexing.
- **First-run model download** still happens on every machine
  independently. Set `FASTEMBED_CACHE_DIR` to a shared path **only**
  if it lives on local disk for that machine; pointing it at the NAS
  produces extremely slow first-load and serializes embedder loads
  across hosts.
- **`alwaysLoad: true`** in the example `.mcp.json` is a Claude Code
  v2.1.121+ option that forces kb-mcp's tools to be present at initial
  load. Useful for RAG ("search anytime"). With NAS-mounted KBs the
  first-startup cost can be larger than personal (slow disk + initial
  model download), so consider dropping it if startup latency matters
  more than tool-availability. Other MCP clients ignore the field.

## When to step up to another recipe

- The "one writer, many readers" assumption breaks (everyone wants to
  edit and reindex on demand) → [`intranet-http/`](../intranet-http/),
  where one HTTP server holds the only writer connection and clients
  query over the network.
- You see `database is locked` / `database disk image is malformed`
  errors → the network FS lock semantics have failed. Move to
  [`intranet-http/`](../intranet-http/) before you lose data.
