//! The corpus side of the `kb://` resource surface: which documents this
//! server will hand over, the URIs it advertises for them, and what a
//! `resources/read` gets back.
//!
//! Not to be confused with the sibling crate module `crate::resources`, which
//! is the *URI* side — it builds `kb://` strings and takes them apart, and
//! says so: "this module builds URIs and takes them apart; it does not know
//! the corpus." This one is the half that knows the corpus.
//!
//! Split out of `server.rs` in audit L-1 (PR-3), the last of the three, on the
//! same terms as PR-1 and PR-2: bodies byte-identical and in their original
//! order, `mod tests` left alone in the parent, and every `pub(super)` below
//! put there because `cargo check` named it after a move made with no
//! visibility changes at all.

// The parent module is what this file was carved out of, so it keeps seeing
// exactly what it saw before. A hand-written list would be a second thing to
// maintain and, on a move this size, a place to silently drop a name.
use super::*;

/// Whether a failed load says something about the **document** or about the
/// **server**.
///
/// `get_document` does not need the distinction — it answers with one JSON
/// error envelope either way. `resources/read` does: MCP gives it two codes,
/// and a client that cannot tell "there is no such resource" from "the index is
/// unreadable" will retry the wrong one, or stop retrying the one it should.
/// `list_resources` already reported a failed index query as an internal error,
/// so collapsing everything here also made the two disagree about the same
/// failure (codex P2 round 3 on PR #162).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadFailure {
    /// Nothing here to hand over: absent, or outside what this server serves.
    NotServed,
    /// The server could not answer. Not a claim about the document.
    Internal,
}

/// (feature-50) A hit, plus the `kb://doc/...` URI that names its document as a
/// resource.
///
/// Flattened, so the hit's own fields keep the shape and the position they had
/// — one new key, nothing moved. The MCP result stays a single text content
/// block carrying this JSON, which is what keeps every existing client working:
/// adding a `resource_link` content block instead would have changed the length
/// of the `content` array.
///
/// The specification permits handing back links to documents that
/// `resources/list` never enumerated, which is what makes the topic-group
/// listing and per-document addressing coexist.
///
/// The key is **omitted** for a hit [`ServableRules`] would not hand over —
/// an extension the active parser registry no longer covers, or a document
/// past the size a read is allowed to return. Such a row stays in the index on
/// purpose (AU-06) and stays in the search results, but neither `get_document`
/// nor `resources/read` will open it — so the honest answer is no link, not a
/// broken one.
#[derive(Serialize)]
pub(super) struct HitWithUri {
    #[serde(flatten)]
    hit: crate::db::SearchHit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) uri: Option<String>,
}

impl HitWithUri {
    pub(super) fn new(hit: crate::db::SearchHit, rules: &ServableRules<'_>) -> Self {
        let uri = rules
            .allows(&hit.path)
            .then(|| crate::resources::doc_uri(&hit.path));
        Self { hit, uri }
    }
}

/// The single decision about whether an indexed document can be handed over.
///
/// Two surfaces ask it — `resources/list` through
/// [`KbCore::servable_document_paths`], and the `uri` on every `search` hit —
/// and each used to call [`crate::indexer::extension_is_registered`] itself.
/// That was harmless only for as long as the two predicates were the same one
/// call: adding the size condition to the listing alone is precisely what
/// makes a `search` hand out a `kb://doc/...` that `resources/read` then
/// refuses, which is the shape of the finding this closes.
///
/// Loading it costs one query that normally returns nothing: only rows past
/// the *smallest* read cap can be excluded, and a knowledge base with no
/// document over 1 MiB has none.
pub(crate) struct ServableRules<'a> {
    registry: &'a Registry,
    oversized: std::collections::HashSet<String>,
    /// False when the recorded sizes could not be read at all. An empty
    /// `oversized` then means "found none", which is the opposite of what the
    /// caller knows, so the two states must not share a representation
    /// (codex P2 round 1).
    sizes_known: bool,
}

impl<'a> ServableRules<'a> {
    /// `rows` は `documents_larger_than(GET_DOCUMENT_MAX_BYTES)` の結果。
    /// 拡張子ごとの本当の cap は [`max_bytes_for`] で当てる —
    /// `load_document_blocking` が `read_checked` に渡すのと**同じ chooser** なので、
    /// 提示側と read 側が別々の上限を持つことが構造的に起こらない。
    pub(crate) fn new(registry: &'a Registry, rows: Vec<(String, u64)>) -> Self {
        let oversized = rows
            .into_iter()
            .filter(|(path, size)| {
                let ext = std::path::Path::new(path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                *size
                    > max_bytes_for(
                        registry,
                        ext,
                        crate::parser::MAX_RAW_BINARY_BYTES,
                        GET_DOCUMENT_MAX_BYTES,
                    )
            })
            .map(|(path, _)| path)
            .collect();
        Self {
            registry,
            oversized,
            sizes_known: true,
        }
    }

    /// The rules to use when the sizes could not be read.
    ///
    /// Offers nothing. A URI a client cannot follow is worse than no URI, and
    /// an empty oversized set would claim the opposite of what is known.
    pub(crate) fn sizes_unavailable(registry: &'a Registry) -> Self {
        Self {
            registry,
            oversized: std::collections::HashSet::new(),
            sizes_known: false,
        }
    }

    pub(crate) fn allows(&self, path: &str) -> bool {
        self.sizes_known
            && crate::indexer::extension_is_registered(path, self.registry)
            && !self.oversized.contains(path)
    }

    /// The documents held back for their size, sorted so a report is stable.
    ///
    /// `groove doctor` explains what the resource surface is withholding, and
    /// it has to explain *this* predicate rather than recompute an equivalent
    /// one — a doctor that answers a slightly different question than the
    /// server is worse than no doctor.
    pub(crate) fn oversized_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.oversized.iter().cloned().collect();
        v.sort();
        v
    }
}

impl KbCore {
    /// The indexed documents this server can actually hand over.
    ///
    /// Not the same set as `all_document_paths`. Narrowing `[parsers].enabled`
    /// without reindexing deliberately **keeps** those rows — `run_server` warns
    /// about them instead of deleting them, because a narrowed setting is often
    /// temporary — and `load_document_blocking` then refuses their extension.
    /// Advertising them would be offering a link the very next call rejects.
    ///
    /// So the filter lives on the one query the whole resource surface asks,
    /// rather than at each of the three places that emit a URI. That is the
    /// same reason `load_document_blocking` exists at all.
    fn servable_document_paths(&self) -> Result<Vec<String>, String> {
        let (paths, oversized) = {
            let db = recover_db(self.db.lock());
            let paths = db
                .all_document_paths()
                .map_err(|e| format!("failed to list indexed documents: {e}"))?;
            let oversized = db
                .documents_larger_than(GET_DOCUMENT_MAX_BYTES)
                .map_err(|e| format!("failed to read indexed document sizes: {e}"))?;
            (paths, oversized)
        };
        let rules = ServableRules::new(&self.parser_registry, oversized);
        Ok(paths.into_iter().filter(|p| rules.allows(p)).collect())
    }

    /// The topic groups `resources/list` answers from.
    ///
    /// Built from the servable paths, which is the same list a read is checked
    /// against — so a URI this listing offers cannot fail membership when the
    /// client reads it back.
    pub(super) fn topic_groups_blocking(
        &self,
    ) -> Result<Vec<crate::resources::TopicGroup>, String> {
        self.servable_document_paths()
            .map(|paths| crate::resources::topic_groups(&paths))
    }

    /// What a `resources/read` of a document produces: its text, and the media
    /// type that text actually is.
    ///
    /// Not the media type of the file on disk. A PDF or a spreadsheet is served
    /// as the text the parser extracted, because that is what an MCP client can
    /// use, so calling it `application/pdf` would be a lie about the bytes it is
    /// holding.
    fn resource_mime_for(ext: &str) -> &'static str {
        if ext.eq_ignore_ascii_case("md") {
            "text/markdown"
        } else {
            "text/plain"
        }
    }

    /// Serve one `kb://` URI, or say why not. Returns `(text, mime)`.
    pub(super) fn read_resource_blocking(
        &self,
        parsed: &crate::resources::ResourceUri,
        uri: &str,
    ) -> Result<(String, &'static str), (LoadFailure, String)> {
        let paths = self
            .servable_document_paths()
            .map_err(|e| (LoadFailure::Internal, e))?;

        match parsed {
            crate::resources::ResourceUri::Topic(prefix) => {
                let group = crate::resources::topic_groups(&paths)
                    .into_iter()
                    .find(|g| &g.prefix == prefix)
                    .ok_or_else(|| {
                        (
                            LoadFailure::NotServed,
                            format!("no such topic group: {uri}"),
                        )
                    })?;
                let mut out = format!("# {}\n\n{}\n\n", group.display_name(), group.description());
                for p in &group.paths {
                    out.push_str(&format!("- `{p}` — {}\n", crate::resources::doc_uri(p)));
                }
                Ok((out, "text/markdown"))
            }
            crate::resources::ResourceUri::Doc(rel) => {
                // Membership first. `resources/read` is for what was offered —
                // strictly narrower than `get_document`, so it cannot widen
                // what is reachable.
                //
                // The message says "offers", not "indexed", because those
                // stopped being the same thing: a document can be indexed and
                // still be held back, by an extension the registry dropped or a
                // size past what a read returns. Naming the index would send
                // someone looking for a document `groove status` counts.
                if !paths.iter().any(|p| p == rel) {
                    return Err((
                        LoadFailure::NotServed,
                        format!(
                            "not a document this server offers: {uri} \
                             (if it is indexed, `groove doctor` says why it is held back)"
                        ),
                    ));
                }
                // Then the guards, by sharing the body `get_document` uses
                // rather than re-deriving them: symlink and hard-link refusal,
                // traversal, extension membership, size cap, handle-bound read.
                // A second sequence is how the two would come to disagree.
                let (doc, ext) = self
                    .load_document_blocking(rel)
                    .map_err(|(kind, e)| (kind, format!("{}: {uri}", e.error)))?;
                if doc.truncated {
                    tracing::warn!(
                        "resources/read: {rel} extracted more than {EXTRACTED_TEXT_MAX_BYTES} \
                         bytes of text; the resource carries the prefix and says so"
                    );
                }
                Ok((
                    resource_text(doc.content, doc.truncated),
                    Self::resource_mime_for(&ext),
                ))
            }
        }
    }
}

/// What a resource read appends when the text it is handing over is a prefix.
///
/// One blank line and a fenced-off sentence, so it reads as an annotation in
/// both media types this server serves and cannot be mistaken for the
/// document's own last paragraph.
const TRUNCATION_NOTICE: &str = "\n\n---\n\n*[groove] Truncated: the extracted text exceeded 1 MiB. \
     What is above is the beginning of the document, not all of it.*\n";

/// The text a resource read hands over, with truncation stated in the body.
///
/// `get_document` returns a JSON envelope with a `truncated` field; a resource
/// read returns bare text, and dropping the flag there presented a prefix as
/// the whole document (codex P2 round 5 on PR #162). The prefix is still worth
/// serving — refusing it would lose text a client can use — so the answer is
/// the same one BU-31 reached for a query cut at the phrase cap: hand over what
/// there is, and say that is what it is.
pub(super) fn resource_text(content: String, truncated: bool) -> String {
    if truncated {
        content + TRUNCATION_NOTICE
    } else {
        content
    }
}

/// Turn a [`LoadFailure`] into the JSON-RPC error that says the same thing.
///
/// Only a statement about the resource gets the not-found code; a failure of
/// the server's own stays an internal error, which is what `list_resources`
/// already reports for the identical unreadable index. Written as a function so
/// the mapping can be asserted directly — the two codes mean different things
/// to a retrying client, and nothing else would notice them being collapsed.
pub(super) fn resource_error(kind: LoadFailure, message: String) -> rmcp::ErrorData {
    match kind {
        LoadFailure::NotServed => rmcp::ErrorData::resource_not_found(message, None),
        LoadFailure::Internal => internal_error(message),
    }
}

#[cfg(test)]
mod tests {
    // No `use super::*;`: both tests below read this file as text and assert on
    // what they find. They name nothing from the module they are checking,
    // which is what lets them notice code leaving it.

    /// `all_document_paths` is the raw index, and the raw index is not what this
    /// server can serve: narrowing `[parsers].enabled` without reindexing keeps
    /// those rows on purpose (AU-06) while `load_document_blocking` refuses
    /// their extension. Everything the resource surface advertises therefore has
    /// to come through `servable_document_paths`. A second, direct call is
    /// exactly how `resources/list` came to offer a `kb://doc/...` that
    /// `resources/read` then rejected.
    #[test]
    fn the_resource_surface_reads_the_index_through_the_registry_filter() {
        let src = include_str!("kb_uri.rs").replace("\r\n", "\n");

        let core_start = src
            .find("\nimpl KbCore {")
            .expect("the `impl KbCore` block moved or was renamed");
        let core_block = &src[core_start..];
        let core_end = core_block[1..]
            .find("\n}\n")
            .expect("could not find the end of the KbCore impl block");
        let core = &core_block[..core_end];

        // Anti-vacuity: this really is the block that holds both the filter and
        // the bodies that must not go around it.
        for needed in [
            "fn servable_document_paths",
            "fn topic_groups_blocking",
            "fn read_resource_blocking",
        ] {
            assert!(
                core.contains(needed),
                "`{needed}` is not in the block this test scanned — the \
                 extraction broke and the assertion below is vacuous."
            );
        }

        assert_eq!(
            core.matches("all_document_paths()").count(),
            1,
            "the raw index query must appear exactly once inside `impl KbCore`, \
             in `servable_document_paths`. Any other call site skips the parser \
             registry filter and advertises a URI a read will refuse."
        );
    }

    /// The companion to the scan above, for the half it could not see.
    ///
    /// `HitWithUri::new` lives outside `impl KbCore`, so the listing test never
    /// covered it — and it called `extension_is_registered` directly, which is
    /// how `search` and `resources/list` could come to disagree the moment one
    /// of them grew a second condition. Both now go through `ServableRules`,
    /// and the way to keep it that way is for the raw predicate to have exactly
    /// one call site in this file.
    #[test]
    fn only_one_place_in_this_file_decides_what_is_servable() {
        let src = include_str!("kb_uri.rs").replace("\r\n", "\n");
        // Scan production code only: this test's own search string is a match
        // for itself, and counting it made the assertion fail for a reason that
        // had nothing to do with the invariant.
        let tests_start = src
            .find("\n#[cfg(test)]\nmod tests")
            .expect("the test module header moved or was renamed");
        let prod = &src[..tests_start];
        assert!(
            prod.contains("impl<'a> ServableRules<'a>"),
            "the extraction broke: the predicate is not in the scanned region, \
             so the count below would be vacuous."
        );
        // Doc comments name the function too; count the calls, not the mentions.
        let calls = prod.matches("extension_is_registered(").count();
        assert_eq!(
            calls, 1,
            "`extension_is_registered` must be called only from \
             `ServableRules::allows`. A second call site is a second definition \
             of \"servable\", and the size condition would apply to one of them."
        );
    }
}
