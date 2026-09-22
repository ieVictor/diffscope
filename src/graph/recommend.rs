//! Whether a graph is worth drawing, and why.
//!
//! A diagram earns its place when a reader would otherwise have to reconstruct a
//! shape from a list; three relationship lines state that shape exactly, so
//! drawing them would produce a picture nobody needs and teach readers to skip
//! diagrams. Where the line falls is decided by fixed thresholds over
//! quantities the graph already carries, and every threshold that is met
//! contributes one [`Signal`] — never a score, because a score is one opaque
//! number where a list of reasons can be checked against the delivered graph.
//!
//! Signals are evaluated over the delivered graph, which is the graph the reader
//! actually receives: a budget that dropped the node or edge behind a signal
//! must not recommend a diagram that does not contain it. Evaluation is a pure
//! function of that graph, and the reasons come out in one fixed order, so two
//! runs over the same graph cannot disagree about what to say first.
//!
//! One negative signal overrides the positive ones: a graph of fewer than four
//! nodes that does not branch is a shape a dependency diff states as exactly as
//! a picture would, whatever else the graph contains.

use std::collections::{BTreeMap, BTreeSet};

use crate::graph::{Cycle, EdgeStatus, Graph, View};
use crate::query::change_area;

/// Relationships on one side of the root that make that side worth showing.
const MANY: u32 = 3;
/// Relationships on each side of the root that make it a meeting point and a
/// fork at once.
const BOTH_SIDES: u32 = 2;
/// Change areas a graph must span to describe a change rather than a file.
const AREAS: u32 = 2;
/// Node count at which a graph is no longer small enough for
/// [`linear_and_small`] to suppress what it earned.
const SMALL_GRAPH_NODES: u32 = 4;
/// Distinct hop levels changed relationships must sit at for the dependency
/// diff to stop reading as one neighborhood.
const LEVELS: u32 = 2;

/// One criterion's contribution, in structured form.
///
/// The shape matches [`crate::query::risk::Reason`] — a stable code, prose
/// including the measurement, and the measured quantity — so a caller reads a
/// diagram's explanation the way it already reads a risk explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    /// Stable `snake_case` identifier of the criterion that produced this
    /// signal.
    pub code: &'static str,
    /// The criterion's explanation, including the values it measured.
    pub message: String,
    /// The measured quantity behind the signal.
    pub value: u32,
}

/// What to do with one delivered graph, and the signals that decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    /// Whether a diagram is worth rendering.
    pub recommended: bool,
    /// Signals in the fixed criterion order, or exactly the negative one when
    /// it applies.
    pub reasons: Vec<Signal>,
}

/// Decide whether a diagram is worth rendering for this graph.
///
/// A graph is recommended when at least one positive signal applies and the
/// negative signal does not. "The root" is whatever the walk started from — a
/// named file, or every changed file when the request named none — so a graph
/// whose walk would have started nowhere withholds the root-side signals
/// instead of guessing which node was meant.
#[must_use]
pub fn evaluate(graph: &Graph) -> Recommendation {
    if let Some(signal) = linear_and_small(graph) {
        return Recommendation {
            recommended: false,
            reasons: vec![signal],
        };
    }

    let roots = graph
        .roots()
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut inbound = 0_u32;
    let mut outbound = 0_u32;
    let mut changed_inbound = 0_u32;
    let mut changed_outbound = 0_u32;
    let mut changed = 0_u32;
    for edge in graph.edges() {
        let touched = edge.status != EdgeStatus::Unchanged;
        if touched {
            changed += 1;
        }
        if roots.contains(edge.to.as_str()) {
            inbound += 1;
            changed_inbound += u32::from(touched);
        }
        if roots.contains(edge.from.as_str()) {
            outbound += 1;
            changed_outbound += u32::from(touched);
        }
    }

    let mut reasons = Vec::new();
    if inbound >= MANY {
        reasons.push(signal(
            "many_callers",
            format!("{inbound} relationships point at the root"),
            inbound,
        ));
    }
    if outbound >= MANY {
        reasons.push(signal(
            "many_dependencies",
            format!("the root points at {outbound} relationships"),
            outbound,
        ));
    }
    if inbound >= BOTH_SIDES && outbound >= BOTH_SIDES {
        reasons.push(signal(
            "converges_and_branches",
            format!("{inbound} relationships point at the root and {outbound} leave it"),
            inbound.min(outbound),
        ));
    }
    let areas = area_count(graph);
    if areas >= AREAS {
        reasons.push(signal(
            "crosses_areas",
            format!("the graph spans {areas} change areas"),
            areas,
        ));
    }
    reasons.extend(cycle_signals(graph));
    if changed_inbound > 0 && changed_outbound > 0 {
        reasons.push(signal(
            "changed_on_both_sides",
            format!(
                "relationships changed on both sides of the root; {} in total",
                counted(changed, "relationship")
            ),
            changed,
        ));
    }
    let levels = changed_levels(graph);
    if levels >= LEVELS {
        reasons.push(signal(
            "multiple_levels",
            format!(
                "changed relationships appear at {} from the root",
                counted(levels, "hop level")
            ),
            levels,
        ));
    }

    Recommendation {
        recommended: !reasons.is_empty(),
        reasons,
    }
}

/// One criterion's outcome.
///
/// Building every signal in one place keeps the criteria identical in shape, so
/// a caller can walk the list without asking which reason came from which rule.
fn signal(code: &'static str, message: String, value: u32) -> Signal {
    Signal {
        code,
        message,
        value,
    }
}

/// The negative signal, when it applies.
///
/// It fires when the graph holds fewer than four nodes and no node is a branch
/// point: no node has more than one relationship entering it, and none has more
/// than one leaving it. Such a graph is a single node, a straight line, or a
/// loop of at most three nodes, and the dependency diff states any of them as
/// exactly as a picture would. The rule is in terms of branching rather than
/// size alone because a three-node graph where relationships converge on the
/// root and branch from it is exactly the shape a picture exists for.
///
/// The signal overrides every positive one, so it is checked first and the
/// remaining criteria are never measured.
fn linear_and_small(graph: &Graph) -> Option<Signal> {
    let nodes = count(graph.nodes().len());
    if nodes >= SMALL_GRAPH_NODES || branches(graph) {
        return None;
    }
    Some(signal(
        "linear_and_small",
        format!(
            "{} and no branching; a dependency diff is the better answer",
            counted(nodes, "node")
        ),
        nodes,
    ))
}

/// Whether any node forks or merges.
///
/// A node branches when more than one relationship enters it or more than one
/// leaves it; a straight line has neither, which is what makes its picture
/// redundant. A self-loop enters and leaves one node once each, so it does not
/// branch a graph by itself.
fn branches(graph: &Graph) -> bool {
    let mut inbound: BTreeMap<&str, u32> = BTreeMap::new();
    let mut outbound: BTreeMap<&str, u32> = BTreeMap::new();
    for edge in graph.edges() {
        *outbound.entry(edge.from.as_str()).or_default() += 1;
        *inbound.entry(edge.to.as_str()).or_default() += 1;
    }
    inbound
        .values()
        .chain(outbound.values())
        .any(|degree| *degree > 1)
}

/// The number of distinct hop levels the changed relationships sit at.
///
/// A relationship's level is the deeper of its two endpoints: a changed edge
/// between the root and something one hop out sits at the first level, and one
/// between that node and its own neighbor sits at the second. Two levels mean
/// the change reaches past the root's immediate neighborhood, a shape a
/// dependency diff states as a flat list a reader has to reconstruct.
fn changed_levels(graph: &Graph) -> u32 {
    let depth_of = |id: &str| graph.node(id).map_or(0, |node| node.depth);
    count(
        graph
            .edges()
            .iter()
            .filter(|edge| edge.status != EdgeStatus::Unchanged)
            .map(|edge| depth_of(&edge.from).max(depth_of(&edge.to)))
            .collect::<BTreeSet<_>>()
            .len(),
    )
}

/// The number of distinct change areas the graph's node paths span.
///
/// The areas come from [`crate::query::change_area`], so "this change crosses
/// two packages" means the same thing here as it does in a change summary.
fn area_count(graph: &Graph) -> u32 {
    count(
        graph
            .nodes()
            .iter()
            .map(|node| change_area(&node.path))
            .collect::<BTreeSet<_>>()
            .len(),
    )
}

/// What the graph's loops are worth saying.
///
/// Two facts, in the order a reader needs them: that the graph loops at all,
/// measured over the nodes the loops run through, and — the stronger fact —
/// that the change is what closed one of them.
fn cycle_signals(graph: &Graph) -> Vec<Signal> {
    let mut signals = Vec::new();
    let cyclic = graph.cycles().iter().map(Cycle::size).sum::<u32>();
    if cyclic > 0 {
        signals.push(signal(
            "cycle",
            format!(
                "the graph contains a cycle through {}",
                counted(cyclic, "node")
            ),
            cyclic,
        ));
    }
    let introduced = introduced_cycles(graph);
    if introduced > 0 {
        signals.push(signal(
            "cycle_introduced",
            format!(
                "the change closes {}; the target loops where the base did not",
                counted(introduced, "cycle")
            ),
            introduced,
        ));
    }
    signals
}

/// The number of loops the target closes and the base did not.
///
/// Every edge already says which revisions have it, so the two revisions'
/// loops are read out of the one delivered graph by following only the edges
/// each revision has: no second graph is built, and nothing here re-reads a
/// repository. A loop counts as introduced when its exact set of nodes is a
/// loop in the target and is not one in the base, so a cycle both revisions
/// close reports nothing while one the change grew a node into is the new
/// loop it is.
fn introduced_cycles(graph: &Graph) -> u32 {
    let before = graph
        .cycles_in(View::Base)
        .into_iter()
        .map(|cycle| cycle.members)
        .collect::<BTreeSet<_>>();
    count(
        graph
            .cycles_in(View::Target)
            .into_iter()
            .filter(|cycle| !before.contains(&cycle.members))
            .count(),
    )
}

/// A count with its noun, spelled out for the case that can be singular.
///
/// Two signals can measure one — a graph of one node is linear and small, and a
/// self-loop is a cycle over one node — and both nouns it takes are regular, so
/// the plural is one `s`.
fn counted(value: u32, noun: &str) -> String {
    if value == 1 {
        format!("1 {noun}")
    } else {
        format!("{value} {noun}s")
    }
}

/// A count as the graph reports it: never wider than a budget, never panicking.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{
        Edge, GraphBuilder, Limits, Node, NodeKind, NodeStatus, Relation, Resolution,
    };

    fn node(path: &str) -> Node {
        Node {
            id: Node::module_id(path),
            key: String::new(),
            label: Node::basename(path),
            kind: NodeKind::Module,
            path: path.to_owned(),
            range_start: (0, 0),
            status: NodeStatus::Unchanged,
            depth: 1,
            group: None,
        }
    }

    fn import(from: &str, to: &str, status: EdgeStatus) -> Edge {
        Edge {
            from: Node::module_id(from),
            to: Node::module_id(to),
            relation: Relation::Imports,
            status,
            resolution: Resolution::ResolvedSpecifier,
            evidence: None,
        }
    }

    /// Build the graph an answer would deliver: ordered, bounded, and keyed by
    /// the same code that serves the projection.
    fn graph(paths: &[&str], roots: &[&str], edges: &[(&str, &str, EdgeStatus)]) -> Graph {
        let mut builder = GraphBuilder::new();
        for &path in paths {
            builder.add_node(node(path));
        }
        for &root in roots {
            builder.add_root(Node::module_id(root));
        }
        for &(from, to, status) in edges {
            builder.add_edge(import(from, to, status));
        }
        builder.finish(Limits::default())
    }

    /// Build a graph whose nodes sit at the depths a walk would have reached
    /// them at, which is what a level signal measures.
    fn graph_at(
        levels: &[(&str, u32)],
        roots: &[&str],
        edges: &[(&str, &str, EdgeStatus)],
    ) -> Graph {
        let mut builder = GraphBuilder::new();
        for &(path, depth) in levels {
            let mut node = node(path);
            node.depth = depth;
            builder.add_node(node);
        }
        for &root in roots {
            builder.add_root(Node::module_id(root));
        }
        for &(from, to, status) in edges {
            builder.add_edge(import(from, to, status));
        }
        builder.finish(Limits::default())
    }

    fn codes(recommendation: &Recommendation) -> Vec<&'static str> {
        recommendation
            .reasons
            .iter()
            .map(|reason| reason.code)
            .collect()
    }

    fn reason<'a>(recommendation: &'a Recommendation, code: &str) -> &'a Signal {
        recommendation
            .reasons
            .iter()
            .find(|reason| reason.code == code)
            .unwrap_or_else(|| panic!("no {code} signal in {:?}", codes(recommendation)))
    }

    #[test]
    fn many_callers_fires_at_three_inbound_relationships_and_not_at_two() {
        let two = graph(
            &["src/root.ts", "src/a.ts", "src/b.ts"],
            &["src/root.ts"],
            &[
                ("src/a.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/b.ts", "src/root.ts", EdgeStatus::Unchanged),
            ],
        );
        let quiet = evaluate(&two);
        assert!(!quiet.recommended);
        assert!(quiet.reasons.is_empty());

        let three = graph(
            &["src/root.ts", "src/a.ts", "src/b.ts", "src/c.ts"],
            &["src/root.ts"],
            &[
                ("src/a.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/b.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/c.ts", "src/root.ts", EdgeStatus::Unchanged),
            ],
        );
        let loud = evaluate(&three);
        assert!(loud.recommended);
        assert_eq!(codes(&loud), ["many_callers"]);
        assert_eq!(reason(&loud, "many_callers").value, 3);
        assert_eq!(
            reason(&loud, "many_callers").message,
            "3 relationships point at the root"
        );
    }

    #[test]
    fn many_dependencies_fires_at_three_outbound_relationships_and_not_at_two() {
        let two = graph(
            &["src/root.ts", "src/a.ts", "src/b.ts"],
            &["src/root.ts"],
            &[
                ("src/root.ts", "src/a.ts", EdgeStatus::Unchanged),
                ("src/root.ts", "src/b.ts", EdgeStatus::Unchanged),
            ],
        );
        let quiet = evaluate(&two);
        assert!(!quiet.recommended);
        assert!(quiet.reasons.is_empty());

        let three = graph(
            &["src/root.ts", "src/a.ts", "src/b.ts", "src/c.ts"],
            &["src/root.ts"],
            &[
                ("src/root.ts", "src/a.ts", EdgeStatus::Unchanged),
                ("src/root.ts", "src/b.ts", EdgeStatus::Unchanged),
                ("src/root.ts", "src/c.ts", EdgeStatus::Unchanged),
            ],
        );
        let loud = evaluate(&three);
        assert!(loud.recommended);
        assert_eq!(codes(&loud), ["many_dependencies"]);
        assert_eq!(reason(&loud, "many_dependencies").value, 3);
        assert_eq!(
            reason(&loud, "many_dependencies").message,
            "the root points at 3 relationships"
        );
    }

    #[test]
    fn converges_and_branches_needs_two_relationships_on_each_side() {
        let fan = graph(
            &["src/root.ts", "src/a.ts", "src/b.ts", "src/c.ts"],
            &["src/root.ts"],
            &[
                ("src/a.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/b.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/root.ts", "src/c.ts", EdgeStatus::Unchanged),
            ],
        );
        let quiet = evaluate(&fan);
        assert!(!quiet.recommended);
        assert!(quiet.reasons.is_empty());

        let delta = graph(
            &[
                "src/root.ts",
                "src/a.ts",
                "src/b.ts",
                "src/c.ts",
                "src/d.ts",
            ],
            &["src/root.ts"],
            &[
                ("src/a.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/b.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/root.ts", "src/c.ts", EdgeStatus::Unchanged),
                ("src/root.ts", "src/d.ts", EdgeStatus::Unchanged),
            ],
        );
        let loud = evaluate(&delta);
        assert!(loud.recommended);
        assert_eq!(codes(&loud), ["converges_and_branches"]);
        assert_eq!(reason(&loud, "converges_and_branches").value, 2);
        assert_eq!(
            reason(&loud, "converges_and_branches").message,
            "2 relationships point at the root and 2 leave it"
        );

        // The value is the smaller side, so the signal says how lopsided the
        // shape is without hiding the side that earned it.
        let lopsided = graph(
            &[
                "src/root.ts",
                "src/a.ts",
                "src/b.ts",
                "src/c.ts",
                "src/d.ts",
                "src/e.ts",
            ],
            &["src/root.ts"],
            &[
                ("src/a.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/b.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/c.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/root.ts", "src/d.ts", EdgeStatus::Unchanged),
                ("src/root.ts", "src/e.ts", EdgeStatus::Unchanged),
            ],
        );
        let rec = evaluate(&lopsided);
        assert_eq!(codes(&rec), ["many_callers", "converges_and_branches"]);
        assert_eq!(reason(&rec, "converges_and_branches").value, 2);
        assert_eq!(
            reason(&rec, "converges_and_branches").message,
            "3 relationships point at the root and 2 leave it"
        );
    }

    #[test]
    fn crosses_areas_fires_at_two_areas_and_not_at_one() {
        let one = graph(
            &[
                "packages/core/root.ts",
                "packages/core/a.ts",
                "packages/core/b.ts",
            ],
            &["packages/core/root.ts"],
            &[
                (
                    "packages/core/a.ts",
                    "packages/core/root.ts",
                    EdgeStatus::Unchanged,
                ),
                (
                    "packages/core/b.ts",
                    "packages/core/root.ts",
                    EdgeStatus::Unchanged,
                ),
            ],
        );
        let quiet = evaluate(&one);
        assert!(!quiet.recommended);
        assert!(quiet.reasons.is_empty());

        let two = graph(
            &[
                "packages/core/root.ts",
                "packages/web/a.ts",
                "packages/web/b.ts",
            ],
            &["packages/core/root.ts"],
            &[
                (
                    "packages/web/a.ts",
                    "packages/core/root.ts",
                    EdgeStatus::Unchanged,
                ),
                (
                    "packages/web/b.ts",
                    "packages/core/root.ts",
                    EdgeStatus::Unchanged,
                ),
            ],
        );
        let loud = evaluate(&two);
        assert!(loud.recommended);
        assert_eq!(codes(&loud), ["crosses_areas"]);
        assert_eq!(reason(&loud, "crosses_areas").value, 2);
        assert_eq!(
            reason(&loud, "crosses_areas").message,
            "the graph spans 2 change areas"
        );
    }

    #[test]
    fn cycle_counts_each_node_on_a_cycle_once() {
        // Two cycles share the middle node: the value is the three distinct
        // nodes that lie on one, not the four places that are entered.
        let shared = graph(
            &["src/a.ts", "src/b.ts", "src/c.ts"],
            &[],
            &[
                ("src/a.ts", "src/b.ts", EdgeStatus::Unchanged),
                ("src/b.ts", "src/a.ts", EdgeStatus::Unchanged),
                ("src/b.ts", "src/c.ts", EdgeStatus::Unchanged),
                ("src/c.ts", "src/b.ts", EdgeStatus::Unchanged),
            ],
        );
        let rec = evaluate(&shared);
        assert!(rec.recommended);
        assert_eq!(codes(&rec), ["cycle"]);
        assert_eq!(reason(&rec, "cycle").value, 3);
        assert_eq!(
            reason(&rec, "cycle").message,
            "the graph contains a cycle through 3 nodes"
        );

        // A diamond branches on both levels and closes nothing.
        let diamond = graph(
            &["src/a.ts", "src/b.ts", "src/c.ts", "src/d.ts"],
            &[],
            &[
                ("src/a.ts", "src/b.ts", EdgeStatus::Unchanged),
                ("src/a.ts", "src/c.ts", EdgeStatus::Unchanged),
                ("src/b.ts", "src/d.ts", EdgeStatus::Unchanged),
                ("src/c.ts", "src/d.ts", EdgeStatus::Unchanged),
            ],
        );
        let rec = evaluate(&diamond);
        assert!(!rec.recommended);
        assert!(rec.reasons.is_empty());

        // One relationship that leaves a node and returns to it is a cycle of
        // one, and the message says so in the singular.
        let self_loop = graph(
            &["src/a.ts", "src/b.ts", "src/c.ts", "src/d.ts"],
            &[],
            &[
                ("src/a.ts", "src/a.ts", EdgeStatus::Unchanged),
                ("src/b.ts", "src/c.ts", EdgeStatus::Unchanged),
            ],
        );
        let rec = evaluate(&self_loop);
        assert_eq!(codes(&rec), ["cycle"]);
        assert_eq!(reason(&rec, "cycle").value, 1);
        assert_eq!(
            reason(&rec, "cycle").message,
            "the graph contains a cycle through 1 node"
        );
    }

    #[test]
    fn changed_on_both_sides_needs_a_changed_relationship_entering_and_leaving() {
        let inbound_only = graph(
            &["src/root.ts", "src/a.ts", "src/b.ts"],
            &["src/root.ts"],
            &[
                ("src/a.ts", "src/root.ts", EdgeStatus::Added),
                ("src/b.ts", "src/root.ts", EdgeStatus::Unchanged),
            ],
        );
        let quiet = evaluate(&inbound_only);
        assert!(!quiet.recommended);
        assert!(quiet.reasons.is_empty());

        // A changed relationship on each side, plus one elsewhere: the value is
        // the total the dependency diff would state.
        let both = graph(
            &[
                "src/root.ts",
                "src/a.ts",
                "src/b.ts",
                "src/c.ts",
                "src/d.ts",
                "src/e.ts",
            ],
            &["src/root.ts"],
            &[
                ("src/a.ts", "src/root.ts", EdgeStatus::Added),
                ("src/b.ts", "src/root.ts", EdgeStatus::Unchanged),
                ("src/root.ts", "src/c.ts", EdgeStatus::Removed),
                ("src/d.ts", "src/e.ts", EdgeStatus::Added),
            ],
        );
        let loud = evaluate(&both);
        assert!(loud.recommended);
        assert_eq!(codes(&loud), ["changed_on_both_sides"]);
        assert_eq!(reason(&loud, "changed_on_both_sides").value, 3);
        assert_eq!(
            reason(&loud, "changed_on_both_sides").message,
            "relationships changed on both sides of the root; 3 relationships in total"
        );
    }

    #[test]
    fn multiple_levels_fires_only_when_changed_relationships_sit_at_two_levels() {
        // Every changed relationship sits at the first hop, so the dependency
        // diff states the whole change as one flat list.
        let one_hop = graph_at(
            &[
                ("src/root.ts", 0),
                ("src/a.ts", 1),
                ("src/b.ts", 1),
                ("src/c.ts", 1),
            ],
            &["src/root.ts"],
            &[
                ("src/root.ts", "src/a.ts", EdgeStatus::Added),
                ("src/root.ts", "src/b.ts", EdgeStatus::Unchanged),
                ("src/c.ts", "src/root.ts", EdgeStatus::Unchanged),
            ],
        );
        let quiet = evaluate(&one_hop);
        assert!(!quiet.recommended);
        assert!(quiet.reasons.is_empty());

        // One changed relationship sits at the first hop and one a hop past it,
        // which is a shape a flat list makes a reader reconstruct.
        let two_levels = graph_at(
            &[
                ("src/root.ts", 0),
                ("src/a.ts", 1),
                ("src/b.ts", 2),
                ("src/c.ts", 1),
            ],
            &["src/root.ts"],
            &[
                ("src/root.ts", "src/a.ts", EdgeStatus::Added),
                ("src/a.ts", "src/b.ts", EdgeStatus::Added),
                ("src/c.ts", "src/root.ts", EdgeStatus::Unchanged),
            ],
        );
        let loud = evaluate(&two_levels);
        assert!(loud.recommended);
        assert_eq!(codes(&loud), ["multiple_levels"]);
        assert_eq!(reason(&loud, "multiple_levels").value, 2);
        assert_eq!(
            reason(&loud, "multiple_levels").message,
            "changed relationships appear at 2 hop levels from the root"
        );

        // The negative signal stays authoritative: three nodes in a line earn
        // the levels and still do not earn a picture.
        let small = graph_at(
            &[("src/root.ts", 0), ("src/a.ts", 1), ("src/b.ts", 2)],
            &["src/root.ts"],
            &[
                ("src/root.ts", "src/a.ts", EdgeStatus::Added),
                ("src/a.ts", "src/b.ts", EdgeStatus::Added),
            ],
        );
        let rec = evaluate(&small);
        assert!(!rec.recommended);
        assert_eq!(codes(&rec), ["linear_and_small"]);
    }

    #[test]
    fn linear_and_small_overrides_the_signals_a_three_node_chain_earns() {
        let lone = evaluate(&graph(&["src/only.ts"], &["src/only.ts"], &[]));
        assert!(!lone.recommended);
        assert_eq!(codes(&lone), ["linear_and_small"]);
        assert_eq!(reason(&lone, "linear_and_small").value, 1);
        assert_eq!(
            reason(&lone, "linear_and_small").message,
            "1 node and no branching; a dependency diff is the better answer"
        );

        // The chain spans three areas, so `crosses_areas` is earned; the shape
        // is still a straight line, and the negative signal keeps it.
        let chain = graph(
            &["packages/a/one.ts", "apps/b/two.ts", "crates/c/three.ts"],
            &["apps/b/two.ts"],
            &[
                ("packages/a/one.ts", "apps/b/two.ts", EdgeStatus::Unchanged),
                ("apps/b/two.ts", "crates/c/three.ts", EdgeStatus::Unchanged),
            ],
        );
        let rec = evaluate(&chain);
        assert!(!rec.recommended);
        assert_eq!(codes(&rec), ["linear_and_small"]);
        assert_eq!(reason(&rec, "linear_and_small").value, 3);
        assert_eq!(
            reason(&rec, "linear_and_small").message,
            "3 nodes and no branching; a dependency diff is the better answer"
        );

        // One node past the threshold the same line recommends a picture, so
        // the override is the size, not the signal it suppressed.
        let longer = graph(
            &[
                "packages/a/one.ts",
                "apps/b/two.ts",
                "crates/c/three.ts",
                "services/d/four.ts",
            ],
            &["apps/b/two.ts"],
            &[
                ("packages/a/one.ts", "apps/b/two.ts", EdgeStatus::Unchanged),
                ("apps/b/two.ts", "crates/c/three.ts", EdgeStatus::Unchanged),
                (
                    "crates/c/three.ts",
                    "services/d/four.ts",
                    EdgeStatus::Unchanged,
                ),
            ],
        );
        let rec = evaluate(&longer);
        assert!(rec.recommended);
        assert_eq!(codes(&rec), ["crosses_areas"]);
        assert_eq!(reason(&rec, "crosses_areas").value, 4);
    }

    #[test]
    fn a_graph_with_no_edges_recommends_nothing() {
        let rec = evaluate(&graph(
            &["src/a.ts", "src/b.ts", "src/c.ts", "src/d.ts"],
            &["src/a.ts"],
            &[],
        ));
        assert!(!rec.recommended);
        assert!(rec.reasons.is_empty());
    }

    #[test]
    fn reasons_are_emitted_in_the_fixed_criterion_order() {
        // One graph earns every signal at once: three callers converge on the
        // root, three dependencies branch from it, one added caller closes a
        // cycle through the root that the base did not have, the paths span
        // three areas, and the change altered a relationship on each side.
        let graph = graph(
            &[
                "packages/a/root.ts",
                "packages/b/one.ts",
                "packages/b/two.ts",
                "packages/b/three.ts",
                "packages/c/four.ts",
                "packages/c/five.ts",
                "packages/c/six.ts",
            ],
            &["packages/a/root.ts"],
            &[
                ("packages/b/one.ts", "packages/a/root.ts", EdgeStatus::Added),
                (
                    "packages/b/two.ts",
                    "packages/a/root.ts",
                    EdgeStatus::Unchanged,
                ),
                (
                    "packages/b/three.ts",
                    "packages/a/root.ts",
                    EdgeStatus::Unchanged,
                ),
                (
                    "packages/a/root.ts",
                    "packages/c/four.ts",
                    EdgeStatus::Unchanged,
                ),
                (
                    "packages/a/root.ts",
                    "packages/c/five.ts",
                    EdgeStatus::Unchanged,
                ),
                ("packages/a/root.ts", "packages/c/six.ts", EdgeStatus::Added),
                (
                    "packages/c/four.ts",
                    "packages/b/one.ts",
                    EdgeStatus::Unchanged,
                ),
            ],
        );
        let rec = evaluate(&graph);
        assert!(rec.recommended);
        assert_eq!(
            codes(&rec),
            [
                "many_callers",
                "many_dependencies",
                "converges_and_branches",
                "crosses_areas",
                "cycle",
                "cycle_introduced",
                "changed_on_both_sides",
            ]
        );
        assert_eq!(
            rec.reasons
                .iter()
                .map(|signal| signal.value)
                .collect::<Vec<_>>(),
            [3, 3, 3, 3, 3, 1, 2]
        );
    }
}
