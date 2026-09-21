//! Build a delta graph from an analysis and the two revisions' import indexes.
//!
//! [`crate::graph`] says what a graph is; this module is the only place that
//! knows where its relationships come from, and it is deliberately the only
//! one: two revisions' indexes, the file statuses the analysis already
//! computed, and the test links [`crate::query::impact`] already resolves.
//!
//! Every edge is decided from the union of the two indexes rather than from the
//! target alone. One revision can show what exists; only two can prove that a
//! relationship was removed, and a delta that cannot prove a removal is not a
//! delta.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    FileStatus,
    graph::{
        Direction, Edge, EdgeStatus, Evidence, Graph, GraphBuilder, Limits, Node, NodeKind,
        NodeStatus, Reached, Relation, Resolution, View, walk,
    },
    imports::ImportIndex,
    query::impact,
    result::{AnalysisResult, FileResult},
};

/// The prefix [`Node::module_id`] puts in front of a path.
const MODULE_PREFIX: &str = "module:";

/// What a graph request asks for, once the caller has validated it.
///
/// These are the values an answer echoes back, not the raw request: roots are
/// paths naming changed files, `depth` and `limits` are already canonical, and
/// `relations` is already restricted to the relations this version resolves. A
/// builder that re-validated them could disagree with the answer's own echo
/// about what was applied.
pub struct Request<'a> {
    /// Repository-relative paths the walk starts from, already validated by the
    /// caller. Empty means every changed file in the analysis.
    pub roots: &'a [String],
    pub direction: Direction,
    pub relations: &'a [Relation],
    pub depth: u32,
    pub view: View,
    pub limits: Limits,
}

/// Build the graph one request describes.
#[must_use]
pub fn build(
    result: &AnalysisResult,
    base: &ImportIndex,
    target: &ImportIndex,
    request: &Request<'_>,
) -> Graph {
    let statuses = changed_statuses(result);
    let roots = root_paths(result, request.roots);
    let mut builder = GraphBuilder::new();
    for path in &roots {
        builder.add_node(module_node(path, &statuses, 0));
        builder.add_root(Node::module_id(path));
    }

    let reached = if request.relations.contains(&Relation::Imports) {
        let ids = roots
            .iter()
            .map(|path| Node::module_id(path))
            .collect::<Vec<_>>();
        walk(&ids, request.direction, request.depth, |id, direction| {
            adjacent(base, target, id, direction)
        })
    } else {
        // A graph of tests alone is still centered on the roots, so the walk is
        // skipped rather than run over a relation it may not follow.
        roots
            .iter()
            .map(|path| (Node::module_id(path), 0))
            .collect()
    };
    for (id, depth) in &reached {
        builder.add_node(module_node(path_of(id), &statuses, *depth));
    }

    if request.relations.contains(&Relation::Imports) {
        add_import_edges(&mut builder, base, target, &reached);
    }
    if request.relations.contains(&Relation::TestedBy) {
        add_test_edges(&mut builder, base, target, &reached, &statuses);
    }

    builder.apply_view(request.view);
    builder.finish(request.limits)
}

/// The paths the walk starts from: what the caller named, or the changed set.
///
/// A named root is a path the caller validated against the analysis, so both
/// cases produce roots of the same kind; deriving them only when none was named
/// is what makes "no root" mean "centered on the change".
fn root_paths(result: &AnalysisResult, roots: &[String]) -> Vec<String> {
    if !roots.is_empty() {
        return roots.to_vec();
    }
    result.files.iter().filter_map(changed_path).collect()
}

/// The path the analysis reports a changed file under.
///
/// The target side is the revision a reader is looking at, so a renamed file is
/// the node at its new path; a file with no target side was deleted, and its
/// base path is the only one it has.
fn changed_path(file: &FileResult) -> Option<String> {
    file.target_path
        .as_deref()
        .or(file.base_path.as_deref())
        .map(str::to_owned)
}

/// The node status the analysis gave every path in the diff.
///
/// Status comes from the analysis rather than from index membership: a file
/// present in both revisions but modified is `modified`, which membership alone
/// could never say. A path the diff does not contain is `unchanged`.
fn changed_statuses(result: &AnalysisResult) -> BTreeMap<String, NodeStatus> {
    result
        .files
        .iter()
        .filter_map(|file| Some((changed_path(file)?, node_status(file.status))))
        .collect()
}

/// How one file status reads as a node status.
///
/// A renamed or binary file is one the comparison touched: the graph reports
/// that it changed, and which kind of change it was stays the analysis'
/// business.
fn node_status(status: FileStatus) -> NodeStatus {
    match status {
        FileStatus::Added => NodeStatus::Added,
        FileStatus::Deleted => NodeStatus::Removed,
        FileStatus::Modified | FileStatus::Renamed | FileStatus::Binary => NodeStatus::Modified,
    }
}

/// One module node, which is every node this stage builds.
fn module_node(path: &str, statuses: &BTreeMap<String, NodeStatus>, depth: u32) -> Node {
    Node {
        id: Node::module_id(path),
        key: String::new(),
        label: Node::basename(path),
        kind: NodeKind::Module,
        path: path.to_owned(),
        status: statuses.get(path).copied().unwrap_or(NodeStatus::Unchanged),
        depth,
    }
}

/// The path inside a module node identity.
///
/// The walk speaks identities, and every identity in this stage is a module, so
/// undoing [`Node::module_id`] is all an index lookup needs.
fn path_of(id: &str) -> &str {
    id.strip_prefix(MODULE_PREFIX).unwrap_or(id)
}

/// What lies one hop from a node, over the union of both revisions.
///
/// The union is what makes a removal visible: a relationship that exists only
/// in the base revision is still a hop the walk can take, so the module it
/// leads to is in the graph and the edge that reached it is reported as
/// removed. Walking the target alone would drop it without saying so.
fn adjacent(
    base: &ImportIndex,
    target: &ImportIndex,
    id: &str,
    direction: Direction,
) -> Vec<String> {
    let path = path_of(id);
    let mut neighbors = Vec::new();
    if direction.follows_upstream() {
        neighbors.extend(
            base.importers(path)
                .union(target.importers(path))
                .map(|importer| Node::module_id(importer)),
        );
    }
    if direction.follows_downstream() {
        neighbors.extend(
            base.dependencies(path)
                .union(target.dependencies(path))
                .map(|imported| Node::module_id(imported)),
        );
    }
    neighbors
}

/// Add an `imports` edge for every ordered pair of reached modules the union of
/// the two revisions resolves.
///
/// Both endpoints must be in the reached set: an edge to a module outside the
/// walk would show a relationship the request asked not to follow. Asking each
/// reached module for its dependencies visits exactly those pairs, without
/// scanning every pair of reached nodes.
fn add_import_edges(
    builder: &mut GraphBuilder,
    base: &ImportIndex,
    target: &ImportIndex,
    reached: &Reached,
) {
    let paths = reached
        .keys()
        .map(|id| path_of(id))
        .collect::<BTreeSet<_>>();
    for from in paths.iter().copied() {
        for to in base.dependencies(from).union(target.dependencies(from)) {
            if !paths.contains(to.as_str()) {
                continue;
            }
            let Some(status) = EdgeStatus::from_membership(
                base.dependencies(from).contains(to),
                target.dependencies(from).contains(to),
            ) else {
                continue;
            };
            builder.add_edge(Edge {
                from: Node::module_id(from),
                to: Node::module_id(to),
                relation: Relation::Imports,
                status,
                resolution: Resolution::ResolvedSpecifier,
                evidence: import_evidence(from, to, status, base, target),
            });
        }
    }
}

/// Where the import statement that produced an edge is written, when the
/// revision that has the edge recorded the line.
///
/// An unchanged or added edge is read from the target, the revision a reader is
/// looking at; a removed edge exists only in the base, so the base is the only
/// place its line can come from.
fn import_evidence(
    from: &str,
    to: &str,
    status: EdgeStatus,
    base: &ImportIndex,
    target: &ImportIndex,
) -> Option<Evidence> {
    let line = match status {
        EdgeStatus::Removed => base.import_line(from, to),
        EdgeStatus::Added | EdgeStatus::Unchanged => target.import_line(from, to),
    }?;
    Some(Evidence {
        file: from.to_owned(),
        line,
    })
}

/// Attach the tests each reached module has, from both revisions.
///
/// The links come from [`impact::for_file`] unchanged, so a test this graph
/// shows is the same test a function-detail answer offers for the same module,
/// at the same confidence. Membership is decided per resolution: a test that
/// imported a module in one revision and merely shares its name in the other
/// loses one edge and keeps the other, which is what a reader needs to see.
///
/// No evidence is attached. A name match has no site to point at, and the site
/// of a direct import is the `imports` edge of the test file itself.
fn add_test_edges(
    builder: &mut GraphBuilder,
    base: &ImportIndex,
    target: &ImportIndex,
    reached: &Reached,
    statuses: &BTreeMap<String, NodeStatus>,
) {
    for (id, depth) in reached {
        let path = path_of(id);
        let base_links = test_links(base, path);
        let target_links = test_links(target, path);
        for link in base_links.union(&target_links) {
            let Some(status) =
                EdgeStatus::from_membership(base_links.contains(link), target_links.contains(link))
            else {
                continue;
            };
            let (resolution, test) = link;
            builder.add_node(module_node(test, statuses, depth.saturating_add(1)));
            builder.add_edge(Edge {
                from: Node::module_id(test),
                to: Node::module_id(path),
                relation: Relation::TestedBy,
                status,
                resolution: *resolution,
                evidence: None,
            });
        }
    }
}

/// The tests one module has, as resolution and test path pairs.
///
/// Keyed by both, because the two resolutions are two different claims about
/// one file and each has its own status.
fn test_links(index: &ImportIndex, path: &str) -> BTreeSet<(Resolution, String)> {
    impact::for_file(index, path)
        .related_tests
        .into_iter()
        .map(|test| (resolution_of(test.link), test.file))
        .collect()
}

/// The resolution a test link is reported as.
fn resolution_of(link: impact::TestLink) -> Resolution {
    match link {
        impact::TestLink::Imports => Resolution::TestImportsModule,
        impact::TestLink::Convention => Resolution::TestNameMatchesModule,
    }
}

#[cfg(test)]
mod tests {
    use super::{Request, build};
    use crate::{
        FileStatus,
        graph::{
            DEFAULT_DEPTH, Direction, Edge, EdgeStatus, Graph, Limits, Node, NodeStatus, Relation,
            Resolution, View,
        },
        imports::ImportIndex,
        result::{AnalysisResult, AnalysisSummary, FileResult, RevisionResult, SCHEMA_VERSION},
    };

    fn analysis(files: Vec<FileResult>) -> AnalysisResult {
        AnalysisResult {
            schema_version: SCHEMA_VERSION,
            tool_version: "test".to_owned(),
            repository: "repository".to_owned(),
            base: RevisionResult {
                id: "base".to_owned(),
                display_name: "base".to_owned(),
            },
            target: RevisionResult {
                id: "target".to_owned(),
                display_name: "target".to_owned(),
            },
            summary: AnalysisSummary::default(),
            files,
            diagnostics: Vec::new(),
        }
    }

    /// One changed file: the paths it has in each revision and how it changed.
    fn changed_file(base: Option<&str>, target: Option<&str>, status: FileStatus) -> FileResult {
        FileResult {
            base_path: base.map(str::to_owned),
            target_path: target.map(str::to_owned),
            status,
            language: None,
            is_binary: false,
            added_lines: 0,
            removed_lines: 0,
            hunks: Vec::new(),
            functions: Vec::new(),
            exports_added: Vec::new(),
            exports_removed: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn modified(path: &str) -> FileResult {
        changed_file(Some(path), Some(path), FileStatus::Modified)
    }

    fn request<'a>(roots: &'a [String], relations: &'a [Relation]) -> Request<'a> {
        Request {
            roots,
            direction: Direction::Both,
            relations,
            depth: DEFAULT_DEPTH,
            view: View::Delta,
            limits: Limits::default(),
        }
    }

    fn paths_of(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    fn node<'a>(graph: &'a Graph, path: &str) -> Option<&'a Node> {
        graph.node(&Node::module_id(path))
    }

    fn edge<'a>(graph: &'a Graph, from: &str, to: &str) -> Option<&'a Edge> {
        graph
            .edges()
            .iter()
            .find(|edge| edge.from == Node::module_id(from) && edge.to == Node::module_id(to))
    }

    fn edge_with<'a>(
        graph: &'a Graph,
        from: &str,
        to: &str,
        resolution: Resolution,
    ) -> Option<&'a Edge> {
        graph.edges().iter().find(|edge| {
            edge.from == Node::module_id(from)
                && edge.to == Node::module_id(to)
                && edge.resolution == resolution
        })
    }

    fn node_paths(graph: &Graph) -> Vec<&str> {
        graph
            .nodes()
            .iter()
            .map(|node| node.path.as_str())
            .collect()
    }

    #[test]
    fn an_edges_status_is_its_membership_in_the_two_revisions() {
        let base =
            ImportIndex::from_edges(&[("src/a.ts", "src/b.ts"), ("src/old.ts", "src/a.ts")], &[]);
        let target =
            ImportIndex::from_edges(&[("src/a.ts", "src/b.ts"), ("src/a.ts", "src/new.ts")], &[]);
        let result = analysis(vec![modified("src/a.ts")]);
        let roots = paths_of(&["src/a.ts"]);

        let graph = build(
            &result,
            &base,
            &target,
            &request(&roots, Relation::SUPPORTED),
        );

        assert_eq!(
            edge(&graph, "src/a.ts", "src/b.ts").map(|edge| edge.status),
            Some(EdgeStatus::Unchanged)
        );
        assert_eq!(
            edge(&graph, "src/a.ts", "src/new.ts").map(|edge| edge.status),
            Some(EdgeStatus::Added)
        );
        assert_eq!(
            edge(&graph, "src/old.ts", "src/a.ts").map(|edge| edge.status),
            Some(EdgeStatus::Removed)
        );
        // The fixture indexes carry no line numbers, so no evidence is invented
        // for an edge that a real revision would have a site for.
        assert!(edge(&graph, "src/a.ts", "src/b.ts").is_some_and(|edge| edge.evidence.is_none()));
    }

    #[test]
    fn a_modified_file_is_a_modified_node_although_it_is_in_both_revisions() {
        let index = ImportIndex::from_edges(&[("src/consumer.ts", "src/changed.ts")], &[]);
        let result = analysis(vec![
            modified("src/changed.ts"),
            modified("src/consumer.ts"),
        ]);
        let roots = paths_of(&["src/changed.ts"]);

        let graph = build(
            &result,
            &index,
            &index,
            &request(&roots, Relation::SUPPORTED),
        );

        assert_eq!(
            node(&graph, "src/consumer.ts").map(|node| node.status),
            Some(NodeStatus::Modified)
        );
        assert_eq!(
            edge(&graph, "src/consumer.ts", "src/changed.ts").map(|edge| edge.status),
            Some(EdgeStatus::Unchanged)
        );
    }

    #[test]
    fn upstream_reaches_importers_and_downstream_reaches_dependencies() {
        let index = ImportIndex::from_edges(
            &[
                ("src/app.ts", "src/core.ts"),
                ("src/core.ts", "src/leaf.ts"),
            ],
            &[],
        );
        let result = analysis(vec![modified("src/core.ts")]);
        let roots = paths_of(&["src/core.ts"]);

        let mut upstream = request(&roots, Relation::SUPPORTED);
        upstream.direction = Direction::Upstream;
        let graph = build(&result, &index, &index, &upstream);
        assert_eq!(node_paths(&graph), vec!["src/app.ts", "src/core.ts"]);

        let mut downstream = request(&roots, Relation::SUPPORTED);
        downstream.direction = Direction::Downstream;
        let graph = build(&result, &index, &index, &downstream);
        assert_eq!(node_paths(&graph), vec!["src/core.ts", "src/leaf.ts"]);
    }

    #[test]
    fn a_walk_stops_at_the_requested_depth() {
        let index =
            ImportIndex::from_edges(&[("src/a.ts", "src/b.ts"), ("src/b.ts", "src/c.ts")], &[]);
        let result = analysis(vec![modified("src/a.ts")]);
        let roots = paths_of(&["src/a.ts"]);

        let mut one_hop = request(&roots, Relation::SUPPORTED);
        one_hop.direction = Direction::Downstream;
        let graph = build(&result, &index, &index, &one_hop);
        assert_eq!(node_paths(&graph), vec!["src/a.ts", "src/b.ts"]);

        let mut two_hops = one_hop;
        two_hops.depth = 2;
        let graph = build(&result, &index, &index, &two_hops);
        assert_eq!(node_paths(&graph), vec!["src/a.ts", "src/b.ts", "src/c.ts"]);
    }

    #[test]
    fn a_cycle_keeps_its_closing_edge_and_the_walk_terminates() {
        let index =
            ImportIndex::from_edges(&[("src/a.ts", "src/b.ts"), ("src/b.ts", "src/a.ts")], &[]);
        let result = analysis(vec![modified("src/a.ts")]);
        let roots = paths_of(&["src/a.ts"]);

        let graph = build(
            &result,
            &index,
            &index,
            &request(&roots, Relation::SUPPORTED),
        );

        assert_eq!(node_paths(&graph), vec!["src/a.ts", "src/b.ts"]);
        assert!(edge(&graph, "src/a.ts", "src/b.ts").is_some());
        assert!(edge(&graph, "src/b.ts", "src/a.ts").is_some());
    }

    #[test]
    fn tested_by_edges_carry_the_resolutions_and_confidences_of_their_links() {
        let index = ImportIndex::from_edges(
            &[("src/__tests__/core.spec.ts", "src/core.ts")],
            &["src/__tests__/core.test.ts"],
        );
        let result = analysis(vec![modified("src/core.ts")]);
        let roots = paths_of(&["src/core.ts"]);

        let graph = build(
            &result,
            &index,
            &index,
            &request(&roots, &[Relation::TestedBy]),
        );

        let imports = edge_with(
            &graph,
            "src/__tests__/core.spec.ts",
            "src/core.ts",
            Resolution::TestImportsModule,
        )
        .expect("the test imports the module");
        assert_eq!(imports.relation, Relation::TestedBy);
        assert_eq!(imports.status, EdgeStatus::Unchanged);
        assert!((imports.confidence() - 0.9).abs() < f64::EPSILON);

        let convention = edge_with(
            &graph,
            "src/__tests__/core.test.ts",
            "src/core.ts",
            Resolution::TestNameMatchesModule,
        )
        .expect("the test is named after the module");
        assert!((convention.confidence() - 0.8).abs() < f64::EPSILON);

        // A test is a node one hop past the module it covers.
        let test = node(&graph, "src/__tests__/core.spec.ts").expect("the test is a node");
        assert_eq!(test.depth, 1);
        assert_eq!(test.status, NodeStatus::Unchanged);
    }

    #[test]
    fn relations_without_tested_by_leave_the_tests_out() {
        let index = ImportIndex::from_edges(&[("src/__tests__/core.spec.ts", "src/core.ts")], &[]);
        let result = analysis(vec![modified("src/core.ts")]);
        let roots = paths_of(&["src/core.ts"]);

        let graph = build(
            &result,
            &index,
            &index,
            &request(&roots, &[Relation::Imports]),
        );

        assert!(edge(&graph, "src/__tests__/core.spec.ts", "src/core.ts").is_some());
        assert!(
            graph
                .edges()
                .iter()
                .all(|edge| edge.relation == Relation::Imports)
        );
    }

    #[test]
    fn an_empty_root_set_is_the_changed_files_at_their_target_paths() {
        let index = ImportIndex::from_edges(&[("src/renamed.ts", "src/leaf.ts")], &[]);
        let result = analysis(vec![
            changed_file(
                Some("src/old-name.ts"),
                Some("src/renamed.ts"),
                FileStatus::Renamed,
            ),
            changed_file(None, Some("src/leaf.ts"), FileStatus::Added),
        ]);

        let graph = build(&result, &index, &index, &request(&[], Relation::SUPPORTED));

        assert_eq!(node_paths(&graph), vec!["src/leaf.ts", "src/renamed.ts"]);
        assert_eq!(
            node(&graph, "src/renamed.ts").map(|node| node.status),
            Some(NodeStatus::Modified)
        );
        assert_eq!(
            node(&graph, "src/leaf.ts").map(|node| node.status),
            Some(NodeStatus::Added)
        );
        assert!(graph.is_root(&Node::module_id("src/renamed.ts")));
        assert!(graph.is_root(&Node::module_id("src/leaf.ts")));
    }
}
