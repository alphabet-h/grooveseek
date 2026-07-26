# Deployment recipe — knowledge base on a NAS

> **日本語版**: [README.ja.md](./README.ja.md)

The knowledge base itself lives on a NAS (NFS / SMB / CIFS) so everyone
edits the same files. **The index does not.** Each machine keeps its own
`.kb-mcp.db` on local disk and indexes the shared files itself.

> **⚠️ Why the index stays local.** SQLite's WAL mode is explicit about
> this ([SQLite docs, WAL](https://www.sqlite.org/wal.html)):
>
> > All processes using a database must be on the same host computer; WAL
> > does not work over a network filesystem. This is because WAL requires
> > all processes to share a small amount of memory and processes on
> > separate host machines obviously cannot share memory with each other.
>
> kb-mcp runs every connection in WAL mode, so putting `.kb-mcp.db` on
> the share and opening it from several machines is not a configuration
> that can be made safe with mount flags or an "only one writer" rule —
> even readers participate in the shared-memory protocol. Earlier
> versions of this recipe did exactly that; it is corrected here.
>
> If you want **one** index rather than one per machine, that is what
> [`intranet-http/`](../intranet-http/) is for: a single host owns the
> database on its own disk and answers searches over HTTP.

## Target environment

- The KB is a directory on a NAS, exported to several workstations.
- Each workstation searches its own local index of those files, so
  embedding cost and disk are paid per machine (a few hundred MB for a
  typical KB, plus the ONNX model cache).
- Everyone is on the same LAN. Cross-WAN mounts make indexing slow, but
  correctness no longer depends on the network filesystem's locking.

## What's in this directory

| File | Purpose |
| --- | --- |
| [`kb-mcp.toml.client`](./kb-mcp.toml.client) | The per-machine config. Watcher off (network filesystems do not deliver inotify / ReadDirectoryChangesW events), index driven by a timer. |
| [`kb-mcp.toml.indexer`](./kb-mcp.toml.indexer) | Same thing with the watcher discussion spelled out, for a machine that also edits the KB locally. |
| [`.mcp.json`](./.mcp.json) | Client-side: stdio, pinned to this machine's config with `--config`. |

## Setup (repeat on every machine)

1. **Mount the share so that its parent directory is on local disk.**
   This is the whole trick: `.kb-mcp.db` is created in the *parent* of
   `kb_path`, so mounting at `/var/lib/kb-mcp/knowledge-base` puts the
   database at `/var/lib/kb-mcp/.kb-mcp.db` — local, private, and never
   touched by another host.

   ```bash
   # The parent must be writable by the account that runs kb-mcp, because
   # .kb-mcp.db and its WAL sidecars are created there. The mount point
   # itself has to exist before mounting — `mount` will not create it.
   sudo install -d -o "$(id -un)" -g "$(id -gn)" /var/lib/kb-mcp
   sudo install -d /var/lib/kb-mcp/knowledge-base

   # Linux NFSv4 example. Read-only is fine here: only the KB files are
   # on the NAS, and kb-mcp never writes those.
   sudo mount -t nfs4 -o ro nas:/exports/kb /var/lib/kb-mcp/knowledge-base
   ```

   Mounting read-only is optional but harmless, and it makes "this
   machine does not edit the shared KB" enforceable. Machines that *do*
   edit the KB mount it read-write as usual.

2. Copy `kb-mcp.toml.client` to `/var/lib/kb-mcp/kb-mcp.toml` and set
   `kb_path = "/var/lib/kb-mcp/knowledge-base"`. Keeping it next to the
   database means the timer below can point `--config` straight at it — the
   model must match what the index was built with, and a systemd unit does
   not inherit your shell's working directory.

3. Build the index (minutes on the first run — reading over NFS is
   slower than local disk, and the ONNX model downloads once):

   ```bash
   kb-mcp index --config /var/lib/kb-mcp/kb-mcp.toml
   ```

4. Keep it fresh on a timer. The watcher cannot help here: neither
   inotify nor ReadDirectoryChangesW propagates over a network
   filesystem, so it would silently miss every remote edit.

   These are **user** units, so they run as you with no `User=` line to
   substitute and no root needed — matching the directory you created in
   step 1:

   ```ini
   # ~/.config/systemd/user/kb-mcp-index.service
   [Service]
   Type=oneshot
   # --config is required: a unit does not inherit a working directory, so
   # config discovery would fall back to defaults and index with the wrong
   # model, which the existing index then rejects.
   ExecStart=/usr/local/bin/kb-mcp index --config /var/lib/kb-mcp/kb-mcp.toml

   # ~/.config/systemd/user/kb-mcp-index.timer
   [Timer]
   OnBootSec=2min
   # Adjust to your edit cadence. systemd has no trailing comments: a `#`
   # after the value becomes part of the value and the setting is dropped.
   OnUnitActiveSec=5min

   [Install]
   WantedBy=timers.target
   ```

   ```bash
   systemctl --user daemon-reload
   systemctl --user enable --now kb-mcp-index.timer
   # Optional: keep the timer running while you are logged out
   sudo loginctl enable-linger "$(id -un)"
   ```

   Re-indexing is incremental (SHA-256 content diff), so a timer that
   finds nothing new costs a directory walk and a hash per file.

5. Drop `.mcp.json` into the project root (or wherever your MCP client
   reads it). It passes `--config /var/lib/kb-mcp/kb-mcp.toml` for the same
   reason the timer does: the client launches kb-mcp from *your project*
   directory, where discovery would never find that file, and the server
   would start with the default model against a `bge-m3` index.

6. Confirm:

   ```bash
   kb-mcp status --config /var/lib/kb-mcp/kb-mcp.toml
   ```

   Should report a non-zero document count. If it says `unable to open
   database file`, the *parent* of `kb_path` is not writable — that is
   where the database and its WAL sidecars have to live.

## Operational notes

- **Every machine embeds the same content.** That is the price of not
  sharing a database over the network. A KB of a few thousand documents
  costs minutes of CPU per machine on the first run and seconds per
  timer tick afterwards. If that is too much, run
  [`intranet-http/`](../intranet-http/) instead and pay it once.
- **The index can lag behind the files** by up to one timer interval,
  and different machines can be at different points in that window.
  Nothing breaks — search just returns slightly older content.
- **Model caches are per machine too.** Set `FASTEMBED_CACHE_DIR` only
  to a *local* path; pointing it at the NAS makes model loading slow and
  serializes it across hosts.
- **Config mismatches are caught, not silently tolerated.** If one
  machine indexes with `bge-small-en-v1.5` and another opens that
  database expecting `bge-m3`, the `index_meta` check rejects it at
  startup. Since each machine now owns its database this only matters if
  you copy one around.
- **`alwaysLoad: true`** in the example `.mcp.json` is a Claude Code
  v2.1.121+ option that forces kb-mcp's tools to be present at initial
  load. Useful for RAG ("search anytime"). First-startup cost here can
  be larger than the personal recipe (NFS reads + initial model
  download), so drop it if startup latency matters more. Other MCP
  clients ignore the field.

## When to step up to another recipe

- You do not want to pay embedding cost on every machine, or you want
  one index everyone agrees on → [`intranet-http/`](../intranet-http/).
- You are tempted to put `.kb-mcp.db` back on the share → read the
  SQLite quote at the top again, then go to
  [`intranet-http/`](../intranet-http/).
- Only one machine uses the KB after all → [`personal/`](../personal/).
