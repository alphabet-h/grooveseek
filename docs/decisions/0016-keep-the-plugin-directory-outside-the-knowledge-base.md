# 16. Keep the plugin directory outside the knowledge base

- Status: accepted
- Date: 2026-09-05
- Deciders: project owner
- Applies to: v1.6.0

## Context and problem

[ADR-0013](0013-compile-in-one-grammar-and-load-the-rest.md) defines a grammar plugin as
native code where **loading it is already executing it**, and draws the consequence that
the directory plugins are read from is a privileged setting. The implementation honours
that consequence — but only for the question of **who may write the value**. A config file
found next to the knowledge base has its `grammar_dir` replaced with a safe default (R4),
and v1.3.0 extended the same rule one step upstream to `[parsers].enabled` (R5).

What was never checked is **what the value may point at**. `grammar_dir` could name a
directory inside the knowledge base, with no check and no warning.

Meanwhile the knowledge-base directory has been declared, consistently since
[ADR-0003](0003-kb-mcpignore-bounds-indexing-not-access.md), to be **not a security
boundary**. The `crate::links` module documentation says it, and so does the README. It is
a statement that whoever can write to the knowledge base decides what gets indexed, and
that this is by design.

Where the two meet there was no check. With the plugin directory inside the knowledge
base, **anyone who can write to the knowledge base can run arbitrary code in the groove
process** by dropping a library there. It is the one place where "index this folder" turns
into "run this code".

## Decision drivers

- The danger holds **regardless of where the config file was found**. Whether a trusted
  config names the directory or an environment variable points at it, a plugin directory
  inside the knowledge base means the same thing.
- ADR-0013 already takes the position that "an enabled language that cannot be resolved
  stops the run. **This holds whether or not the config is trusted**, because otherwise the
  same physical situation would carry two meanings depending on where a file was found."
- A relative `GROOVE_GRAMMAR_DIR` is already refused outright, even as trusted input,
  because "a relative value resolves against the working directory, which may be a
  directory you do not control; a grammar plugin is native code and is loaded from it
  without verification." The same sentence applies to a directory inside the knowledge
  base.
- **A guard must not claim more than it does** (a decision driver from ADR-0003).

## Options considered

1. **Warn and load anyway.**
2. **Substitute the safe default and warn** — the shape R4 uses for untrusted configs.
3. **Refuse and stop the run.**

## Decision

Take **option 3**. When the resolved `grammar_dir` is inside `kb_path` — including when it
is `kb_path` itself — the run stops, whatever the trust of the config.

**Option 1 is not taken** because the warning would be a claim and nothing more. What is
needed here is stopping before the load, not explaining after it. The problem ADR-0003
names — a guard claiming more than it does — reappears here with its direction reversed.

**Option 2 is not taken** because the justification for substituting does not extend to a
trusted setting. R4 may discard an untrusted config's value precisely because **whoever
wrote it holds no authority**. Silently redirecting a value the operator wrote themselves
leaves that operator with no way to learn why the plugin they installed is never read.

The judgement is made **after** `grammar_dir` is resolved. Environment variable, config
file and OS default all carry the same danger, so the inputs are not judged separately —
separating them would give one physical situation several meanings.

The refusal **names the input that has to change**. Telling someone who set the
environment variable to edit the config file is advice that does not work when followed,
since the variable wins either way.

## Consequences

- **Keeping grammar plugins inside the knowledge base is no longer possible.** Every
  installation instruction points at the OS local-data directory; no document describes
  placing them inside the knowledge base, so no supported workflow depends on it.
- **The check needs the effective `kb_path`.** `--kb-path` on the command line overrides
  the config file, so comparing against the configured value would compare against a
  directory nobody indexes. The knowledge base is now passed in as an argument. The Rust
  API sits outside the stable surface by
  [ADR-0008](0008-declare-what-1-0-freezes.md), so this is not a change to the contract.
- **A knowledge base that needs no plugin is unaffected.** The check runs only when an
  enabled id actually requires one; a Markdown-only knowledge base is never asked where
  plugins live.
- **A directory that does not exist is not refused.** Not having installed a plugin yet is
  a normal state, and the existing diagnostic already covers a directory that cannot be
  found. Refusing here would stop users who are in no danger.

## References

- [ADR-0013](0013-compile-in-one-grammar-and-load-the-rest.md) made the plugin directory a
  privileged setting. This decision widens that consequence from "who may write it" to
  "what it may point at", and does not overturn it.
- [ADR-0003](0003-kb-mcpignore-bounds-indexing-not-access.md) made the knowledge base a
  boundary for indexing and stated that it is not a security boundary.
