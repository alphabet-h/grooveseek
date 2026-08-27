# 13. Compile in one grammar and load the rest

- Status: accepted
- Date: 2026-08-27
- Deciders: project owner
- Applies to: v1.2.0 (this decision); the loader and its directory arrive in v1.3.0

## Context and Problem Statement

[ADR-0012](0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.md)
settles how a source file becomes chunks. It needs a grammar to do it, and
grammars are not small: a generated parse table is a large C source file per
language, and a tool that carries every language carries all of them.

groove's value proposition has been one binary you place and run. Nothing
else to install, no runtime, no package manager. A code parser that ships
twenty grammars would break that promise by weight alone; one that ships
none would not parse anything out of the box.

The question this decision answers: which grammars does the binary carry,
and how does a user who wants another language get it?

## Decision Drivers

- The download stays a download. Someone who indexes only prose should not
  pay for languages they will never index.
- Someone who wants a language should be able to get it without building
  groove from source.
- What ships must be reproducible from the release pipeline that already
  exists. A distribution mechanism nobody can rebuild is one that rots.
- A grammar and the tags query written for it have to travel together. A
  query newer than its grammar matches nothing, silently.
- groove does not reach the network. That is a property people rely on, and
  a feature is not a reason to spend it.

## Considered Options

1. **Every grammar compiled in.** Simple, and the size is the problem: an
   editor that took this route found grammars dominating its installed
   footprint. The user who indexes only Markdown pays all of it.
2. **Cargo features, so each user compiles the set they want.** Works only
   for people who build from source. The release pipeline produces one
   artifact per platform and cannot produce a matrix of feature
   combinations, so the published binary would still have to pick one set —
   which is option 1 or option 5 wearing a different hat.
3. **One grammar compiled in, the rest as dynamic libraries groove
   publishes** (chosen).
4. **WebAssembly plugins.** Sandboxed, which is the attractive part. The
   embedded runtime costs several megabytes on its own — paid by everyone,
   including the people who never load a plugin, which is the cost option 1
   was rejected for. It also raises the toolchain floor, and on macOS a
   just-in-time compiler needs entitlements a signed application must
   request.
5. **Nothing compiled in; every language is a plugin.** The same code path
   as option 3 with the feature turned off, so it stays available. As a
   default it means the tool cannot parse a single line of code until the
   user goes and fetches something.
6. **Download a language bundle on first use.** Convenient, and it spends
   the network property for a convenience. The one bundle available is also
   large per platform and does not carry the queries.

## Decision Outcome

Chosen: **option 3**. Rust is compiled in, behind a feature that is on by
default. Every other language is a separate dynamic library, published by
groove's own release pipeline, that the user places in a directory groove
reads.

Rust rather than some other first language for two reasons: it is what
groove is written in, so pointing groove at its own repository works with
nothing extra; and its grammar was measured at just over a megabyte of
binary — small enough to hand to everyone.

The upstream tree-sitter project publishes source archives and WebAssembly
builds, not native libraries, so the libraries are groove's to build and
sign for. They are produced by the same release job as the binaries, from a
crate that depends on the grammar directly — which is what keeps a grammar
and its tags query at versions that were built together.

### Consequences

- **A plugin is native code, and loading one runs it.** The library's own
  initialisation runs before groove can inspect a single symbol, so a
  malformed or hostile file can do anything the groove process can, and no
  check groove performs afterwards changes that. Two things follow: groove
  opens only the file belonging to a language the configuration actually
  asked for, never everything in the directory; and the setup instructions
  tell the user to verify the published checksum before unpacking. Placing
  a library from anywhere other than a groove release is outside what this
  design defends.
- **The directory a plugin is read from becomes a privileged setting.** A
  configuration file found next to a knowledge base is not trusted to point
  at it, for the same reason such a file is not trusted to redirect the
  model cache: it would turn "index this folder" into "run this code". When
  an untrusted configuration is detected, the setting is reset to the safe
  default whether or not the file mentioned it — omitting the key must not
  be a way around the rule.
- **An enabled language that cannot be resolved stops the run**, with a
  message naming what is missing and where it should go, and without
  creating a database. This holds whether or not the configuration was
  trusted: the alternative makes the same physical situation mean two
  different things depending on where a file was found.
- **Adding a language means a groove release**, because groove holds the
  table mapping a language id to its library file name and has to know that
  name before opening anything. Dropping an arbitrary library into the
  directory does nothing.
- The number of files in a release grows by eight per language: an archive
  and a checksum for each supported platform.

## More Information

The grammar contract lives in `crates/groove-grammar-abi`. The chunking
decision this one supports is
[ADR-0012](0012-chunk-code-at-its-definitions-and-fill-the-gaps-by-line.md).

**This decision ships before its mechanism.** v1.2.0 carries the compiled-in
Rust grammar and the shared contract; the loader, the directory setting and
the published libraries arrive in v1.3.0. A reader of v1.2.0 who wants to
place a plugin will not find anywhere to put it yet.
