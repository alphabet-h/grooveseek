# Deployment recipe — intranet HTTP server

> **日本語版**: [README.ja.md](./README.ja.md)

One server runs `kb-mcp serve --transport http`, holds the only writer
connection to the index, and answers MCP requests from many client
machines on the same intranet over Streamable HTTP.

> **⚠️ Trust boundary.** kb-mcp does not yet authenticate clients. Bind
> only to interfaces reachable from your trusted intranet, and assume
> that anyone who can reach the port can read the entire knowledge base.
> See "Security model" below.

## Target environment

- A team / household / lab with a single shared knowledge base.
- One Linux server (bare metal, VM, or NAS appliance with shell access)
  with reasonable disk + CPU. The KB and the SQLite DB live here.
- Multiple client machines on the same LAN run Claude Code / Cursor and
  hit the server over HTTP.
- Optional but recommended: a reverse proxy (nginx / Caddy) in front for
  TLS + access control if you have non-trusted users on the network.

## What's in this directory

| File | Purpose |
| --- | --- |
| [`kb-mcp.toml`](./kb-mcp.toml) | Server-side: HTTP transport, watcher on, kb_path, model |
| [`kb-mcp.service`](./kb-mcp.service) | systemd unit for the server. `User=kbmcp`, restart on failure |
| [`.mcp.json`](./.mcp.json) | **Client-side**: HTTP transport pointing at the server URL |

## Setup

### Server side (one machine)

1. Create a dedicated unix user (recommended): `sudo useradd -r -s /usr/sbin/nologin kbmcp`
2. Place the binary at `/usr/local/bin/kb-mcp` (chmod 755) and the
   knowledge base at e.g. `/srv/kb-mcp/knowledge-base/`. Make `kbmcp`
   the owner of the parent so `.kb-mcp.db` can be written:

   ```bash
   sudo install -d -o kbmcp -g kbmcp /srv/kb-mcp
   sudo cp -r ./knowledge-base /srv/kb-mcp/
   sudo chown -R kbmcp:kbmcp /srv/kb-mcp/
   ```
3. Drop `kb-mcp.toml` from this directory at `/srv/kb-mcp/kb-mcp.toml`
   (CWD discovery — the systemd unit sets `WorkingDirectory=/srv/kb-mcp`).
   Edit `kb_path`, `model`, and `[transport.http].bind` to taste.
4. Create the ONNX cache directory (the systemd unit only declares
   `ReadWritePaths=`, it does not create or chown the dir):

   ```bash
   sudo install -d -o kbmcp -g kbmcp /var/cache/fastembed
   ```
5. Build the initial index (as root or sudo as kbmcp):

   ```bash
   sudo -u kbmcp /usr/local/bin/kb-mcp index \
       --kb-path /srv/kb-mcp/knowledge-base
   ```

   Expect minutes the first time (model download + embedding generation).
6. Install the systemd unit:

   ```bash
   sudo cp kb-mcp.service /etc/systemd/system/kb-mcp.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now kb-mcp.service
   ```
7. Health check:

   ```bash
   curl http://127.0.0.1:3100/healthz   # → 200 OK
   ```
8. Open the firewall to your intranet only. Example UFW:

   ```bash
   sudo ufw allow from 192.168.1.0/24 to any port 3100 proto tcp
   ```

### Client side (every workstation)

1. Confirm the server URL is reachable: `curl http://kb-server.lan:3100/healthz`
2. Drop `.mcp.json` from this directory into your project root or
   `~/.config/claude/.mcp.json`. Edit the URL to match your server's
   address.
3. That's it — no kb-mcp installed on the client necessary, just an
   HTTP-capable MCP client.

## Operational notes

- **Single writer**. `serve` holds the only `Mutex<Database>` for the
  index. The watcher on the server picks up edits to files under
  `kb_path` and re-indexes incrementally; clients never write.
- **Concurrency**. rmcp's Streamable HTTP layer accepts many connections
  in parallel, but `search` calls serialize on the embedder + DB
  mutexes. Throughput is roughly 5-15 qps per kb-mcp instance with
  reranker off, depending on CPU. For higher throughput, vertical-scale
  the server (more CPU, faster disk) — kb-mcp is single-process by
  design.
- **Edits to the KB**. Two ways to keep the index fresh:
  - Edit files directly on the server (e.g. via SSH / the editor on the
    server). The watcher catches the change within ~500 ms.
  - Push edits via `git push` to a bare repo on the server, with a
    post-receive hook that runs `git pull` in `/srv/kb-mcp/knowledge-base`.
    The watcher catches the resulting file changes.
- **Restart safety**. The DB is written with WAL + `synchronous = NORMAL`
  by SQLite defaults. Killing the process mid-index loses at most the
  current chunk's commit — the next `kb-mcp index` rebuilds from
  authoritative source files.

## Security model

kb-mcp has **no built-in authentication**. The Streamable HTTP layer
defaults to `127.0.0.1:3100` precisely to avoid accidental exposure;
binding to `0.0.0.0` is opt-in and your responsibility.

| Threat | Mitigation |
| --- | --- |
| Casual local-network sniff (HTTP unencrypted) | Front kb-mcp with nginx/Caddy doing TLS termination, bind kb-mcp to loopback only |
| Unauthorized clients on the LAN | Reverse proxy with HTTP basic auth or mTLS; or run kb-mcp on a per-team subnet that's already access-controlled |
| Malicious request floods (DoS) | Rate limiting on the proxy. kb-mcp itself has no rate limiter. |
| DNS rebinding from a browser | rmcp validates the Host header (loopback only by default); tightening for non-loopback binds is on the roadmap |

If you need authentication today, the canonical recipe is:

```
[Internet / VPN] → nginx (TLS + basic auth) → 127.0.0.1:3100 → kb-mcp
```

Bind kb-mcp to `127.0.0.1:3100` in `kb-mcp.toml`, configure nginx to
proxy `/mcp` and `/healthz` with `proxy_set_header Host $host` etc.

### `alwaysLoad: true` (client-side)

The example client `.mcp.json` sets `"alwaysLoad": true`. This is a
Claude Code v2.1.121+ option that forces kb-mcp's tools to be present
at initial load instead of going through the tool-search shortlist.
Recommended for RAG (always-available search). Heavy lifting happens
server-side, so client-side startup cost is negligible — safe to keep
enabled for HTTP transport. Other MCP clients (Cursor, etc.) ignore
the field.

## When to step up to another recipe

- Authentication isn't optional → you've already outgrown this recipe;
  put a real reverse proxy with auth in front.
- Multiple geographic locations → the LAN-only assumption breaks; this
  is past kb-mcp's current ops surface. Either replicate the KB with
  rsync-style sync per region, or wait for hosted kb-mcp.
