# assets

| File | What it is |
| --- | --- |
| `logo-light.svg` / `logo-dark.svg` | The mark, and the editable source of truth. Lines of a document with the `◆` that also sits beside the `/ui` heading, marking the passage a search found. Colours are the accent and muted tokens defined in `grooveseek/src/transport/webui_index.html`. |
| `logo-light.png` / `logo-dark.png` | 112×112 renders of those SVGs, at 2× the 56px the READMEs display. **These are what the READMEs reference.** |
| `screenshot-light.png` / `screenshot-dark.png` | The operator view at `/ui`, 980×860, one per color scheme. |

## Why the READMEs point at the PNGs and not the SVGs

The READMEs reference images by absolute URL, because a release archive ships
`README.md` and no `assets/`. An absolute URL resolves to
`raw.githubusercontent.com`, which is reported to serve `.svg` as `text/plain`
so that an `<img>` will not render it. That report could not be measured from
the machine this was built on — the host answered 429 for the whole session —
so the PNGs are used instead: the screenshots are PNG regardless, so this makes
the READMEs depend on one mechanism rather than two.

The SVGs stay because they are the source the PNGs are rendered from.

They are **not** reachable from the published site. GitHub Pages publishes
either the repository root or `/docs`, and v0.27.0 chose `/docs` — so this
directory is not on the site, and a page there cannot reference these files
same-origin either. The PNGs by absolute URL are what works from everywhere.

## Regenerating the PNGs

Render each SVG at 112×112 with a transparent background. Any renderer will do;
these were produced by loading the SVG at that size in a headless browser and
screenshotting the element with `omitBackground: true`.

An XML comment may not contain a double hyphen, so the SVGs spell out the CSS
custom property names rather than quoting them. Writing `--accent` inside the
comment makes the file malformed, and a browser then renders nothing at all
rather than complaining.
