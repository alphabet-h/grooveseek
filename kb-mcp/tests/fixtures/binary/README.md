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
