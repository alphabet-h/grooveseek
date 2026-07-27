# 1. Withdraw `.xls` (legacy BIFF) support

- Status: accepted
- Date: 2026-07-27
- Deciders: project owner
- Applies to: v0.14.0 (`.xls` was indexable in v0.11.0 through v0.13.1)

## Context and Problem Statement

`.xls` was added alongside `.docx` / `.xlsx` / `.pptx` in v0.11.0, sharing the
[calamine](https://crates.io/crates/calamine) reader with `.xlsx`. A security
audit of the binary parsers found that the two formats do not share a memory
profile, and that the comment justifying the `.xls` path was wrong.

`.xlsx` is read as a stream. `.xls` is not: `calamine::Xls::new()` parses the
whole workbook eagerly, holding every sheet in a `BTreeMap<String, SheetData>`
and calling `Range::from_sparse` for each, which takes the bounding rectangle
of the populated cells and allocates it densely as
`vec![Data::default(); rows * cols]`.

The source claimed this was safe because BIFF caps a sheet at 65,536 × 256.
**That bounds a sheet, not a workbook.** Measured:

| Quantity | Value |
|---|---|
| `size_of::<calamine::Data>()` | 32 B |
| Maximal sheet (65,536 × 256) | 16,777,216 cells = **512 MB** |
| `worksheet_range()` returns a clone | **1 GB** peak per sheet in flight |
| Workbook limit | **none** — (sheet count) × 512 MB |

Two cell records at opposite corners are enough to make a sheet maximal, so a
crafted file of a few tens of kilobytes can declare enough sheets to exhaust
memory. An allocation failure aborts the process rather than returning an
error, so neither the per-file skip nor the parser panic guard — both of which
already protect the other formats — can contain it. A single file in a watched
directory could take down the server.

## Decision Drivers

- Reading a knowledge base must not let one input file terminate the process.
- The guards that make the other binary formats safe are structurally unable
  to help here, because the failure is an abort, not an error or a panic.
- `.xls` is a pre-2007 format with a lossless conversion path (`.xlsx`), and
  no user had asked for it.

## Considered Options

1. **Bound the allocation with a pre-check before `calamine` opens the file.**
   Walk the CFB (OLE2) container ourselves and read the BOUNDSHEET and
   DIMENSIONS records to learn the sheet count and declared extents, then
   refuse oversized workbooks before `Xls::new()` runs.
2. **Accept the ceiling and document it.**
3. **Get a borrow-based or streaming BIFF API upstream.**
4. **Withdraw the format.**

## Decision Outcome

Chosen option: **4 — withdraw the format**, because it is the only option that
closes the hole without new dependencies, and the cost to users is a file
conversion.

Listing `"xls"` in `[parsers].enabled` is now rejected at startup with the
reason. The recommended path for affected workbooks is conversion to `.xlsx`.

Why the others were not chosen:

- **Option 1** is the correct long-term fix but is not a small change: it
  needs a new dependency (`cfb`) and a BIFF record walker, to guard a format
  nobody had requested. Note that the check cannot be placed where the audit
  originally proposed — inside or before `worksheet_range()`. By the time that
  function is reached the dense allocation is already done inside
  `Xls::new()`, and `Xls` does not implement `ReaderRef`, so there is no
  borrow-based accessor and even the clone is unavoidable. Any pre-check has
  to run strictly before `Xls::new()`.
- **Option 2** was briefly favoured and is wrong. It rests on there being a
  ceiling to accept; the first analysis found the per-sheet bound, concluded
  "bounded at 1 GB, acceptable", and missed that `sheets` is a map with no
  cardinality limit. **Anyone reopening this should confirm which unit a
  format's stated limit applies to before treating it as a bound.**
- **Option 3** removes the problem for everyone but is not actionable on our
  schedule.

### Consequences

- The abort path is gone. No `.xls` file reaches `calamine`.
- Upgrading from v0.11.0–v0.13.1 with `"xls"` still in `[parsers].enabled` is
  a **startup error, not a silent downgrade**. This forced a related fix:
  `kb-mcp index` now validates `[parsers].enabled` before it opens the
  database, loads the embedding model, or — with `--force` — performs the
  reset. Previously a config carrying a now-rejected id emptied the database
  and then exited, leaving no index at all.
- `kb-mcp serve` warns when the index still holds documents whose extension
  `[parsers].enabled` no longer covers. Those rows are pruned by the next
  `kb-mcp index`, but `serve` does not index, so a server-only installation
  keeps them and they surface as hits that search returns and `get_document`
  then refuses. The warning names the count and an example; it deletes
  nothing, because a narrowed `enabled` list is often temporary.
- `XlsParser` and its unit tests remain in the tree but are unreachable from
  the registry. They are kept deliberately, so that option 1 can be
  implemented later without reconstructing the extraction path.
- Users with `.xls` archives must convert them. There is no in-tree migration.

### Confirmation

- `Registry::from_enabled` rejects `"xls"` with an explanation; covered by
  unit tests.
- `[parsers].enabled` validation happens before any side effect — a rejected
  config leaves no database behind and downloads no model.

## More Information

- Audit item AU-06, implemented in PR #107 (v0.14.0)
- `CHANGELOG.md`, v0.14.0 → Removed
- Re-entry conditions are tracked privately; the trigger is a user report of
  `.xls` being rejected.
- Japanese version: [0001-withdraw-xls-legacy-biff-support.ja.md](./0001-withdraw-xls-legacy-biff-support.ja.md)
