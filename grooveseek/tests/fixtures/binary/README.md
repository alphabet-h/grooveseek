# Binary test fixtures — PDF parser

This directory holds the first binary (non-Markdown/text) test assets in the
repository (feature-45 PR-2). All PDFs here are **hand-crafted, uncompressed,
ASCII-only PDF 1.4** files — no PDF-generation library was used except for
`encrypted.pdf` (see below), so every byte is deliberate and reproducible.
Each file is well under the 20 KB budget agreed for `src/parser/pdf.rs` test
fixtures.

## Files

| File               | Size      | Purpose                                                                 | Used by (`src/parser/pdf.rs`)                                                              |
| ------------------ | --------- | ------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------- |
| `minimal.pdf`      | 1069 B    | 2-page PDF with a real text layer and `/Title` + `/CreationDate` in the Info dict | `test_pdf_page_chunks_have_heading_and_no_level` (happy path: per-page chunking, heading `p.N`) |
| `empty_text.pdf`   | 660 B     | 1 page whose `/Contents` stream is present but has zero text-showing operators (`/Length 0`) — simulates a scanned/image-only PDF | `test_pdf_scanned_no_text_layer_is_err` (avg chars/page below the 50-char scanned-PDF threshold → `Err`) |
| `untitled.pdf`     | 848 B     | 1 page with real body text (padded past the scanned-PDF threshold) but **no `/Title`** in the Info dict, only `/CreationDate` | `test_pdf_frontmatter_falls_back_to_filename` (title must fall back to the filename stem) |
| `encrypted.pdf`    | 2047 B    | `minimal.pdf`, re-saved by `pikepdf` under AES-256 (`R=6`) encryption with a **non-empty** user password | `test_pdf_encrypted_real_fixture_is_err` (real encrypted PDF must be `Err` without calling `unlock()`) |
| `utf16_title.pdf`  | 761 B     | 1 page (padded past the scanned-PDF threshold) with `/Title` as a **literal PDF string** (`(...)`, raw bytes) encoding UTF-16BE `"日本語"` with a BOM (`0xFEFF`) | `test_pdf_recovers_utf16be_title_from_real_pdf_encoding` (Task 2.9 follow-up: `oxidize-pdf` mis-decodes this byte-by-byte through a CP1252/WinAnsi-style table instead of detecting the BOM; groove must recover the correct title) |
| `mostly_blank.pdf` | 2920 B    | 10 pages: 9 with an empty `/Contents` stream (same technique as `empty_text.pdf`) and exactly 1 (page 5) with a real 221-char text layer | `test_pdf_mostly_blank_pages_not_misclassified_as_scanned` (codex P2, PR #69 round 1: the scanned-PDF heuristic must average over *non-empty* pages only — `221 / 10 = 22 < 50` wrongly rejected the whole PDF under the old total-page-count denominator, while `221 / 1 = 221` correctly does not) |
| `cid_descendant_indirect.pdf` | 1107 B | 1 page of Japanese in a Type0 font, `/Encoding /UniJIS-UCS2-H`, **no `/ToUnicode`**, with the CIDFont written as an indirect reference (`/DescendantFonts [ 6 0 R ]`) | `test_cid_font_with_indirect_descendant_extracts_japanese` (AU-70: this form decodes correctly today and must keep doing so — the test is a regression guard on oxidize-pdf's CID path) |
| `cid_descendant_direct.pdf` | 1273 B | The same document with the CIDFont written as a **direct dictionary** (`/DescendantFonts [ << … >> ]`) — the only differing object is 5 (the Type 0 font) | not referenced by a test; kept as the minimal witness of the upstream defect and the control half of the A/B pair |
| `cid_descendant_direct_dense.pdf` | 3897 B | The direct-dictionary form with 25 lines of Japanese, so it measures 1179 chars/page **while mis-decoded** | `test_cid_font_with_direct_descendant_never_reaches_the_index_as_mojibake` (AU-70: mis-decoding doubles the character count, so the garbage clears the 50 chars/page gate — this fixture is what proves the C1 gate is load-bearing) |
| `cid_descendant_kana.pdf` | 2083 B | The direct-dictionary form whose body is **unvoiced kana only** (`あいうえお…`, 8 lines). Bytewise mis-decoding turns it into pure ASCII (`0B0D0F…`) with **zero** C1 controls at 407 chars/page | `test_kana_only_cid_mojibake_never_reaches_the_index` (PR #132 codex P1: the one shape that evades both the C1 gate and the density gate — this fixture is what proves the byte-pair-signature gate is load-bearing. oxidize-pdf 4.1.1, the pin at the time, mis-decoded it; 4.3.0 — the pin since v0.15.2, carrying our upstream fix #470 — extracts it correctly, so the test asserts across both regimes) |
| `cid_descendant_kana_labels.pdf` | 2407 B | The same unvoiced-kana evasion laid out as a **label sheet**: thirty 2-kana words, each its own positioned `BT … Td … Tj ET` run, so extraction yields 4-char tokens (`0B0K 0D0W …`, measured 148 chars/page, 0.00% C1) | `test_kana_label_sheet_mojibake_never_reaches_the_index` (PR #132 codex P1 round 2: per-run parity judgment cannot see runs this short — this fixture is what proves the short-run pair aggregation is load-bearing; same dual-regime contract as the fixture above) |

`minimal.pdf` intentionally keeps `/Title` so it stays valid for the Task 2.3
happy-path test; the filename-fallback case needed a *separate* fixture
(`untitled.pdf`) rather than mutating `minimal.pdf`, to avoid coupling two
unrelated tests to the same file.

## Anatomy of the hand-crafted PDFs

`minimal.pdf`, `empty_text.pdf` and `untitled.pdf` all follow the same
minimal PDF 1.4 skeleton:

```
%PDF-1.4
%<4 bytes >= 0x80>            <- "binary marker" comment line, conventional
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [...] /Count N >>
endobj
3 0 obj  (Page 1)
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 X 0 R >> >> /Contents Y 0 R >>
endobj
...                            <- one obj pair (Page, Contents stream) per page
X 0 obj  (Font)
<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>
endobj
N 0 obj  (Info dict — /Title optional, /CreationDate present)
<< /Title (...) /CreationDate (D:20260115093000) >>
endobj
xref
0 <object-count+1>
0000000000 65535 f 
<10-digit offset> 00000 n 
...                            <- one line per object, in object-number order
trailer
<< /Size <object-count+1> /Root 1 0 R /Info <info-obj-num> 0 R >>
startxref
<byte offset of the "xref" keyword>
%%EOF
```

`PdfDocument::extract_text()` (oxidize-pdf) only reads `BT ... Tj ... ET`
text-showing operators inside each page's `/Contents` stream, so the content
streams here use the simplest possible form: a single `Tj` per page.

### Computing xref offsets by hand

oxidize-pdf's parser (like most PDF readers) trusts the `xref` table's byte
offsets rather than scanning the whole file, so they must be exact or the
reader will fail to locate objects. To hand-write a fixture:

1. Start writing the file from `%PDF-1.4\n` + the binary-marker comment line
   (9 + 6 = 15 bytes with the marker used here — verify with your own tool if
   you change the marker).
2. Append each `N 0 obj ... endobj\n` block in object-number order, **noting
   the running byte offset before each `N 0 obj` line begins** (that's the
   value that goes in the xref table for object `N`).
3. After the last object, write the `xref` line + the table itself (`0000000000 65535 f `
   for the free head, then one `<offset:010> 00000 n ` line per object,
   *including a single trailing space and no more* — this is a fixed-width
   20-byte record per the PDF spec and some readers are strict about it).
4. Note the byte offset of the `xref` keyword itself — that goes after
   `startxref`.
5. `trailer << /Size <n+1> /Root 1 0 R /Info <info-obj> 0 R >>`.

In practice this repo's fixtures were built with a small Python helper that
writes objects to a `bytearray`, records `len(buf)` before each object is
appended, and formats the xref table from those recorded offsets — see the
"Regenerating" section below for the equivalent recipe. All three
hand-written fixtures were round-tripped through
`PdfParser::parse_bytes` (the same code path the unit tests exercise) to
confirm the offsets are correct — a wrong offset produces a parse error
immediately, so getting the arithmetic right is self-checking as long as you
actually run the parser against the file, not just eyeball it.

### Regenerating a hand-crafted fixture

There's no committed generator script (these fixtures are static test
assets, not build artifacts), but the shape used to produce them is:

```python
def build_pdf(pages: list[str], title: str | None, creation_date: str) -> bytes:
    buf = bytearray()
    offsets: dict[int, int] = {}

    def obj(num: int, body: bytes):
        offsets[num] = len(buf)
        buf.extend(f"{num} 0 obj\n".encode())
        buf.extend(body)
        buf.extend(b"\nendobj\n")

    buf.extend(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    # object 1 = Catalog, 2 = Pages, 3..3+2N-1 = (Page, Contents) pairs,
    # next = Font, last = Info — assign object numbers, call obj() for each
    # in ascending order, then:
    xref_offset = len(buf)
    buf.extend(f"xref\n0 {len(offsets) + 1}\n".encode())
    buf.extend(b"0000000000 65535 f \n")
    for n in sorted(offsets):
        buf.extend(f"{offsets[n]:010} 00000 n \n".encode())
    buf.extend(
        f"trailer\n<< /Size {len(offsets) + 1} /Root 1 0 R /Info {max(offsets)} 0 R >>\n"
        f"startxref\n{xref_offset}\n%%EOF".encode()
    )
    return bytes(buf)
```

Adjust which object is Root/Info/page count as needed, write the result with
`open(path, "wb")`, then run
`cargo test --lib parser::pdf` to confirm oxidize-pdf accepts it.

### `utf16_title.pdf` — encode `/Title` as a *literal* PDF string, not a hex string

`utf16_title.pdf`'s Info dict `/Title` is written as `(` + raw bytes + `)`
(the literal-string PDF syntax), i.e. `bytes([0xFE, 0xFF, 0x65, 0xE5, 0x67,
0x2C, 0x8A, 0x9E])` (UTF-16BE BOM + "日本語") sandwiched directly between
parentheses — **not** the hex-string form `<FEFF65E5672C8A9E>`. This
distinction matters and is not interchangeable for this fixture's purpose:
while building this fixture (Task 2.9 follow-up, 2026-07-19) a first attempt
used a hex string, and `oxidize-pdf` (4.1.1) came back with an **empty**
title (silently — no error, `meta.title` was `None`/empty), so the
filename-fallback path fired instead of the mis-decode bug the fixture is
meant to exercise. Switching to the literal-string form reproduced the exact
mis-decode found in the real Downloads PDF during dogfooding. If you need
another non-ASCII-title fixture, use the literal-string form and verify
end-to-end (`cargo test --lib parser::pdf::tests::test_pdf_recovers_utf16be_title...`)
that it actually reproduces mis-decoded output before wiring the fix's
"before" state into a test — don't assume any Unicode PDF-string
representation exercises oxidize-pdf's decode paths identically.

### `mostly_blank.pdf` — many blank pages, one dense page

Same hand-crafted recipe as `minimal.pdf` / `untitled.pdf`, just extended to
`NUM_PAGES = 10` in a loop instead of writing each `N 0 obj` by hand: for
`i` in `1..=10`, emit a `(Page, Contents)` object pair, where `Contents` is
an empty stream (`/Length 0`, same technique as `empty_text.pdf`) for every
page except `i == 5`, which gets a `BT ... Tj ... ET` content stream with a
221-character string. Object numbering: `1` = Catalog, `2` = Pages,
`2+i` = page `i`, `2+NUM_PAGES+i` = that page's Contents, then Font, then
Info — same offset-tracking `obj()` helper as the other fixtures, just
called in a loop. Exists specifically to reproduce the codex P2 finding
(PR #69 round 1) that averaging chars-per-page over *all* pages (including
blank ones) dilutes a real content page's density below the scanned-PDF
threshold; regenerate by adjusting `NUM_PAGES` / `TEXT_PAGE_INDEX` /
`TEXT_PAGE_CONTENT` if a different page count or position is needed, and
re-run `cargo test --lib parser::pdf::tests::test_pdf_mostly_blank_pages_not_misclassified_as_scanned`
to confirm the offsets are correct and the fixture still exercises the
intended density gap (i.e. verify it fails against the pre-fix code first —
see the git history around the codex P2 fix commit for the exact numbers
this fixture was tuned against).

### The `cid_descendant_*.pdf` trio — one variable, three files

These are PDF 1.7 (the others are 1.4; the version is irrelevant to what they
exercise, it was simply the baseline chosen for a Type0/CID fixture) and follow
the same hand-written skeleton, with the text drawn by hex strings whose code
points *are* the UTF-16BE values — that is what `/Encoding /UniJIS-UCS2-H`
means, so no font file is embedded and the whole file stays ASCII apart from
the binary marker.

Object layout, contiguous `1..7` in every variant: `1` Catalog, `2` Pages,
`3` Page, `4` Contents, `5` the Type 0 font, `6` the CIDFont, `7` the
FontDescriptor. Object 6 is written in every file whether or not anything
references it, and the FontDescriptor is an indirect reference from the CIDFont
on purpose — ISO 32000-1 Table 117 marks it *(Required; shall be an indirect
reference)*, and `pdfminer.six` reads all three files warning-free, which
keeps "a conformant PDF is mishandled" defensible when citing these fixtures
upstream. The first cut of these files omitted the descriptor and pdfminer
flagged it; that is exactly the kind of nit an upstream maintainer would
reject a repro over.

`cid_descendant_indirect.pdf` and `cid_descendant_direct.pdf` differ in
**exactly one object** (5, the Type 0 font): whether the CIDFont inside
`/DescendantFonts` is an indirect reference or a direct dictionary. Content
stream, text bytes, `/Encoding`, `/CIDSystemInfo` and the object set are
identical. Against oxidize-pdf 4.2.2 (the behavior fixed in 4.3.0 by our
upstream PR bzsanti/oxidizePdf#470 — both files extract identically since):

| `/DescendantFonts` | extracted text |
| --- | --- |
| `[ 6 0 R ]` | `第1章 概要 GRIMWALD` |
| `[ << /Subtype /CIDFontType0 … >> ]` | `{, 1zà i\u{82}\u{89}\u{81} G R I M W A L D` |

(`第` is U+7B2C, whose UTF-16BE bytes `7B 2C` are the leading `{,`.) ISO 32000-1
Table 121 types `DescendantFonts` as an array with no reference requirement,
and §7.3.7 lets any dictionary value be direct or indirect;
`extraction_cmap.rs` (through 4.2.3) read only the reference, which left
`descendant_font` empty and skipped the branch that already resolved
`UniJIS-UCS2-H` correctly; 4.3.0 reads all four legal spellings.
Keeping both files is the point — a single fixture would show mojibake without
establishing *what* causes it. (A third spelling, the `/DescendantFonts` value
itself as an indirect reference to the array, mis-decodes identically — it is
kept as scratch evidence for the upstream report rather than committed here,
because the C1 gate the committed fixtures exercise is spelling-agnostic.)

`cid_descendant_direct_dense.pdf` exists because the minimal direct-dictionary
file is only 27 chars/page, so it would be rejected by the density gate whether
or not the mojibake gate existed. Repeating the body to 1179 chars/page is what
makes the C1 check the only thing standing between mis-decoded text and the
index. Verified by raising `MISDECODED_C1_RATIO` above 1.0 and confirming the
test fails with the mojibake in the assertion output.

To regenerate: use the `build_pdf` recipe above with the object layout listed
here, emitting the body as `<hex> Tj` where `hex` is
`text.encode("utf-16-be").hex()`. Then re-run
`cargo test --lib parser::pdf` and confirm both AU-70 tests pass, and run
pdfminer.six over all three files expecting warning-free, correct Japanese.

## `encrypted.pdf` — real encrypted fixture

Unlike the other three, `encrypted.pdf` is **not** hand-written byte-by-byte
(hand-rolling RC4/AES PDF encryption is not a good use of a test fixture).
It was generated by re-saving `minimal.pdf` through
[`pikepdf`](https://pikepdf.readthedocs.io/) (Python binding for `qpdf`)
with a **non-empty user password**:

```python
import pikepdf

pdf = pikepdf.open("minimal.pdf")
pdf.save(
    "encrypted.pdf",
    encryption=pikepdf.Encryption(user="userpw", owner="ownerpw", R=6),
)
```

Equivalent with the `qpdf` CLI directly (if available instead of Python):

```sh
qpdf --encrypt userpw ownerpw 256 -- minimal.pdf encrypted.pdf
```

`R=6` / `256` selects AES-256 encryption (PDF 2.0-style, the strongest qpdf
supports); any revision qpdf/pikepdf can produce is acceptable here — the
test only asserts that `PdfParser` returns `Err` when the password is never
supplied, not that a specific encryption algorithm is rejected.

**Why the user password must be non-empty**: PDF encryption allows an empty
("") user password, which most readers (including oxidize-pdf) will accept
transparently without prompting — the file *behaves* as if unencrypted. If
`encrypted.pdf` were built with an empty user password, `PdfParser` would
successfully extract text from it and `test_pdf_encrypted_real_fixture_is_err`
would fail to reproduce the "real world locked PDF" scenario it exists to
guard. Using a non-empty password (`"userpw"`) forces oxidize-pdf down the
locked-document error path, which is confirmed (dry-run) to raise:

```
PDF text extraction failed (possibly encrypted or unreadable): PDF is locked:
call unlock() with the correct password before reading objects
```

i.e. the same `"encrypted"`/`"unreadable"` wording asserted by both
`test_pdf_encrypted_is_err` (cheap structurally-broken-bytes fixture, no
dependency on `pikepdf`/`qpdf` being installed) and
`test_pdf_encrypted_real_fixture_is_err` (this real fixture, exercising the
actual encrypted-PDF code path end-to-end).

If neither `qpdf` nor `pikepdf` is available in a given environment, skip
this fixture and keep relying on `test_pdf_encrypted_is_err`'s
structurally-broken bytes — `encrypted.pdf` is a defense-in-depth addition,
not a replacement for that lighter-weight test.
