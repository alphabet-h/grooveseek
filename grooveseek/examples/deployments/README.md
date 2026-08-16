# Deployment recipes

Three opinionated deployment patterns for groove. Each subdirectory ships
ready-to-adapt `groove.toml` and `.mcp.json` files plus a short README.
Pick the one closest to your situation, copy the files into the target
machine, and adjust paths.

> **日本語版**: [README.ja.md](./README.ja.md)

| Scenario | Best for | Transport | Indexer machines |
| --- | --- | --- | --- |
| [`personal/`](./personal/) | Single user, single Claude Code session at a time | stdio | 1 (this machine) |
| [`nas-shared/`](./nas-shared/) | KB files on a NAS, each machine indexing them locally | stdio (each machine) | every machine |
| [`intranet-http/`](./intranet-http/) | Team server, multiple users at once | Streamable HTTP | 1 (the server) |

For **single-user personal-http** (= 1 machine, 1 user, 1 daemon, loopback only — multiple parallel Claude Code sessions on the same host), the prior `personal-http/` recipe was removed in v0.8.0. Use the built-in service installer instead:

```bash
groove service install --kb-path /path/to/your/kb
```

It self-registers an OS service (Linux systemd-user / macOS LaunchAgent / Windows Task Scheduler AT_LOGON) without needing manual template editing. Run `groove service --help` for full flag reference.

## Selection guide

```
Are you the only person using this KB?
├── Yes → personal flavors
│   ├── Only one Claude Code session at a time? → personal/  (stdio, no daemon)
│   └── Multiple Claude Code sessions in parallel on the same machine?
│       → groove service install  (built-in OS service registration, v0.8.0+)
│
└── No
    ├── Each user keeps their own copy of the KB? → personal/ on every machine
    │
    └── Single source of truth (KB lives on a NAS or shared host)
        ├── All clients on the same LAN as the host that can run groove serve?
        │   └── Yes → intranet-http/  (one server, many clients)
        │
        └── Clients want stdio simplicity (no groove serve process to manage)?
            └── nas-shared/  (share the KB files; each machine keeps its
                             own index — SQLite WAL cannot span hosts)
```

## Common notes

- **Embedding model cache**: First run downloads the ONNX model (BGE-small ~130 MB or BGE-M3 ~2.3 GB) per machine. Set the `fastembed_cache_dir` key in `groove.toml` to share it across all groove invocations on a given machine — the key is lower-case, and unknown keys are rejected at startup, so the environment-variable spelling `FASTEMBED_CACHE_DIR` will not work in the file. (That spelling *is* correct as a real environment variable, which overrides the file.) See each scenario's config: `personal/groove.toml`, `intranet-http/groove.toml`, and for nas-shared the two variants `nas-shared/groove.toml.client` / `nas-shared/groove.toml.indexer`.
- **Index location**: `.groove.db` is always created in the **parent of `kb_path`** (e.g. `kb_path = /srv/kb/notes` → DB at `/srv/kb/.groove.db`). There is no CLI flag to relocate the DB. Plan disk layout with this in mind.
- **Backup policy**: The DB can be rebuilt at any time via `groove index --force --kb-path <kb_path>`. Treat the source files as authoritative; the DB is a derived artifact.

## What's not here

- **Public-internet hosting** — groove has no built-in authentication. Anything beyond an intranet needs a reverse proxy with auth + TLS terminator in front.
- **Container / Kubernetes manifests** — feasible but not yet packaged. Reuse the `intranet-http/` recipe inside a container. Size it from the release assets rather than from the download: the published tarballs are ~9–11 MB compressed, while the extracted binary — what an image layer actually carries — is several times that, because the ONNX runtime is linked in statically. The ONNX model cache is downloaded at runtime and is larger still (~130 MB for BGE-small, ~2.3 GB for BGE-M3), so mount it as a volume rather than baking it into the image.
- **High availability** — groove is single-process; index updates serialize through one `Mutex<Database>`. Run a single instance per index.
