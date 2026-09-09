# Security policy

GrooveSeek is maintained by one person on a best-effort basis. There is no
paid support and no service-level agreement, but a vulnerability report is
read before anything else.

## Reporting a vulnerability

Do not open a public issue for a security problem. Use GitHub's private
reporting instead: open the repository's
[Security tab](https://github.com/alphabet-h/grooveseek/security) and press
**Report a vulnerability**, which creates a draft advisory that only the
maintainer can see. Include the version (`groove --version`), the transport
and configuration in use, and steps to reproduce.

You will get a first reply within 48 hours of the report, stating whether the
issue is accepted and, if it is, the expected fix version.

## Supported versions

Only the latest release receives security fixes. Older releases are not
patched; upgrade to the latest release to pick up a fix.

## Scope

In scope: anything that lets a knowledge base, a configuration file, an MCP
client or an HTTP peer do more than the documentation says it can — reading
files outside the knowledge base, executing code, crashing the server with a
crafted document, or escaping the trust boundary described in
[docs/deployment-topologies.md](./docs/deployment-topologies.md).

Out of scope, because they are documented design limits rather than defects:

- `groove serve --transport http` ships no authentication. Binding to a
  non-loopback address requires `--i-know`, and the documentation says to put
  a reverse proxy with authentication in front of it.
- Embedding and reranker models are downloaded from Hugging Face on first use
  and are trusted as published there.
- Known advisories in dependencies are tracked by the nightly `cargo audit`
  job; report one only if you believe it is reachable from GrooveSeek.

## Disclosure

A fix ships in a regular release. The advisory is published after that release
is out, crediting the reporter unless they ask otherwise.
