# Deployment recipe — intranet HTTP server

> **日本語版**: [README.ja.md](./README.ja.md)

One server runs `groove serve --transport http`, holds the only writer
connection to the index, and answers MCP requests from many client
machines on the same intranet over Streamable HTTP.

> **⚠️ Trust boundary.** groove does not yet authenticate clients. Bind
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
| [`groove.toml`](./groove.toml) | Server-side: HTTP transport, watcher on, kb_path, model |
| [`groove.service`](./groove.service) | systemd unit for the server. `User=groove`, restart on failure |
| [`.mcp.json`](./.mcp.json) | **Client-side**: HTTP transport pointing at the server URL |

## Setup

### Server side (one machine)

1. Create a dedicated unix user (recommended): `sudo useradd -r -s /usr/sbin/nologin groove`
2. Place the binary at `/usr/local/bin/groove` (chmod 755) and the
   knowledge base at e.g. `/srv/groove/knowledge-base/`. Make `groove`
   the owner of the parent so `.groove.db` can be written:

   ```bash
   sudo install -d -o groove -g groove /srv/groove
   sudo cp -r ./knowledge-base /srv/groove/
   sudo chown -R groove:groove /srv/groove/
   ```
3. Drop `groove.toml` from this directory at `/srv/groove/groove.toml`.
   Edit `kb_path`, `model`, and `[transport.http].bind` to taste.

   **Every command below names it with `--config`, and that is not a style
   choice.** Naming a config is what makes groove *trust* it. A config groove
   only finds — from the working directory, or from a `.git` ancestor — has
   `fastembed_cache_dir`, `allowed_hosts`, `allowed_origins` and a
   non-loopback `bind` stripped out of it, because a file found that way is
   one anyone who can write to the directory could have put there. Every key
   this recipe depends on is on that list. See
   [Trusted and untrusted config locations](../../../../docs/configuration.md#trusted-and-untrusted-config-locations).

   **If clients connect from other machines, set
   `[transport.http].allowed_hosts` as well.** It defaults to loopback only
   (`localhost`, `127.0.0.1`, `::1`) as a DNS-rebinding defence, so a LAN
   client requesting `http://kb-server.lan:3100/mcp` is answered with 403 no
   matter what `bind` says. List every hostname / address clients put in
   their URL:

   ```toml
   [transport.http]
   bind = "0.0.0.0:3100"
   allowed_hosts = ["kb-server.lan", "192.168.1.10"]
   ```

   groove warns at startup when it binds off-loopback with this key still
   absent. Behind a reverse proxy, list the name clients use for the proxy
   and make the proxy forward it (`proxy_set_header Host $host;`).
4. Create the ONNX cache directory (the systemd unit only declares
   `ReadWritePaths=`, it does not create or chown the dir):

   ```bash
   sudo install -d -o groove -g groove /var/cache/fastembed
   ```
5. Build the initial index (as root or sudo as groove):

   ```bash
   sudo -u groove /usr/local/bin/groove index \
       --config /srv/groove/groove.toml
   ```

   Expect minutes the first time (model download + embedding generation).

   The config is named here for a second reason on top of step 3's: it carries
   `model`, and an index is built with one embedding model and can only be
   read with the same one. Indexing without it would use the built-in default
   and produce an index the server then refuses at startup.
6. Install the systemd unit:

   ```bash
   sudo cp groove.service /etc/systemd/system/groove.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now groove.service
   ```

   > `groove service install` (v0.8.0+) is **not** what this recipe uses.
   > That command registers a *user-level* unit
   > (`~/.config/systemd/user/`), which starts with your login session and
   > runs as you. A shared server wants the opposite: a system unit that
   > boots without anyone logged in, runs as a dedicated `groove` account,
   > and carries the sandboxing directives in `groove.service`. Use
   > `groove service install` for a personal always-on daemon on your own
   > workstation.
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
3. That's it — no groove installed on the client necessary, just an
   HTTP-capable MCP client.

## Operational notes

- **Single writer**. `serve` holds the only `Mutex<Database>` for the
  index. The watcher on the server picks up edits to files under
  `kb_path` and re-indexes incrementally; clients never write.
- **Concurrency**. rmcp's Streamable HTTP layer accepts many connections
  in parallel, but `search` calls serialize on the embedder + DB
  mutexes. Throughput is roughly 5-15 qps per groove instance with
  reranker off, depending on CPU. For higher throughput, vertical-scale
  the server (more CPU, faster disk) — groove is single-process by
  design.
- **Edits to the KB**. Two ways to keep the index fresh:
  - Edit files directly on the server (e.g. via SSH / the editor on the
    server). The watcher catches the change within ~500 ms.
  - Push edits via `git push` to a bare repo on the server, with a
    post-receive hook that runs `git pull` in `/srv/groove/knowledge-base`.
    The watcher catches the resulting file changes.
- **Restart safety**. The DB is written with WAL + `synchronous = NORMAL`
  by SQLite defaults. Killing the process mid-index loses at most the
  current chunk's commit — the next `groove index` rebuilds from
  authoritative source files.

## Security model

groove has **no built-in authentication**. The Streamable HTTP layer
defaults to `127.0.0.1:3100` precisely to avoid accidental exposure;
binding to `0.0.0.0` is opt-in and your responsibility.

| Threat | Mitigation |
| --- | --- |
| Casual local-network sniff (HTTP unencrypted) | Front groove with nginx/Caddy doing TLS termination, bind groove to loopback only |
| Unauthorized clients on the LAN | Reverse proxy with HTTP basic auth or mTLS; or run groove on a per-team subnet that's already access-controlled |
| Malicious request floods (DoS) | Rate limiting on the proxy. groove itself has no rate limiter. |
| DNS rebinding from a browser | Two checks, in this order. The Host header is validated against `[transport.http].allowed_hosts` — loopback only unless you list your own hostnames (v0.5.0+). Then the Origin header against `[transport.http].allowed_origins`, which defaults to the loopback origins for the bind port (v0.27.0+). Requests with no Origin — ordinary MCP clients, `curl` — pass. **A browser reaching you through the proxy sends the public origin, so list it there or every browser-based client is refused.** `/healthz` is exempt from the Host check by default; set `healthz_public = false` (v0.8.0+) to put it behind the same check. |

The web UI (`/ui`) and the admin API (`/api/admin/*`) are **not** reachable
from other machines: they sit behind a separate check that also requires the
peer address to be loopback, so a LAN client gets 403 there even when its Host
header is allow-listed. Use them by SSH-forwarding a port to the server
(`ssh -L 3100:127.0.0.1:3100 kb-server.lan`) rather than by opening them up.

> **A reverse proxy on the same host defeats that check.** groove then sees
> the proxy's loopback address as the peer, and a plain `proxy_pass
> http://127.0.0.1:3100` also sends `Host: 127.0.0.1:3100` — which is in the
> admin allow-list (that list is the loopback aliases, plus the bind address
> when the bind is itself loopback). Both gates pass and those routes are
> served to whoever reached the proxy. Use an **allow-list**: map `/mcp` and
> `/healthz`, nothing else. A block-list is the wrong shape here — `/ui` and
> `/api/admin/*` sit in the same router and report on the knowledge base and the
> daemon, so anything you forget to deny stays exposed. If you publish these
> routes deliberately, the proxy's own authentication becomes the only thing in
> front of your KB, its index rebuilds, and daemon status.

If you need authentication today, the canonical recipe is:

```
[Internet / VPN] → nginx (TLS + basic auth) → 127.0.0.1:3100 → groove
```

Bind groove to `127.0.0.1:3100` in `groove.toml`, and configure nginx to
proxy **`/mcp` and `/healthz` only**, as an allow-list — every other route
(`/ui`, `/api/admin/*`) is loopback-gated and a same-host proxy
would defeat that gate (see the warning above). Forward the client's Host (`proxy_set_header Host $host;`)
and list that name in `[transport.http].allowed_hosts`. If browser-based clients
will connect, list the public origin in `[transport.http].allowed_origins` as
well — the browser sends the proxy's origin, not the server's.

Origin entries are spelled unlike host entries, in two ways worth knowing before
you write the line. A scheme is required, and a missing one stops groove from
starting rather than being ignored: an entry that cannot be parsed is dropped
before matching, and a list whose entries were all dropped leaves validation on
with nothing to match, refusing every browser without a word in the log. And an
entry with no port matches *every* port on that host, so name the port unless
you mean the scheme's default — `https://kb.example.com` means 443, which is
what you want behind the TLS termination above.

### `alwaysLoad: true` (client-side)

The example client `.mcp.json` sets `"alwaysLoad": true`. This is a
Claude Code v2.1.121+ option that forces groove's tools to be present
at initial load instead of going through the tool-search shortlist.
Recommended for RAG (always-available search). Heavy lifting happens
server-side, so client-side startup cost is negligible — safe to keep
enabled for HTTP transport. Other MCP clients (Cursor, etc.) ignore
the field.

## When to step up to another recipe

- Authentication isn't optional → you've already outgrown this recipe;
  put a real reverse proxy with auth in front.
- Multiple geographic locations → the LAN-only assumption breaks; this
  is past groove's current ops surface. Either replicate the KB with
  rsync-style sync per region, or wait for hosted groove.
