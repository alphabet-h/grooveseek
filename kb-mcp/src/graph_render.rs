//! Drawing the connection graph: DOT for Graphviz, and SVG that opens on its own.
//!
//! The walk itself lives in [`crate::graph`]; this module only turns its result
//! into a picture. `json` and `text` show the same nodes, but neither shows the
//! shape — and the shape is the reason the graph exists.
//!
//! **No layout dependency.** A general graph would need one, but this is a tree:
//! [`GraphNode`] carries a single `parent_id`, and the walk's `visited` set means
//! no node is reached twice. Depth becomes the column and sibling order the row,
//! which is both easy to compute and a better fit for a BFS result than a
//! general-purpose layout would be.

use crate::graph::{ConnectionGraph, GraphNode};
use std::collections::HashMap;
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Escaping — one implementation each, used by every label builder
// ---------------------------------------------------------------------------

/// Escape a string for a DOT double-quoted ID.
///
/// The rule is not the one C-like syntax suggests. The DOT grammar says *"in
/// quoted strings, the only escaped character is double-quote"* — a backslash is
/// **not** an escape character to the lexer. The label *renderer*, however, does
/// read `\n`, `\l`, `\N` and friends as directives, so a backslash that reaches
/// it untouched silently becomes a line break or a substitution. Doubling it
/// gives the renderer the literal backslash the source had.
///
/// Control characters are replaced with a space: they have no meaning in a label
/// and a raw newline would end the string.
fn dot_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        // One pass on purpose. Escaping quotes and backslashes in two passes
        // makes the order significant, and the wrong order double-escapes the
        // backslash the first pass just added.
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Escape a string for XML text and attribute values.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Cut to at most `max` characters, appending an ellipsis when it cut.
///
/// **Counts characters, not bytes.** This knowledge base is largely Japanese, so
/// slicing by byte offset would split a code point and panic.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The two lines a node shows, before escaping: its path, and its heading.
///
/// Both renderers call this, so a node cannot describe itself one way in DOT and
/// another way in SVG.
fn node_lines(n: &GraphNode) -> (String, Option<String>) {
    (n.path.clone(), n.heading.as_ref().map(|h| format!("#{h}")))
}

/// One line naming what the walk left out, or `None` when it was complete.
///
/// Both pictures carry it for the reason the text output does: a reader looking
/// only at the drawing would otherwise take it for the whole neighbourhood.
fn truncation_line(g: &ConnectionGraph) -> Option<String> {
    if g.truncation.is_empty() {
        return None;
    }
    let parts: Vec<String> = g
        .truncation
        .iter()
        .map(|t| format!("{} (limit {})", t.reason, t.limit))
        .collect();
    Some(format!("truncated: {}", parts.join(", ")))
}

/// Fill colour per BFS depth. Deeper than the palette reuses the last entry
/// rather than wrapping, so "further away" never looks like "back at the start".
const DEPTH_FILLS: [&str; 4] = ["#c9ddff", "#dbe8ff", "#eaf1ff", "#f5f8ff"];

fn depth_fill(depth: u32) -> &'static str {
    DEPTH_FILLS[(depth as usize).min(DEPTH_FILLS.len() - 1)]
}

// ---------------------------------------------------------------------------
// DOT
// ---------------------------------------------------------------------------

/// Render the graph as a Graphviz DOT program.
pub fn to_dot(g: &ConnectionGraph) -> String {
    let mut s = String::new();
    writeln!(s, "digraph kb_mcp_graph {{").unwrap();
    // Left to right: depth then reads along the page the way the walk expanded.
    writeln!(s, "  rankdir=LR;").unwrap();

    let mut caption = format!(
        "connection graph from {}\\nnodes={} depth={} knn_queries={}",
        dot_escape(&g.start_path),
        g.stats.total_nodes,
        g.stats.max_depth_reached,
        g.stats.knn_queries
    );
    if let Some(t) = truncation_line(g) {
        let _ = write!(caption, "\\n{}", dot_escape(&t));
    }
    writeln!(
        s,
        "  graph [fontname=\"sans-serif\", labelloc=\"t\", label=\"{caption}\"];"
    )
    .unwrap();
    writeln!(
        s,
        "  node [shape=box, style=\"rounded,filled\", color=\"#5b7fb9\", fontname=\"sans-serif\", fontsize=10];"
    )
    .unwrap();
    writeln!(
        s,
        "  edge [color=\"#8899aa\", fontname=\"sans-serif\", fontsize=9];"
    )
    .unwrap();

    for n in &g.nodes {
        let (path, heading) = node_lines(n);
        // `\n` here is the two characters Graphviz reads as a line break, added
        // after escaping so it is not itself escaped.
        let label = match heading {
            Some(h) => format!("{}\\n{}", dot_escape(&path), dot_escape(&h)),
            None => dot_escape(&path),
        };
        writeln!(
            s,
            "  \"{}\" [label=\"{}\", fillcolor=\"{}\"];",
            n.node_id,
            label,
            depth_fill(n.depth)
        )
        .unwrap();
    }

    for n in &g.nodes {
        if let Some(p) = n.parent_id {
            writeln!(
                s,
                "  \"{}\" -> \"{}\" [label=\"{:.2}\"];",
                p, n.node_id, n.score
            )
            .unwrap();
        }
    }

    writeln!(s, "}}").unwrap();
    s
}

// ---------------------------------------------------------------------------
// SVG
// ---------------------------------------------------------------------------

const BOX_W: f32 = 260.0;
const BOX_H: f32 = 44.0;
const H_GAP: f32 = 70.0;
const V_GAP: f32 = 14.0;
const MARGIN: f32 = 20.0;
/// Room above the drawing for the caption.
const HEADER_H: f32 = 46.0;
/// Characters per label line at `BOX_W`, chosen so a proportional sans-serif at
/// 11px stays inside the box for Latin text. CJK is wider and can reach the
/// edge; that is a cosmetic overflow, not a broken picture.
const LABEL_CHARS: usize = 38;

/// Where every node sits, and how big the drawing came out.
struct Layout {
    /// Indexed like `ConnectionGraph::nodes`.
    x: Vec<f32>,
    y: Vec<f32>,
    width: f32,
    height: f32,
}

/// Place the tree: depth chooses the column, and a node sits centred on its
/// children, with leaves stacked in the order they were walked.
///
/// A forest is fine — every node whose parent is absent starts its own row
/// group, which is what a multi-seed walk produces.
fn layout_tree(g: &ConnectionGraph) -> Layout {
    let n = g.nodes.len();
    let mut x = vec![MARGIN; n];
    let mut y = vec![MARGIN + HEADER_H; n];
    if n == 0 {
        return Layout {
            x,
            y,
            width: MARGIN * 2.0,
            height: MARGIN * 2.0 + HEADER_H,
        };
    }

    let id_to_idx: HashMap<usize, usize> = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.node_id, i))
        .collect();

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();
    for (i, node) in g.nodes.iter().enumerate() {
        match node.parent_id.and_then(|p| id_to_idx.get(&p)) {
            // A parent that is not in the node list would otherwise drop the
            // whole subtree off the drawing; treat it as a root instead.
            Some(&p) if p != i => children[p].push(i),
            _ => roots.push(i),
        }
    }

    let mut cursor = MARGIN + HEADER_H;
    for &r in &roots {
        assign_y(r, &children, &mut cursor, &mut y);
    }

    let mut max_x: f32 = 0.0;
    let mut max_y: f32 = 0.0;
    for (i, node) in g.nodes.iter().enumerate() {
        x[i] = MARGIN + node.depth as f32 * (BOX_W + H_GAP);
        max_x = max_x.max(x[i] + BOX_W);
        max_y = max_y.max(y[i] + BOX_H);
    }

    Layout {
        x,
        y,
        width: max_x + MARGIN,
        height: max_y + MARGIN,
    }
}

/// Post-order y assignment: leaves take the next free row, parents take the
/// midpoint of their first and last child.
fn assign_y(idx: usize, children: &[Vec<usize>], cursor: &mut f32, y: &mut [f32]) -> f32 {
    let kids = &children[idx];
    if kids.is_empty() {
        let here = *cursor;
        *cursor += BOX_H + V_GAP;
        y[idx] = here;
        return here;
    }
    let first = assign_y(kids[0], children, cursor, y);
    let mut last = first;
    for &k in &kids[1..] {
        last = assign_y(k, children, cursor, y);
    }
    let here = (first + last) / 2.0;
    y[idx] = here;
    here
}

/// Render the graph as a standalone SVG.
///
/// It references no external font, stylesheet or image, because opening it
/// without any other tool installed is the whole reason this format exists.
pub fn to_svg(g: &ConnectionGraph) -> String {
    let l = layout_tree(g);
    let mut s = String::new();
    writeln!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {:.0} {:.0}\" width=\"{:.0}\" height=\"{:.0}\">",
        l.width, l.height, l.width, l.height
    )
    .unwrap();
    writeln!(s, "<rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>").unwrap();

    writeln!(
        s,
        "<text x=\"{:.0}\" y=\"28\" font-family=\"sans-serif\" font-size=\"14\" fill=\"#222222\">{}</text>",
        MARGIN,
        xml_escape(&format!(
            "connection graph from {} — {} nodes, depth {}",
            g.start_path, g.stats.total_nodes, g.stats.max_depth_reached
        ))
    )
    .unwrap();
    if let Some(t) = truncation_line(g) {
        writeln!(
            s,
            "<text x=\"{:.0}\" y=\"44\" font-family=\"sans-serif\" font-size=\"11\" fill=\"#a04040\">{}</text>",
            MARGIN,
            xml_escape(&t)
        )
        .unwrap();
    }

    let id_to_idx: HashMap<usize, usize> = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.node_id, i))
        .collect();

    // Edges first so the boxes paint over their ends.
    for (i, n) in g.nodes.iter().enumerate() {
        let Some(pi) = n.parent_id.and_then(|p| id_to_idx.get(&p)).copied() else {
            continue;
        };
        let (x1, y1) = (l.x[pi] + BOX_W, l.y[pi] + BOX_H / 2.0);
        let (x2, y2) = (l.x[i], l.y[i] + BOX_H / 2.0);
        let mid = (x1 + x2) / 2.0;
        writeln!(
            s,
            "<path d=\"M {x1:.1} {y1:.1} C {mid:.1} {y1:.1}, {mid:.1} {y2:.1}, {x2:.1} {y2:.1}\" fill=\"none\" stroke=\"#8899aa\" stroke-width=\"1.2\"/>"
        )
        .unwrap();
        writeln!(
            s,
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"sans-serif\" font-size=\"9\" fill=\"#667788\">{:.2}</text>",
            mid,
            (y1 + y2) / 2.0 - 3.0,
            n.score
        )
        .unwrap();
    }

    for (i, n) in g.nodes.iter().enumerate() {
        let (path, heading) = node_lines(n);
        writeln!(
            s,
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.0}\" height=\"{:.0}\" rx=\"6\" fill=\"{}\" stroke=\"#5b7fb9\" stroke-width=\"1\"/>",
            l.x[i],
            l.y[i],
            BOX_W,
            BOX_H,
            depth_fill(n.depth)
        )
        .unwrap();
        writeln!(
            s,
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"#12203a\">{}</text>",
            l.x[i] + 8.0,
            l.y[i] + 18.0,
            xml_escape(&truncate_chars(&path, LABEL_CHARS))
        )
        .unwrap();
        if let Some(h) = heading {
            writeln!(
                s,
                "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"#44557a\">{}</text>",
                l.x[i] + 8.0,
                l.y[i] + 33.0,
                xml_escape(&truncate_chars(&h, LABEL_CHARS))
            )
            .unwrap();
        }
    }

    writeln!(s, "</svg>").unwrap();
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{GraphStats, GraphTruncation, TruncationReason};

    fn node(node_id: usize, parent_id: Option<usize>, depth: u32, path: &str) -> GraphNode {
        GraphNode {
            node_id,
            parent_id,
            depth,
            chunk_id: node_id as i64,
            score: 0.75,
            path: path.to_string(),
            heading: None,
            title: None,
            topic: None,
            snippet: String::new(),
        }
    }

    fn graph(nodes: Vec<GraphNode>) -> ConnectionGraph {
        let total = nodes.len();
        ConnectionGraph {
            start_path: "start.md".to_string(),
            truncated: false,
            truncation: Vec::new(),
            nodes,
            stats: GraphStats {
                total_nodes: total,
                max_depth_reached: 1,
                knn_queries: 1,
                duration_ms: 0,
                seeds_used: 1,
            },
        }
    }

    /// The characters that actually occur in this corpus. Measured before
    /// writing the escaper: of 8773 headings, **74 contain a double quote** and
    /// 4 contain a backslash, so both paths run on the first real graph.
    #[test]
    fn test_dot_escape_handles_the_characters_real_headings_contain() {
        assert_eq!(
            dot_escape(r#"OpenAI — GPT-5.4 "Thinking""#),
            r#"OpenAI — GPT-5.4 \"Thinking\""#
        );
        // A backslash is *not* an escape character to the DOT lexer, but the
        // label renderer reads `\n` as a line break — so it has to be doubled,
        // or this heading silently becomes two lines.
        assert_eq!(dot_escape(r"a\note"), r"a\\note");
        // Doubling and quoting in one pass: escaping in two passes would turn
        // this into `\\\"` or `\\\\"` depending on the order.
        assert_eq!(dot_escape(r#"\"#), r"\\");
        assert_eq!(dot_escape("日本語の見出し"), "日本語の見出し");
        // A raw newline would end the quoted string.
        assert_eq!(dot_escape("a\nb\tc"), "a b c");
    }

    #[test]
    fn test_xml_escape_covers_every_character_that_breaks_a_document() {
        assert_eq!(
            xml_escape(r#"<tag> & "quote" 'apos'"#),
            "&lt;tag&gt; &amp; &quot;quote&quot; &apos;apos&apos;"
        );
        assert_eq!(xml_escape("日本語"), "日本語");
    }

    /// Byte slicing would panic here: the knowledge base is largely Japanese and
    /// every one of these characters is three bytes.
    #[test]
    fn test_truncate_chars_counts_characters_not_bytes() {
        assert_eq!(truncate_chars("あいうえお", 3), "あい…");
        assert_eq!(truncate_chars("あいうえお", 5), "あいうえお");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert!("あいうえお".len() > 5, "premise: bytes exceed chars");
    }

    /// Pins the emitted shape. A change to the format should be a deliberate
    /// edit to this expectation, not something noticed later in a picture.
    #[test]
    fn test_to_dot_emits_the_expected_program() {
        let mut root = node(0, None, 0, "a.md");
        root.heading = Some("Intro".to_string());
        let child = node(1, Some(0), 1, "b.md");
        let dot = to_dot(&graph(vec![root, child]));

        assert!(dot.starts_with("digraph kb_mcp_graph {\n"), "{dot}");
        assert!(dot.contains("  rankdir=LR;\n"), "{dot}");
        // `r##"…"##`, not `r#"…"#`: the shorter form would end at the `"#` in
        // `fillcolor="#c9ddff"`.
        // The heading keeps its `#` marker, matching how the text output spells
        // `path#heading`.
        assert!(
            dot.contains(r##"  "0" [label="a.md\n#Intro", fillcolor="#c9ddff"];"##),
            "{dot}"
        );
        assert!(
            dot.contains(r##"  "1" [label="b.md", fillcolor="#dbe8ff"];"##),
            "{dot}"
        );
        assert!(dot.contains(r#"  "0" -> "1" [label="0.75"];"#), "{dot}");
        assert!(dot.trim_end().ends_with('}'), "{dot}");
    }

    #[test]
    fn test_to_dot_draws_one_edge_per_node_with_a_parent() {
        let nodes = vec![
            node(0, None, 0, "a.md"),
            node(1, Some(0), 1, "b.md"),
            node(2, Some(0), 1, "c.md"),
            node(3, Some(1), 2, "d.md"),
            // A second root, as a multi-seed walk produces.
            node(4, None, 0, "e.md"),
        ];
        let with_parent = nodes.iter().filter(|n| n.parent_id.is_some()).count();
        let dot = to_dot(&graph(nodes));
        assert_eq!(dot.matches(" -> ").count(), with_parent);
    }

    /// A reader looking only at the picture would otherwise take a truncated
    /// walk for the whole neighbourhood.
    #[test]
    fn test_both_renderers_say_when_the_walk_was_cut_short() {
        let mut g = graph(vec![node(0, None, 0, "a.md")]);
        g.truncated = true;
        g.truncation = vec![GraphTruncation {
            reason: TruncationReason::NodeBudget,
            limit: 5,
            detail: "raise --max-nodes".to_string(),
        }];
        assert!(to_dot(&g).contains("truncated: node_budget (limit 5)"));
        assert!(to_svg(&g).contains("truncated: node_budget (limit 5)"));
    }

    #[test]
    fn test_layout_puts_a_parent_between_its_children_and_never_overlaps_siblings() {
        let nodes = vec![
            node(0, None, 0, "root.md"),
            node(1, Some(0), 1, "b.md"),
            node(2, Some(0), 1, "c.md"),
        ];
        let g = graph(nodes);
        let l = layout_tree(&g);

        assert!(l.y[1] < l.y[2], "siblings stack in walk order");
        assert!(
            l.y[2] - l.y[1] >= BOX_H,
            "sibling boxes must not overlap: {} vs {}",
            l.y[1],
            l.y[2]
        );
        assert!(
            (l.y[0] - (l.y[1] + l.y[2]) / 2.0).abs() < f32::EPSILON,
            "the parent sits centred on its children"
        );
        assert!(l.x[0] < l.x[1], "depth chooses the column");
        assert!(
            (l.x[1] - l.x[2]).abs() < f32::EPSILON,
            "same depth, same column"
        );
    }

    /// Several roots is the ordinary shape of a multi-seed walk, not an edge
    /// case: the seed strategy defaults to one node per chunk of the start
    /// document.
    #[test]
    fn test_layout_stacks_a_forest_without_overlap() {
        let nodes = vec![
            node(0, None, 0, "a.md"),
            node(1, None, 0, "b.md"),
            node(2, None, 0, "c.md"),
        ];
        let l = layout_tree(&graph(nodes));
        assert!(l.y[0] < l.y[1] && l.y[1] < l.y[2]);
        assert!(l.y[1] - l.y[0] >= BOX_H);
        assert!(l.height >= l.y[2] + BOX_H);
    }

    /// A parent that is not in the node list must not take its subtree off the
    /// drawing; it becomes a root instead.
    #[test]
    fn test_layout_keeps_nodes_whose_parent_is_missing() {
        let nodes = vec![node(0, None, 0, "a.md"), node(1, Some(99), 1, "orphan.md")];
        let l = layout_tree(&graph(nodes));
        assert!(l.y[1] >= MARGIN + HEADER_H);
        assert!(l.height >= l.y[1] + BOX_H);
    }

    #[test]
    fn test_to_svg_is_self_contained_and_sized_to_its_content() {
        let mut root = node(0, None, 0, "a.md");
        root.heading = Some("Intro".to_string());
        let g = graph(vec![root, node(1, Some(0), 1, "b.md")]);
        let l = layout_tree(&g);
        let svg = to_svg(&g);

        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(
            svg.contains(&format!("viewBox=\"0 0 {:.0} {:.0}\"", l.width, l.height)),
            "the viewBox has to match the computed extent: {svg}"
        );
        // Opening it without anything else installed is the reason this format
        // exists, so nothing may be fetched.
        for external in ["http://", "https://", "<image", "@import", "url("] {
            let allowed = external == "http://"; // the SVG namespace itself
            if !allowed {
                assert!(!svg.contains(external), "{external} must not appear: {svg}");
            }
        }
        assert_eq!(
            svg.matches("<rect").count(),
            3,
            "background plus one box per node"
        );
    }

    #[test]
    fn test_to_svg_escapes_labels_so_a_heading_cannot_break_the_document() {
        let mut n0 = node(0, None, 0, "a<b>.md");
        n0.heading = Some(r#"tom & "jerry""#.to_string());
        let svg = to_svg(&graph(vec![n0]));
        assert!(svg.contains("a&lt;b&gt;.md"), "{svg}");
        assert!(svg.contains("tom &amp; &quot;jerry&quot;"), "{svg}");
        // The raw forms must not survive anywhere in the text nodes.
        assert!(!svg.contains("a<b>.md"), "{svg}");
    }

    #[test]
    fn test_renderers_accept_an_empty_graph() {
        let g = graph(Vec::new());
        assert!(to_dot(&g).contains("digraph"));
        let svg = to_svg(&g);
        assert!(svg.contains("<svg") && svg.contains("</svg>"));
    }
}
