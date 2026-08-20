//! Reading a document out of the knowledge base, and deciding whether it may
//! be read at all: the `get_document` and `get_best_practice` tool bodies, the
//! four-stage path check they go through, and the size limits they are held to.
//!
//! Split out of `server.rs` in audit L-1 (PR-2), after the search half
//! (PR-1) and on the same terms: bodies byte-identical, in their original
//! order, `mod tests` left alone in the parent.
//!
//! The private items that gained `pub(super)` did so because the parent still
//! calls them -- the compiler was asked, not consulted from memory: the move
//! was made without widening anything, and each one below is a name `cargo
//! check` then reported as unreachable.

// The parent module is what this file was carved out of, so it keeps seeing
// exactly what it saw before. A hand-written list would be a second thing to
// maintain and, on a move this size, a place to silently drop a name.
use super::*;

impl KbCore {
    pub(super) fn get_document_blocking(&self, params: GetDocumentParams) -> String {
        match self.load_document_blocking(&params.path) {
            Ok((doc, _ext)) => serde_json::to_string_pretty(&doc).unwrap_or_default(),
            // The category is for `resources/read`, which has two codes to
            // choose between. This tool has one envelope either way, so its
            // output is unchanged by carrying it.
            Err((_kind, e)) => serde_json::to_string_pretty(&e).unwrap_or_default(),
        }
    }

    /// Every guard a document has to clear, in one place, plus the extraction.
    ///
    /// Shared by `get_document` — which wraps the result in its JSON envelope —
    /// and `resources/read`, which returns the extracted text. Two call sites
    /// with two copies of this sequence is how a guard ends up applying to one
    /// of them; `max_bytes_for` exists for the same reason one level down.
    ///
    /// The extension is handed back because the caller needs it and it must be
    /// the **canonical** one the checks used, not the one from the requested
    /// path (BU-22: Windows 8.3 short names make those differ).
    pub(super) fn load_document_blocking(
        &self,
        rel: &str,
    ) -> Result<(DocumentResponse, String), (LoadFailure, ErrorResponse)> {
        // (BU-22) Both caps go in; `validate_get_document_path` picks between
        // them from the canonical extension, which is the same one its
        // registry-membership check uses.
        let canonical = validate_get_document_path(
            &self.kb_path,
            rel,
            &self.parser_registry,
            GET_DOCUMENT_MAX_BYTES,
            crate::parser::MAX_RAW_BINARY_BYTES,
        )
        .into_result()?;
        let ext = canonical.extension().and_then(|e| e.to_str()).unwrap_or("");
        // (BU-20) The validation above checked a path; this checks the handle
        // the bytes actually come from, so nothing renamed over that path in
        // between is read. The cap comes from the shared chooser rather than
        // being recomputed, so the two steps cannot enforce different limits.
        let cap = max_bytes_for(
            &self.parser_registry,
            ext,
            crate::parser::MAX_RAW_BINARY_BYTES,
            GET_DOCUMENT_MAX_BYTES,
        );
        match crate::links::read_checked(&canonical, cap) {
            Ok(crate::links::Content::Bytes(bytes)) => {
                match build_document_response(&self.parser_registry, rel, ext, &bytes) {
                    Ok(resp) => Ok((resp, ext.to_string())),
                    // The document is there; producing text from it failed.
                    // That is the server's problem, not a missing resource.
                    Err(e) => Err((
                        LoadFailure::Internal,
                        ErrorResponse {
                            error: format!("Failed to extract document: {e}"),
                        },
                    )),
                }
            }
            Ok(crate::links::Content::Refused(refused)) => {
                tracing::warn!("{}", refused.log_line(&canonical));
                Err((
                    LoadFailure::NotServed,
                    ErrorResponse {
                        error: refused.client_message().to_string(),
                    },
                ))
            }
            Err(e) => Err((
                LoadFailure::Internal,
                ErrorResponse {
                    error: format!("Failed to read file: {e}"),
                },
            )),
        }
    }

    pub(super) fn get_best_practice_blocking(&self, params: GetBestPracticeParams) -> String {
        if self.best_practice_templates.is_empty() {
            return serde_json::to_string_pretty(&ErrorResponse {
                error: "get_best_practice is not configured. Add `[best_practice].path_templates` to groove.toml (for example: `path_templates = [\"best-practices/{target}/PERFECT.md\"]`) to enable this tool.".to_string(),
            })
            .unwrap_or_default();
        }
        let canonical = match resolve_best_practice_path(
            &self.kb_path,
            &self.best_practice_templates,
            &params.target,
            &self.parser_registry,
            GET_DOCUMENT_MAX_BYTES,
        ) {
            ResolveOutcome::Found(p) => p,
            ResolveOutcome::NotFound(tried) => {
                // (BU-23) The candidate paths are built from
                // `[best_practice].path_templates`, so echoing them back hands
                // an unauthenticated caller the server's configured layout —
                // directory names it may not otherwise know exist. The count
                // is enough for the caller to tell "no template matched" from
                // "the tool is not configured"; the operator gets the paths
                // themselves on stderr.
                tracing::debug!(
                    target = %params.target,
                    tried = ?tried,
                    "get_best_practice found no matching template"
                );
                return serde_json::to_string_pretty(&ErrorResponse {
                    error: best_practice_not_found_message(&params.target, &tried),
                })
                .unwrap_or_default();
            }
            ResolveOutcome::Denied(err) => {
                return serde_json::to_string_pretty(&err).unwrap_or_default();
            }
        };

        // (BU-20) Same handle-checked read as `get_document`; the templates
        // resolve to a path, and a path is what can be swapped. The cap is the
        // one `resolve_best_practice_path` already applied to this file.
        let content = match crate::links::read_checked(&canonical, GET_DOCUMENT_MAX_BYTES) {
            Ok(crate::links::Content::Bytes(bytes)) => match String::from_utf8(bytes) {
                Ok(s) => Ok(s),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                )),
            },
            Ok(crate::links::Content::Refused(refused)) => {
                tracing::warn!("{}", refused.log_line(&canonical));
                return serde_json::to_string_pretty(&ErrorResponse {
                    error: refused.client_message().to_string(),
                })
                .unwrap_or_default();
            }
            Err(e) => Err(e),
        };

        match content {
            Ok(content) => {
                if let Some(ref cat) = params.category {
                    // Extract a specific h2 section
                    match extract_section(&content, cat) {
                        Some(section) => {
                            let resp = BestPracticeResponse {
                                target: params.target,
                                category: Some(cat.clone()),
                                content: section,
                            };
                            serde_json::to_string_pretty(&resp).unwrap_or_default()
                        }
                        None => {
                            // Return available sections as guidance
                            let sections = list_h2_sections(&content);
                            serde_json::to_string_pretty(&ErrorResponse {
                                error: format!(
                                    "Section '{}' not found. Available sections: {}",
                                    cat,
                                    sections.join(", ")
                                ),
                            })
                            .unwrap_or_default()
                        }
                    }
                } else {
                    // Return TOC + full content
                    let sections = list_h2_sections(&content);
                    let resp = BestPracticeResponse {
                        target: params.target,
                        category: None,
                        content: format!(
                            "## Sections\n{}\n\n---\n\n{}",
                            sections
                                .iter()
                                .map(|s| format!("- {s}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            content
                        ),
                    };
                    serde_json::to_string_pretty(&resp).unwrap_or_default()
                }
            }
            Err(e) => serde_json::to_string_pretty(&ErrorResponse {
                error: format!("Failed to read best-practices file: {e}"),
            })
            .unwrap_or_default(),
        }
    }
}

/// `get_document` ツール用に、拡張子に対応する Parser で
/// frontmatter (title/date/topic/tags) を抽出し DocumentResponse を組む。
/// 純粋関数化してテスト可能にしている。
/// `get_document` の最大バイト数。1 MiB を超える文書は `fs::read` による
/// バイト一括読みでのメモリ膨張・レスポンス過大を避けるため拒否する。
pub(crate) const GET_DOCUMENT_MAX_BYTES: u64 = 1024 * 1024;

/// get_document がバイナリ形式で応答する抽出テキストの上限 (1 MiB)。超過分は
/// char 境界で truncate し `DocumentResponse.truncated = true` を立てる (§4.4)。
pub(crate) const EXTRACTED_TEXT_MAX_BYTES: usize = 1024 * 1024;

/// `s` を UTF-8 char 境界を保って最大 `max_bytes` バイトに truncate する。
/// truncate したら `true`、無切り詰めなら `false`。
pub(super) fn truncate_on_char_boundary(s: &mut String, max_bytes: usize) -> bool {
    if s.len() <= max_bytes {
        return false;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    s.truncate(boundary);
    true
}

/// `validate_get_document_path` の結果。各 fail variant に既存の
/// `ErrorResponse` を内蔵することで、caller (`get_document` /
/// `resolve_best_practice_path`) は文言生成や prefix 追加なしで
/// `ErrorResponse` を直接 JSON 化できる (= 既存 5 unit test の
/// `err.error.contains("...")` assertion 完全保持)。
///
/// - `Found(PathBuf)` — 4 段階防御を通過、canonical な絶対パス
/// - `NotFound(ErrorResponse)` — file-not-found / canonicalize-failed /
///   outside-kb / extension-denied / size-exceeded の総称。`get_best_practice`
///   の template loop では「次 template を試す」価値ありと解釈
/// - `Denied(ErrorResponse)` — symlink hit のみ (security event)。
///   `get_best_practice` の template loop では即 break = 攻撃 indicator を
///   surface
#[derive(Debug)]
pub(crate) enum ValidatePathOutcome {
    Found(PathBuf),
    NotFound(ErrorResponse),
    Denied(ErrorResponse),
    /// The path could not be **examined** — a permission error, a device error.
    /// Distinct from `NotFound`, which says the path is not there: this says
    /// the server could not look, and answering "no such document" would send
    /// the caller hunting for a typo that does not exist.
    Unavailable(ErrorResponse),
}

impl ValidatePathOutcome {
    /// The canonical path, or the failure and how it should be reported.
    ///
    /// The mapping lives here rather than at the call site so there is one of
    /// it: `resources/read` picks a JSON-RPC code from the [`LoadFailure`], and
    /// a second copy is how the code and the outcome would come to disagree.
    pub(super) fn into_result(self) -> Result<PathBuf, (LoadFailure, ErrorResponse)> {
        match self {
            Self::Found(p) => Ok(p),
            // Both say something about the path: it is absent, or it is not
            // something this server hands over. Neither says the server failed.
            Self::NotFound(e) | Self::Denied(e) => Err((LoadFailure::NotServed, e)),
            Self::Unavailable(e) => Err((LoadFailure::Internal, e)),
        }
    }
}

/// Whether an I/O error while examining a path means the server could not look,
/// rather than that the path is not there.
///
/// Two kinds say the path is not there. `NotFound` is the obvious one.
/// `NotADirectory` is the same statement about an interior component — on Unix,
/// an indexed `dir/note.md` whose `dir` has since been replaced by a regular
/// file reports that, and the path cannot exist while it holds (codex P2 round
/// 7 on PR #162). Everything else — a permission error, a device error — says
/// the examination failed and tells the caller nothing about the path.
pub(super) fn path_probe_failed(e: &std::io::Error) -> bool {
    !matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// (BU-23) `get_best_practice` の「見つからなかった」応答。
///
/// **`tried` の中身をクライアントへ返さないこと**が本 fn の存在理由。候補パスは
/// `[best_practice].path_templates` から作られるので、そのまま返すと未認証の
/// 呼び出し元にサーバの設定した配置 (存在すら知らないはずのディレクトリ名を含む)
/// を渡すことになる。件数だけあれば「どのテンプレートにも当たらなかった」と
/// 「そもそも未設定」は呼び出し元にも区別できる。実際のパスは operator が
/// `RUST_LOG=grooveseek=debug` で stderr から見る。
pub(super) fn best_practice_not_found_message(target: &str, tried: &[String]) -> String {
    format!(
        "Best-practices document for target '{}' not found ({} template{} tried). \
         Check `[best_practice].path_templates` in groove.toml, or run the server \
         with `RUST_LOG=grooveseek=debug` to see which paths were probed.",
        target,
        tried.len(),
        if tried.len() == 1 { "" } else { "s" }
    )
}

/// `get_document` のパス検証 + size cap。成功時は canonical な絶対パスを返す。
/// 拒否時は `ErrorResponse` を返し、呼び出し側が JSON 化する。
///
/// 防御の順序:
/// 1. **symlink reject** — `canonicalize` の前に拾う必要がある
/// 2. **canonicalize + starts_with(kb_path)** — `..` 抜け道を defeat
/// 3. **extension membership** — indexer と同じ拡張子セットに限定。
///    `.git/config` のように registry に無い拡張子のファイルは読めない
/// 4. **size cap** — RAM-OOM を防ぐ。**どちらの上限を使うかは canonical
///    パスの拡張子から決める** (BU-08 と同じく、3 と同じ情報源を使う)
///
/// (BU-22) 以前は cap の選択だけ呼び出し側が **canonicalize 前のリクエスト
/// パス**の拡張子から行い、membership check は canonical 側を見ていた。両者が
/// 食い違うと上限が入れ替わる。Windows の 8.3 短縮名がまさにそれで、
/// `presentation-deck.pptx` は `PRESEN~1.PPT` になる (この開発機で実測)。
/// 拡張子は 3 文字に切られるので `.pptx`/`.xlsx`/`.docx` はいずれも registry に
/// 無い legacy 拡張子に化け、text 上限 (1 MiB) が binary 上限 (50 MiB) の代わりに
/// 適用される。1 MiB 超の Office 文書が短縮名経由で「File too large」になっていた。
///
/// (BU-08) **`exclude_dirs` はここに効かない**。この fn は `exclude_dirs` を
/// 引数に取っておらず、`.obsidian/note.md` のように「除外ディレクトリ配下だが
/// 拡張子は registry にある」ファイルは `get_document` から読める。
/// `exclude_dirs` の契約は「**索引しない**」であって「読ませない」ではない
/// — 検索には出ないがパスを知っていれば取得できる。kb_path 配下に置いた時点で
/// 読める前提の設計なので、読ませたくないものは kb_path の外に置くこと。
/// `document_in_excluded_dir_is_still_readable` が現契約として pin している。
///
/// (feature-49) **`.grooveignore` も同じくここには効かない**。同じ契約を意図的に
/// 踏襲している: KB に書ける者は `.grooveignore` を消すこともできるので、
/// 木の中に置いたルールがその木を守る境界にはなり得ない。`.grooveignore` に
/// 書いたパスは索引されず `search` にも `get_connection_graph` にも出ないが、
/// パスを知っていれば `get_document` で読める。
/// Which of the two caps applies to `ext` (BU-22).
///
/// (BU-20) Shared, because the number is now needed twice: once by
/// [`validate_get_document_path`], which stats the path, and once by the read
/// that follows it, which enforces the same limit on the handle it reads from.
/// Recomputing it at the second site is how the two would come to disagree.
pub(crate) fn max_bytes_for(
    registry: &Registry,
    ext: &str,
    binary_max_bytes: u64,
    text_max_bytes: u64,
) -> u64 {
    if registry
        .by_extension(ext)
        .map(|p| p.is_binary())
        .unwrap_or(false)
    {
        binary_max_bytes
    } else {
        text_max_bytes
    }
}

pub(crate) fn validate_get_document_path(
    kb_path: &std::path::Path,
    rel_path: &str,
    registry: &Registry,
    text_max_bytes: u64,
    binary_max_bytes: u64,
) -> ValidatePathOutcome {
    let file_path = kb_path.join(rel_path);

    // 1. Symlink reject (canonicalize の前に判定)
    match std::fs::symlink_metadata(&file_path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return ValidatePathOutcome::Denied(ErrorResponse {
                error: "Access denied: symlinks are not allowed.".to_string(),
            });
        }
        // (BU-20) A hard link reaches the same content without being a symlink,
        // and `canonicalize` below cannot help: a hard link has no target, so
        // it canonicalizes to itself, inside the KB. The index refuses these
        // too, but `get_document` is reachable by path without going through
        // the index at all.
        Ok(_) if crate::links::is_multiply_linked(&file_path) => {
            tracing::warn!("{}", crate::links::refusal_reason(&file_path));
            return ValidatePathOutcome::Denied(ErrorResponse {
                // One literal for both moments a hard link can be refused —
                // here and at the read that follows (BU-20).
                error: crate::links::HARD_LINK_DENIED.to_string(),
            });
        }
        Ok(_) => {}
        Err(e) if path_probe_failed(&e) => {
            return ValidatePathOutcome::Unavailable(ErrorResponse {
                error: format!("Failed to examine {rel_path}: {e}"),
            });
        }
        Err(_) => {
            return ValidatePathOutcome::NotFound(ErrorResponse {
                error: format!(
                    "File not found: {rel_path}. Path should be relative to knowledge-base/ (e.g. \"deep-dive/mcp/overview.md\")."
                ),
            });
        }
    }

    // 2. Path traversal prevention
    let canonical = match file_path.canonicalize() {
        Ok(p) => p,
        Err(e) if path_probe_failed(&e) => {
            return ValidatePathOutcome::Unavailable(ErrorResponse {
                error: format!("Failed to resolve {rel_path}: {e}"),
            });
        }
        Err(_) => {
            return ValidatePathOutcome::NotFound(ErrorResponse {
                error: format!(
                    "File not found: {rel_path}. Path should be relative to knowledge-base/ (e.g. \"deep-dive/mcp/overview.md\")."
                ),
            });
        }
    };
    if !canonical.starts_with(kb_path) {
        return ValidatePathOutcome::NotFound(ErrorResponse {
            error: "Access denied: path is outside the knowledge base.".to_string(),
        });
    }

    // 3. Extension membership check
    let ext = canonical.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !registry.has_extension(ext) {
        return ValidatePathOutcome::NotFound(ErrorResponse {
            error: format!(
                "Access denied: extension {ext:?} is not in the indexed parser registry. Allowed: {:?}",
                registry.extensions()
            ),
        });
    }

    // 4. Size cap — chosen from the same `ext` that step 3 just accepted, so
    // the two cannot disagree (BU-22).
    let max_bytes = max_bytes_for(registry, ext, binary_max_bytes, text_max_bytes);
    match std::fs::metadata(&canonical) {
        Ok(meta) if meta.len() > max_bytes => {
            return ValidatePathOutcome::NotFound(ErrorResponse {
                error: format!(
                    "File too large: {} bytes (max {} bytes).",
                    meta.len(),
                    max_bytes
                ),
            });
        }
        Ok(_) => {}
        // Steps 1 and 2 already saw this file, so a failure here is almost
        // always the server's: a permission change, an I/O error. Only a
        // genuine `NotFound` — deleted in between — is the path's own answer.
        Err(e) if path_probe_failed(&e) => {
            return ValidatePathOutcome::Unavailable(ErrorResponse {
                error: format!("Failed to stat file: {e}"),
            });
        }
        Err(e) => {
            return ValidatePathOutcome::NotFound(ErrorResponse {
                error: format!("Failed to stat file: {e}"),
            });
        }
    }

    ValidatePathOutcome::Found(canonical)
}

/// `get_document` ツール用に、拡張子に対応する Parser で `parse_bytes` を呼び、
/// frontmatter + 抽出テキストから DocumentResponse を組む。抽出失敗 (不正 UTF-8 /
/// 暗号化 PDF 等) は `Err` にして handler が既存のエラー応答形式へ流す。
/// 登録されていない拡張子はフォールバックで Markdown parser を使う (pre-feature-20 挙動)。
pub(super) fn build_document_response(
    registry: &Registry,
    path_hint: &str,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<DocumentResponse> {
    let parsed = match registry.by_extension(ext) {
        Some(p) => p.parse_bytes(bytes, path_hint, &[])?,
        None => {
            let s = std::str::from_utf8(bytes)
                .map_err(|e| anyhow::anyhow!("{path_hint}: not valid UTF-8: {e}"))?;
            markdown::parse(s)
        }
    };
    // text 形式: raw_content = ファイル全体 (既存 `content: raw` と一致)。
    // binary 形式: raw_content = 抽出テキスト全体。1 MiB 超は truncate。
    let mut content = parsed.raw_content;
    let truncated = truncate_on_char_boundary(&mut content, EXTRACTED_TEXT_MAX_BYTES);
    Ok(DocumentResponse {
        path: path_hint.to_string(),
        title: parsed.frontmatter.title,
        date: parsed.frontmatter.date,
        topic: parsed.frontmatter.topic,
        tags: parsed.frontmatter.tags,
        content,
        truncated,
    })
}

/// `get_best_practice` のパス解決結果。
#[derive(Debug)]
pub(super) enum ResolveOutcome {
    /// `canonicalize` 済みのファイル絶対パス。
    Found(PathBuf),
    /// どのテンプレートにもマッチしなかった。試行した相対パス列。
    NotFound(Vec<String>),
    /// security event (= symlink hit) で即 break した。`validate_get_document_path`
    /// から bubble up した `ErrorResponse` を内蔵し、handler は文言生成や prefix 追加
    /// なしで `serde_json::to_string_pretty(&err)` で直接 client に返却する。
    Denied(ErrorResponse),
}

/// Best-practice resolver: テンプレート列に `{target}` を置換してファイルを探す。
/// 先頭から順に試し、`validate_get_document_path` の 4 段階防御 (symlink reject /
/// canonicalize+starts_with / extension membership / size cap) を通過した最初の
/// 候補を返す。`kb_path` は呼び出し側で既に canonicalize されている前提
/// (`run_server` / tests で事前処理)。
///
/// fail 種別の挙動 (F-45):
/// - `Found(p)` → 即 return
/// - `NotFound(_)` (file not found / canonicalize failed / outside-kb / extension
///   denied / size exceeded) → 次 template を試行 (err 文言は捨てて `tried` に
///   rel path のみ記録、info leak ゼロ)
/// - `Denied(err)` (symlink hit = security event) → 即 return `ResolveOutcome::Denied(err)`
///   (= 文言保持、template ordering より security event 優先)
pub(super) fn resolve_best_practice_path(
    kb_path: &std::path::Path,
    templates: &[String],
    target: &str,
    registry: &Registry,
    max_bytes: u64,
) -> ResolveOutcome {
    let mut tried: Vec<String> = Vec::new();
    for tmpl in templates {
        let rel = tmpl.replace("{target}", target);
        tried.push(rel.clone());
        // Best-practice templates resolve to prose documents; pass the same cap
        // for both classes so this path keeps the single limit it always had.
        match validate_get_document_path(kb_path, &rel, registry, max_bytes, max_bytes) {
            ValidatePathOutcome::Found(p) => return ResolveOutcome::Found(p),
            // One unexaminable candidate must not end the search: the next
            // template may well resolve. This tool answers with a JSON
            // envelope rather than a JSON-RPC code, so the two failures are
            // not distinguishable in its reply anyway.
            ValidatePathOutcome::NotFound(_) | ValidatePathOutcome::Unavailable(_) => continue,
            ValidatePathOutcome::Denied(err) => return ResolveOutcome::Denied(err),
        }
    }
    ResolveOutcome::NotFound(tried)
}

/// Extract the h2 section whose heading contains `category_lower` (case-insensitive).
/// Returns all text from that heading until the next h2 heading.
fn extract_section(content: &str, category: &str) -> Option<String> {
    let cat_lower = category.to_lowercase();
    let mut lines = content.lines();
    let mut found = false;
    let mut section_lines: Vec<&str> = Vec::new();

    for line in &mut lines {
        if line.starts_with("## ") {
            if found {
                // We've hit the next h2 — stop collecting
                break;
            }
            let heading_text = line.trim_start_matches("## ").trim();
            if heading_text.to_lowercase().contains(&cat_lower) {
                found = true;
                section_lines.push(line);
                continue;
            }
        }
        if found {
            section_lines.push(line);
        }
    }

    if found {
        Some(section_lines.join("\n").trim().to_string())
    } else {
        None
    }
}

/// List all h2 headings in the content.
fn list_h2_sections(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|line| line.starts_with("## "))
        .map(|line| line.trim_start_matches("## ").trim().to_string())
        .collect()
}
