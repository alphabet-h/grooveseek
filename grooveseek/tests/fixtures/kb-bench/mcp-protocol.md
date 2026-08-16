---
title: Model Context Protocol
topic: mcp
date: 2026-01-03
tags: [mcp, protocol, json-rpc]
---

# Model Context Protocol

MCP is a JSON-RPC 2.0 over stdio (or Streamable HTTP) protocol that lets
LLM clients discover and call tools, read resources, and stream events
from a server.

## Transports

The default transport is stdio. Streamable HTTP adds support for
long-lived sessions over an HTTP request, useful for hosting MCP
servers behind a network boundary.
