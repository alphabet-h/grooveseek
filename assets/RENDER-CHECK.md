# Render check (temporary, deleted before merge)

The README references its images by absolute URL pinned to `main`, which does
not carry `assets/` until this branch merges — so the README preview on this
branch cannot show whether the mechanism works. This file pins the same markup
to a commit SHA on this branch so it can be checked on GitHub before merging.

The open question it answers: `raw.githubusercontent.com` is reported to serve
`.svg` as `text/plain`, which would stop an `<img>` from rendering it. If the
logo below is blank, the README must use PNG instead.

## Logo, via `<picture>` (this is the README's markup, SHA-pinned)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://github.com/alphabet-h/grooveseek/raw/977cb22870fbd99755d6bf4f929241c6fd2a840a/assets/logo-dark.svg">
  <img src="https://github.com/alphabet-h/grooveseek/raw/977cb22870fbd99755d6bf4f929241c6fd2a840a/assets/logo-light.svg" alt="" width="56" height="56">
</picture>

## Screenshot, via `<picture>`

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://github.com/alphabet-h/grooveseek/raw/977cb22870fbd99755d6bf4f929241c6fd2a840a/assets/screenshot-dark.png">
  <img src="https://github.com/alphabet-h/grooveseek/raw/977cb22870fbd99755d6bf4f929241c6fd2a840a/assets/screenshot-light.png" width="880" alt="operator view">
</picture>

## Badges

[![CI](https://img.shields.io/github/actions/workflow/status/alphabet-h/grooveseek/ci.yml?branch=main&label=CI)](https://github.com/alphabet-h/grooveseek/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/alphabet-h/grooveseek?label=release)](https://github.com/alphabet-h/grooveseek/releases/latest)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
