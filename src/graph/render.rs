//! Dependency-diff and Mermaid renderings of one graph.
//!
//! The model decides what a graph contains; these functions only decide how to
//! say it, and they are pure and total so that one graph renders byte for byte
//! the same way every time — the property a caller relies on when it compares
//! two answers or keeps a rendering as evidence.
//!
//! The dependency diff is the compact rendering and the right answer for most
//! changes: one line per relationship, with no topology to reconstruct.
//! Mermaid earns its place only when a reader would otherwise rebuild a shape
//! from a list, so whether to draw is [`crate::graph::recommend`]'s decision
//! and this module simply draws what it is handed.

use std::collections::BTreeSet;

use super::{Edge, EdgeStatus, Graph, Node, NodeKind, NodeStatus, Relation};

/// Longest label a diagram line may carry before it is truncated.
///
/// Labels come from a repository `DiffScope` does not control and each has to fit
/// on one line of a diagram, so the budget is fixed rather than derived from
/// the name. It is the budget `src/languages/typescript.rs` applies to identity
/// segments, so a name shown in a rendering and a name inside an identity run
/// out of room at the same place.
const MAX_LABEL_BYTES: usize = 64;

/// What a truncated label ends with.
const TRUNCATION_MARKER: &str = "...";

/// Every status a `classDef` is emitted for, in the model's own order.
///
/// The block is fixed rather than filtered to the statuses present: a diagram
/// whose header does not change with its contents is easier to compare against
/// another one, and a status never means two different things in two answers.
const STATUSES: [NodeStatus; 4] = [
    NodeStatus::Added,
    NodeStatus::Removed,
    NodeStatus::Modified,
    NodeStatus::Unchanged,
];

/// Render the graph as a dependency diff: one line per edge, marked by status.
///
/// Endpoints are places in the repository rather than labels: a module is its
/// path, so two files sharing a basename are two different files, and a
/// function adds its qualified name to the path, so two functions of one file
/// are two different lines. Lines follow the graph's edge order, so a removed
/// relationship and the added one that replaced it appear together. The result
/// keeps the trailing newline of its last line and an empty graph renders as
/// nothing at all, which is what lets a caller print it unmodified.
#[must_use]
pub fn dependency_diff(graph: &Graph) -> String {
    let mut output = String::new();
    for edge in graph.edges() {
        let (Some(from), Some(to)) = (graph.node(&edge.from), graph.node(&edge.to)) else {
            // A placeholder would name a file that does not exist, and a path
            // is the whole point of the line; omitting it is the only answer
            // that cannot mislead.
            continue;
        };
        let marker = match edge.status {
            EdgeStatus::Removed => "-",
            EdgeStatus::Added => "+",
            EdgeStatus::Unchanged => " ",
        };
        push_line(&mut output, &edge_line(marker, from, to, edge.relation));
    }
    output
}

/// One dependency-diff line: the marker, both endpoints, and the relation when
/// it is not the common one.
///
/// `imports` goes unnamed because a module diff is read as one by default;
/// naming it on every line would be noise, and naming the others keeps a
/// `tested_by` line from reading like an import it is not.
fn edge_line(marker: &str, from: &Node, to: &Node, relation: Relation) -> String {
    match relation {
        Relation::Imports => format!("{marker} {} -> {}", endpoint(from), endpoint(to)),
        other => format!(
            "{marker} {} -[{}]-> {}",
            endpoint(from),
            other.name(),
            endpoint(to)
        ),
    }
}

/// The endpoint a dependency-diff line names.
///
/// A module is its path, because that is the place in the repository a reader
/// looks for. A function is its path and its qualified name, because two
/// functions of one file are two different places and a line naming the file
/// alone would merge them.
fn endpoint(node: &Node) -> String {
    match node.kind {
        NodeKind::Function => format!("{}::{}", node.path, node.label),
        NodeKind::Module | NodeKind::Group => node.path.clone(),
    }
}

/// Render the graph as a Mermaid `flowchart` document.
///
/// The source is what a caller can reproduce; geometry belongs to whatever
/// draws it. So everything here comes from the graph's own order and keys: the
/// status block is fixed, each node's shape is declared where the node first
/// appears and referred to by key after that, and the class assignments repeat
/// the node order, which leaves one place to look for any one node's style.
#[must_use]
pub fn mermaid(graph: &Graph) -> String {
    let mut output = String::new();
    push_line(&mut output, "flowchart LR");
    for status in STATUSES {
        push_line(
            &mut output,
            &format!("    classDef {} {}", status.name(), style(status)),
        );
    }

    let mut declared = BTreeSet::new();
    for edge in graph.edges() {
        let (Some(from), Some(to)) = (graph.node(&edge.from), graph.node(&edge.to)) else {
            continue;
        };
        let from_text = reference(from, &mut declared);
        let to_text = reference(to, &mut declared);
        let arrow = arrow(edge);
        push_line(&mut output, &format!("    {from_text} {arrow} {to_text}"));
    }

    // A node no kept edge touches still belongs in the document: dropping it
    // would let a truncated graph quietly lose the root it was built around,
    // and an isolated node is itself a finding.
    for node in graph.nodes() {
        if declared.insert(node.id.clone()) {
            push_line(&mut output, &format!("    {}", declaration(node)));
        }
    }

    for node in graph.nodes() {
        push_line(
            &mut output,
            &format!("    class {} {}", node.key, node.status.name()),
        );
    }
    output
}

/// A node's shape declaration, or its bare key once it has been declared.
///
/// Mermaid draws one box per declaration, so declaring a node again at every
/// mention would produce either duplicates or a box with no label; the first
/// mention is where the label belongs and every later one points at a box the
/// reader has already seen.
fn reference(node: &Node, declared: &mut BTreeSet<String>) -> String {
    if declared.insert(node.id.clone()) {
        declaration(node)
    } else {
        node.key.clone()
    }
}

/// The declaration of a node: its key, with the label quoted.
fn declaration(node: &Node) -> String {
    format!("{}[\"{}\"]", node.key, label(node))
}

/// The text a node's box shows.
///
/// The status of every node but an unchanged one is part of the label, because
/// the diagram is read by someone who did not build it and a box reading
/// `parse.ts` alone does not say that this change created the file. An
/// unchanged node states nothing, which keeps the labels of the context a
/// diagram carries short.
fn label(node: &Node) -> String {
    if node.status == NodeStatus::Unchanged {
        sanitize(&node.label)
    } else {
        format!("{} · {}", sanitize(&node.label), node.status.name())
    }
}

/// What the arrow between two nodes says about the edge.
///
/// A removed edge keeps the `removed` form even below full confidence: a reader
/// has to know the relationship is gone before knowing how sure the resolver
/// was, and one dashed style for both statements would make "this is gone" and
/// "this may exist" look alike when they are opposites.
///
/// A `re_exports` edge is named, because a module that both imports and
/// forwards another would otherwise draw the same bare arrow twice and say
/// nothing about why. The other relations need no name: `imports` is what a
/// module diagram is read as, and `calls`, `contains`, and `tested_by` join
/// node kinds that already say which relationship it is.
fn arrow(edge: &Edge) -> String {
    if edge.status == EdgeStatus::Removed {
        return "-. \"removed\" .->".to_owned();
    }
    let confidence = edge.confidence();
    if confidence < 1.0 {
        return format!("-. \"{}\" .->", uncertainty(confidence));
    }
    if edge.relation == Relation::ReExports {
        return "-- \"re_exports\" -->".to_owned();
    }
    "-->".to_owned()
}

/// How an edge below full confidence states it.
///
/// Two decimals with trailing zeros trimmed, so an edge reads `~0.9` rather
/// than `~0.90` or the float's own expansion. Confidence is fixed per
/// resolution, so the only values that can appear are the documented ones and
/// this formatting never invents precision a resolution does not have.
fn uncertainty(confidence: f64) -> String {
    let formatted = format!("{confidence:.2}");
    let trimmed = formatted.trim_end_matches('0');
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    format!("~{trimmed}")
}

/// The fixed style a node status is drawn with.
///
/// Colors stay within the vocabulary a reader already has from a diff — green
/// for what the change adds, red for what it removes, amber for what it changes
/// in place, grey for what it leaves alone — so the status of a box is legible
/// before the class names are read.
fn style(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Added => "fill:#e6ffed,stroke:#22863a",
        NodeStatus::Removed => "fill:#ffeef0,stroke:#cb2431",
        NodeStatus::Modified => "fill:#fff5b1,stroke:#b08800",
        NodeStatus::Unchanged => "fill:#f6f8fa,stroke:#d1d5db",
    }
}

/// Reduce a label to the quoted, single-line, bounded text a diagram may carry.
///
/// A label is untrusted input: a path can contain a quote, a newline, or a
/// control character, and Mermaid reads all three as structure while the
/// diagram means none of them. Control characters become spaces, whitespace
/// runs become one space, quotes are dropped, and what is left is truncated on
/// a character boundary — the reductions `src/languages/typescript.rs` applies
/// to identity segments, so a name is abbreviated the same way wherever its
/// budget runs out.
fn sanitize(label: &str) -> String {
    let mut cleaned = String::with_capacity(label.len());
    for character in label.chars() {
        match character {
            '"' => {}
            character if character.is_control() => cleaned.push(' '),
            character => cleaned.push(character),
        }
    }
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(collapsed)
}

/// Cut a label to the diagram's budget on a character boundary.
fn truncate(text: String) -> String {
    if text.len() <= MAX_LABEL_BYTES {
        return text;
    }
    let mut end = MAX_LABEL_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = text[..end].to_owned();
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

fn push_line(output: &mut String, line: &str) {
    output.push_str(line);
    output.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{GraphBuilder, Limits, NodeKind, Resolution};

    fn node(path: &str, status: NodeStatus) -> Node {
        Node {
            id: Node::module_id(path),
            key: String::new(),
            label: Node::basename(path),
            kind: NodeKind::Module,
            path: path.to_owned(),
            range_start: (0, 0),
            status,
            depth: 1,
        }
    }

    /// One function node of `path`, labelled with the qualified name a reader
    /// knows it by.
    fn function_node(path: &str, qualified_name: &str, status: NodeStatus) -> Node {
        Node {
            id: Node::function_id(&format!("{path}#fn:{qualified_name}@target:1:0")),
            key: String::new(),
            label: qualified_name.to_owned(),
            kind: NodeKind::Function,
            path: path.to_owned(),
            range_start: (1, 0),
            status,
            depth: 1,
        }
    }

    fn edge(
        from: &str,
        to: &str,
        status: EdgeStatus,
        relation: Relation,
        resolution: Resolution,
    ) -> Edge {
        Edge {
            from: Node::module_id(from),
            to: Node::module_id(to),
            relation,
            status,
            resolution,
            evidence: None,
        }
    }

    fn graph_of(nodes: &[(&str, NodeStatus)], edges: &[Edge]) -> Graph {
        let mut builder = GraphBuilder::new();
        for (path, status) in nodes {
            builder.add_node(node(path, *status));
        }
        for edge in edges {
            builder.add_edge(edge.clone());
        }
        builder.finish(Limits::default())
    }

    /// A graph of nodes of any kind, which is what a function-rooted answer
    /// delivers.
    fn graph_with(nodes: &[Node], edges: &[Edge]) -> Graph {
        let mut builder = GraphBuilder::new();
        for node in nodes {
            builder.add_node(node.clone());
        }
        for edge in edges {
            builder.add_edge(edge.clone());
        }
        builder.finish(Limits::default())
    }

    /// An edge between two nodes, whatever kind they are.
    fn edge_between(
        from: &Node,
        to: &Node,
        status: EdgeStatus,
        relation: Relation,
        resolution: Resolution,
    ) -> Edge {
        Edge {
            from: from.id.clone(),
            to: to.id.clone(),
            relation,
            status,
            resolution,
            evidence: None,
        }
    }

    /// The fixed `classDef` block every document opens with, derived from the
    /// renderer's own table so a test can assert a whole document without
    /// restating the styles the golden test pins byte for byte.
    fn header() -> String {
        use std::fmt::Write as _;

        let mut header = String::from("flowchart LR\n");
        for status in STATUSES {
            writeln!(header, "    classDef {} {}", status.name(), style(status))
                .expect("writing into a string cannot fail");
        }
        header
    }

    #[test]
    fn the_dependency_diff_marks_each_status_and_names_a_non_import_relation() {
        let graph = graph_of(
            &[
                ("src/cssVars.ts", NodeStatus::Modified),
                ("src/compileStyle.ts", NodeStatus::Unchanged),
                ("src/legacyParser.ts", NodeStatus::Removed),
                ("src/parse.ts", NodeStatus::Added),
                ("src/cssVars.spec.ts", NodeStatus::Added),
            ],
            &[
                edge(
                    "src/compileStyle.ts",
                    "src/cssVars.ts",
                    EdgeStatus::Unchanged,
                    Relation::Imports,
                    Resolution::ResolvedSpecifier,
                ),
                edge(
                    "src/cssVars.ts",
                    "src/legacyParser.ts",
                    EdgeStatus::Removed,
                    Relation::Imports,
                    Resolution::ResolvedSpecifier,
                ),
                edge(
                    "src/cssVars.ts",
                    "src/parse.ts",
                    EdgeStatus::Added,
                    Relation::Imports,
                    Resolution::ResolvedSpecifier,
                ),
                edge(
                    "src/cssVars.spec.ts",
                    "src/cssVars.ts",
                    EdgeStatus::Added,
                    Relation::TestedBy,
                    Resolution::TestImportsModule,
                ),
            ],
        );

        assert_eq!(
            dependency_diff(&graph),
            concat!(
                "  src/compileStyle.ts -> src/cssVars.ts\n",
                "+ src/cssVars.spec.ts -[tested_by]-> src/cssVars.ts\n",
                "- src/cssVars.ts -> src/legacyParser.ts\n",
                "+ src/cssVars.ts -> src/parse.ts\n",
            )
        );
    }

    #[test]
    fn a_graph_without_edges_renders_no_diff_and_no_dangling_placeholder() {
        assert_eq!(dependency_diff(&Graph::default()), "");

        // `finish` cannot deliver an edge whose endpoint it dropped, but a
        // caller can hand one over, and the omission is the contract.
        let mut dangling = Graph::default();
        dangling.nodes.push(node("src/a.ts", NodeStatus::Unchanged));
        dangling.edges.push(edge(
            "src/a.ts",
            "src/gone.ts",
            EdgeStatus::Added,
            Relation::Imports,
            Resolution::ResolvedSpecifier,
        ));
        assert_eq!(dependency_diff(&dangling), "");
        assert!(!mermaid(&dangling).contains("gone.ts"));
    }

    #[test]
    fn a_removed_edge_and_an_uncertain_edge_render_as_different_dashed_forms() {
        let gone = graph_of(
            &[
                ("src/a.ts", NodeStatus::Modified),
                ("src/b.ts", NodeStatus::Removed),
            ],
            &[edge(
                "src/a.ts",
                "src/b.ts",
                EdgeStatus::Removed,
                Relation::Imports,
                Resolution::ResolvedSpecifier,
            )],
        );
        let unsure = graph_of(
            &[
                ("src/a.ts", NodeStatus::Unchanged),
                ("src/b.ts", NodeStatus::Added),
            ],
            &[edge(
                "src/a.ts",
                "src/b.ts",
                EdgeStatus::Added,
                Relation::TestedBy,
                Resolution::TestImportsModule,
            )],
        );

        let removed_line = mermaid(&gone)
            .lines()
            .find(|line| line.contains("-. "))
            .expect("the removed edge is drawn dashed")
            .to_owned();
        let uncertain_line = mermaid(&unsure)
            .lines()
            .find(|line| line.contains("-. "))
            .expect("the uncertain edge is drawn dashed")
            .to_owned();

        assert!(removed_line.contains("-. \"removed\" .->"));
        assert!(uncertain_line.contains("-. \"~0.9\" .->"));
        assert_ne!(removed_line, uncertain_line);
    }

    #[test]
    fn a_re_export_is_told_apart_from_the_import_beside_it() {
        // A barrel module both imports and forwards the module it re-exports,
        // so two bare arrows between one pair of boxes would say nothing about
        // why there are two.
        let graph = graph_of(
            &[
                ("src/index.ts", NodeStatus::Modified),
                ("src/parse.ts", NodeStatus::Unchanged),
            ],
            &[
                edge(
                    "src/index.ts",
                    "src/parse.ts",
                    EdgeStatus::Unchanged,
                    Relation::Imports,
                    Resolution::ResolvedSpecifier,
                ),
                edge(
                    "src/index.ts",
                    "src/parse.ts",
                    EdgeStatus::Unchanged,
                    Relation::ReExports,
                    Resolution::ExportClause,
                ),
            ],
        );

        let arrows = mermaid(&graph)
            .lines()
            .filter(|line| line.contains("-->"))
            .map(str::trim)
            .map(str::to_owned)
            .collect::<Vec<_>>();

        assert_eq!(arrows.len(), 2, "one arrow per edge");
        assert!(
            arrows
                .iter()
                .any(|line| line.contains("-- \"re_exports\" -->")),
            "the forwarding names itself: {arrows:?}"
        );
        assert_ne!(arrows[0], arrows[1]);
    }

    #[test]
    fn a_removed_edge_keeps_the_removed_form_below_full_confidence() {
        // "Gone" is what the reader has to learn first; how sure the resolver
        // was about a relationship that no longer exists is the second fact.
        let graph = graph_of(
            &[
                ("src/a.ts", NodeStatus::Modified),
                ("src/b.ts", NodeStatus::Removed),
            ],
            &[edge(
                "src/a.ts",
                "src/b.ts",
                EdgeStatus::Removed,
                Relation::TestedBy,
                Resolution::TestNameMatchesModule,
            )],
        );

        assert!(mermaid(&graph).contains("-. \"removed\" .->"));
    }

    #[test]
    fn a_full_confidence_edge_states_no_uncertainty() {
        let graph = graph_of(
            &[
                ("src/a.ts", NodeStatus::Added),
                ("src/b.ts", NodeStatus::Unchanged),
            ],
            &[edge(
                "src/a.ts",
                "src/b.ts",
                EdgeStatus::Added,
                Relation::Imports,
                Resolution::ResolvedSpecifier,
            )],
        );

        assert_eq!(
            mermaid(&graph),
            format!(
                "{}    n0[\"a.ts · added\"] --> n1[\"b.ts\"]\n    class n0 added\n    class n1 unchanged\n",
                header()
            )
        );
    }

    #[test]
    fn a_node_declares_its_shape_once_and_is_referenced_by_key_afterwards() {
        let graph = graph_of(
            &[
                ("src/a.ts", NodeStatus::Modified),
                ("src/b.ts", NodeStatus::Unchanged),
                ("src/c.ts", NodeStatus::Unchanged),
            ],
            &[
                edge(
                    "src/a.ts",
                    "src/b.ts",
                    EdgeStatus::Added,
                    Relation::Imports,
                    Resolution::ResolvedSpecifier,
                ),
                edge(
                    "src/c.ts",
                    "src/a.ts",
                    EdgeStatus::Unchanged,
                    Relation::Imports,
                    Resolution::ResolvedSpecifier,
                ),
            ],
        );

        assert_eq!(
            mermaid(&graph),
            format!(
                "{}    n0[\"a.ts · modified\"] --> n1[\"b.ts\"]\n    n2[\"c.ts\"] --> n0\n{}",
                header(),
                "    class n0 modified\n    class n1 unchanged\n    class n2 unchanged\n"
            )
        );
    }

    #[test]
    fn a_node_no_edge_touches_is_still_declared() {
        let mut builder = GraphBuilder::new();
        builder.add_node(node("src/root.ts", NodeStatus::Modified));
        builder.add_root(Node::module_id("src/root.ts"));
        let graph = builder.finish(Limits::default());

        assert_eq!(
            mermaid(&graph),
            format!(
                "{}    n0[\"root.ts · modified\"]\n    class n0 modified\n",
                header()
            )
        );
    }

    #[test]
    fn a_long_hostile_label_becomes_one_quoted_bounded_line() {
        let hostile = format!(
            "{}\n{}\r\n\"{}\"",
            "a".repeat(80),
            "b".repeat(79),
            "c".repeat(36)
        );
        assert_eq!(hostile.len(), 200, "the fixture is a 200-byte label");
        let graph = graph_of(&[(&hostile, NodeStatus::Added)], &[]);

        let declared = mermaid(&graph)
            .lines()
            .find(|line| line.contains("n0["))
            .expect("the node is declared")
            .to_owned();
        assert_eq!(
            declared,
            format!("    n0[\"{}... · added\"]", "a".repeat(64))
        );
        assert_eq!(declared.matches('"').count(), 2);
    }

    #[test]
    fn a_label_is_collapsed_to_one_line_and_keeps_its_status() {
        let graph = graph_of(&[("src/one\ttwo\nthree.ts", NodeStatus::Removed)], &[]);

        assert!(mermaid(&graph).contains("n0[\"one two three.ts · removed\"]"));
    }

    #[test]
    fn the_dependency_diff_separates_two_functions_of_one_file() {
        let module = node("src/a.ts", NodeStatus::Modified);
        let first = function_node("src/a.ts", "first", NodeStatus::Added);
        let second = function_node("src/a.ts", "second", NodeStatus::Added);
        let graph = graph_with(
            &[module.clone(), first.clone(), second.clone()],
            &[
                edge_between(
                    &module,
                    &first,
                    EdgeStatus::Added,
                    Relation::Contains,
                    Resolution::Declaration,
                ),
                edge_between(
                    &module,
                    &second,
                    EdgeStatus::Added,
                    Relation::Contains,
                    Resolution::Declaration,
                ),
            ],
        );

        assert_eq!(
            dependency_diff(&graph),
            concat!(
                "+ src/a.ts -[contains]-> src/a.ts::first\n",
                "+ src/a.ts -[contains]-> src/a.ts::second\n",
            )
        );
    }

    #[test]
    fn a_function_label_keeps_the_sanitization_and_budget_a_module_label_gets() {
        let long = format!("Namespace.{}", "a".repeat(80));
        let graph = graph_with(&[function_node("src/a.ts", &long, NodeStatus::Added)], &[]);
        let declared = mermaid(&graph)
            .lines()
            .find(|line| line.contains("n0["))
            .expect("the function is declared")
            .to_owned();
        assert_eq!(
            declared,
            format!("    n0[\"{}... · added\"]", &long[..MAX_LABEL_BYTES])
        );

        let hostile = "Outer.in\"ner\nname";
        let graph = graph_with(
            &[function_node("src/a.ts", hostile, NodeStatus::Unchanged)],
            &[],
        );
        assert!(mermaid(&graph).contains("n0[\"Outer.inner name\"]"));
    }

    #[test]
    fn rendering_the_same_graph_twice_produces_identical_text() {
        let graph = graph_of(
            &[
                ("src/a.ts", NodeStatus::Modified),
                ("src/b.ts", NodeStatus::Removed),
                ("src/c.ts", NodeStatus::Added),
            ],
            &[
                edge(
                    "src/a.ts",
                    "src/b.ts",
                    EdgeStatus::Removed,
                    Relation::Imports,
                    Resolution::ResolvedSpecifier,
                ),
                edge(
                    "src/a.ts",
                    "src/c.ts",
                    EdgeStatus::Added,
                    Relation::TestedBy,
                    Resolution::TestImportsModule,
                ),
            ],
        );

        assert_eq!(dependency_diff(&graph), dependency_diff(&graph));
        assert_eq!(mermaid(&graph), mermaid(&graph));
    }
}
