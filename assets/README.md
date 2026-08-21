# assets

| File | What it is |
| --- | --- |
| `logo-light.svg` / `logo-dark.svg` | The mark, and the editable source of truth. Lines of a document with the `◆` that also sits beside the `/ui` heading, marking the passage a search found. Colours are the accent and muted tokens defined in `grooveseek/src/transport/webui_index.html`. |
| `logo-light.png` / `logo-dark.png` | 112×112 renders of those SVGs, at 2× the 56px they used to display at. |
| `screenshot-light.png` / `screenshot-dark.png` | The operator view at `/ui`, 980×860, one per color scheme. |
| `grooveseek-readme-hero-{light,dark}-v2.webp` | The banner, 1942×809. **These are what all four front pages reference** — `README.md`, `README.ja.md`, `docs/index.md`, `docs/index.ja.md`. |
| `grooveseek-readme-hero-{light,dark}-v2.png` | The same image as PNG, kept as the fallback described below. |

## No page embeds the logo any more

The READMEs and both `docs/index` pages opened with the mark at 56 pixels; all
four now open with the banner, which carries the mark at its centre. `/ui` never
used these files — it draws the diamond as a character coloured by the accent
token, and its favicon is an inline `data:` URI.

They are kept anyway, and not because something might break. **The SVGs are
where the mark is defined**, and that is what made it possible to measure the
banner against it — see the colour table below. The PNGs are the renders to
hand anyone who needs the mark outside this repository.

## The hero banner

It carries no words. An earlier draft had a wordmark, a tagline, and a strip of
five captions; all of it was removed, because the wordmark repeated the heading
immediately below it and a caption set at this width is illegible on a phone.
One of those captions read "Local-First & Private", which
[`docs/clients.md`](../docs/clients.md) contradicts — there is no built-in
authentication, so the bind address is the only access control. Text in the banner would also be text no screen reader
and no translation can reach — the `alt` attribute carries the meaning instead.

What it draws is the one thing the project does that a plain vector store does
not: a semantic path and a lexical path converging on a single node, and ranked
results leaving it. That is the RRF fusion of the sqlite-vec and FTS5 legs.

**WebP, not PNG.** 33 KB against roughly 1 MB, for an image every visitor
loads. **Measured**: the raw host serves these as `image/webp` (see the last
section), so this is not the leap it looked like before the branch was pushed.
The PNGs stay anyway — if the banner ever fails to render, swap the `srcset`
and `src` values on all four front pages to the `.png` files and nothing else
has to change.

**The colours are close to the theme tokens but not equal to them.** Measured
from the files:

| | background | accent (the diamond) |
|---|---|---|
| dark banner | `#141419` | `#6893ec` |
| dark tokens | `#16161a` | `#8ab4e8` |
| light banner | `#f8f7f5` | `#367ac2` |
| light tokens | `#f7f7f5` | `#2f5f9e` |

The backgrounds land on the tokens; both accents are more saturated. This was
accepted rather than fixed: an illustration is not chrome, and the mark that
has to be exact — `logo-{light,dark}.svg` — is. Worth knowing before generating
a v3 that has to match this one.

## Why every reference is an absolute URL, and why no reference is an SVG

A release archive ships `README.md` and no `assets/`, so a relative path breaks
the moment someone reads the README out of a download. Every page here uses an
absolute URL instead.

That directory is also **not reachable from the published site**. GitHub Pages
publishes either the repository root or `/docs`, and v0.27.0 chose `/docs` — so
`assets/` is not on the site, and a page under `docs/` cannot reach it
same-origin either. Absolute URLs are what works from everywhere.

An absolute URL resolves to `raw.githubusercontent.com`. This directory was
built on a report that the host serves `.svg` as `text/plain`, so that an
`<img>` would not render it — a report that could not be checked at the time,
because the host answered 429 for the whole session. That is why the mark was
rendered to PNG for the pages that embedded it.

**Measured 2026-08-22, with a `HEAD` against this repository's own files:**

| | status | content type |
|---|---|---|
| `grooveseek-readme-hero-light-v2.webp` | 200 | `image/webp` |
| `screenshot-light.png` | 200 | `image/png` |
| `logo-light.svg` | 200 | **`image/svg+xml`** |

So `.webp` is served correctly, and **the report about `.svg` does not hold** —
the host labels it as an image. What that measurement does *not* settle is
whether GitHub's Markdown renderer displays an SVG referenced this way: it
proxies images through `camo`, which is a second mechanism and was not tested.
The PNGs are therefore still what the pages reference, but the reason is now
"one untested step remains" rather than "the host mislabels it".

## Regenerating the PNGs

Render each SVG at 112×112 with a transparent background. Any renderer will do;
these were produced by loading the SVG at that size in a headless browser and
screenshotting the element with `omitBackground: true`.

An XML comment may not contain a double hyphen, so the SVGs spell out the CSS
custom property names rather than quoting them. Writing `--accent` inside the
comment makes the file malformed, and a browser then renders nothing at all
rather than complaining.
