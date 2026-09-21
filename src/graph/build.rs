//! Build a delta graph from an analysis, the two revisions' import indexes, and
//! the call sites the analysis collected per function.
//!
//! [`crate::graph`] says what a graph is; this module is the only place that
//! knows where its relationships come from, and it is deliberately the only
//! one: two revisions' indexes, the file statuses and call sites the analysis
//! already computed, and the test links [`crate::query::impact`] already
//! resolves.
//!
//! Every edge is decided from the union of the two indexes rather than from the
//! target alone. One revision can show what exists; only two can prove that a
//! relationship was removed, and a delta that cannot prove a removal is not a
//! delta. Local calls are decided the same way, from the two sides' call sites
//! rather than from one revision's.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    FileStatus,
    analysis::FunctionChangeStatus,
    graph::{
        Direction, Edge, EdgeStatus, Evidence, Graph, GraphBuilder, Limits, Node, NodeKind,
        NodeStatus, Reached, Relation, Resolution, View, walk,
    },
    imports::ImportIndex,
    languages::CallSite,
    query::{self, impact},
    result::{AnalysisResult, FileResult, FunctionResult},
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
    /// The identity of the function the graph is rooted at, already validated
    /// by the caller against the analysis, or `None` for a graph rooted at
    /// [`Request::roots`].
    ///
    /// The two are mutually exclusive: a function root asks what the function
    /// calls and what calls it, and a file root asks the module question
    /// instead.
    pub function_root: Option<&'a str>,
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
    if let Some(function_id) = request.function_root {
        return function_graph(result, function_id, request);
    }

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

/// Build the graph a function root describes.
///
/// The graph is the function, the local calls it takes part in on either side
/// of the change, and the module that declares it. Local means what
/// [`CallSite`] records: a call whose callee is a plain identifier declared in
/// the same file. Each side is resolved against the names that side declares,
/// so a call only one revision wrote is one edge with one status, and the walk
/// follows both revisions' edges, so a removed call is as visible as an added
/// one.
///
/// The module's own `imports` and `tested_by` relationships are deliberately
/// not walked. They are the file-rooted answer; mixing them in would put two
/// notions of "near the root" in one graph, and a caller who wants the module's
/// relationships asks for a file root.
fn function_graph(result: &AnalysisResult, function_id: &str, request: &Request<'_>) -> Graph {
    let Some((path, file, root)) = locate_function(result, function_id) else {
        // The caller validates the identity against the analysis before the
        // request reaches here, so a root that cannot be placed is an empty
        // graph rather than a panic.
        return GraphBuilder::new().finish(request.limits);
    };

    let functions = file.functions.as_slice();
    // The published identity of every record, computed once: a file can hold
    // thousands of call sites, and formatting an identity per site would repeat
    // work the analysis already did.
    let mut node_ids: BTreeMap<&str, String> = BTreeMap::new();
    let mut by_id: BTreeMap<String, &FunctionResult> = BTreeMap::new();
    for function in functions {
        let id = function_node_id(file, function);
        node_ids.insert(function.id.as_str(), id.clone());
        by_id.insert(id, function);
    }
    let base_names = declared_names(functions, |function| function.base_range.is_some());
    let target_names = declared_names(functions, |function| function.target_range.is_some());
    let calls = local_calls(functions, &node_ids, &base_names, &target_names);
    let (outgoing, incoming) = call_adjacency(&calls);
    let root_id = function_node_id(file, root);

    let mut builder = GraphBuilder::new();
    builder.add_root(root_id.clone());
    let reached = if request.relations.contains(&Relation::Calls) {
        walk(
            &[root_id],
            request.direction,
            request.depth,
            |id, direction| call_neighbors(&outgoing, &incoming, id, direction),
        )
    } else {
        // A graph that may not follow calls is still centered on the root, so
        // the walk is skipped rather than run over a relation it may not
        // follow.
        BTreeMap::from([(root_id, 0)])
    };
    for (id, depth) in &reached {
        if let Some(function) = by_id.get(id) {
            builder.add_node(function_node(file, &path, function, *depth));
        }
    }

    if request.relations.contains(&Relation::Calls) {
        add_call_edges(&mut builder, &calls, &reached, &path);
    }
    if request.relations.contains(&Relation::Contains) {
        add_contains_edges(
            &mut builder,
            &path,
            &changed_statuses(result),
            &reached,
            &by_id,
        );
    }

    builder.apply_view(request.view);
    builder.finish(request.limits)
}

/// The rooted function, the file it belongs to, and the path the graph names.
///
/// The identity is compared with [`query::function_id`], the same derivation
/// the caller validated the request with, so a graph cannot be rooted at a
/// function that a detail answer would not recognize.
fn locate_function<'a>(
    result: &'a AnalysisResult,
    function_id: &str,
) -> Option<(String, &'a FileResult, &'a FunctionResult)> {
    result.files.iter().find_map(|file| {
        let path = changed_path(file)?;
        let function = file
            .functions
            .iter()
            .find(|function| query::function_id(file, function) == function_id)?;
        Some((path, file, function))
    })
}

/// The identity a function node is addressed by.
///
/// It is [`query::function_id`], the identity listings and detail answers
/// publish, rather than [`FunctionResult::id`], which is an opaque counter
/// local to one analysis. A root is named by the published identity, so the
/// root the caller asked for and the node the walk reaches have to be the same
/// string.
fn function_node_id(file: &FileResult, function: &FunctionResult) -> String {
    Node::function_id(&query::function_id(file, function))
}

/// One function node: the analysis' record, placed where it starts.
///
/// The label is the qualified name, which is what a reader knows the function
/// by, and the position is the target side's when there is one, matching the
/// path the node carries.
fn function_node(file: &FileResult, path: &str, function: &FunctionResult, depth: u32) -> Node {
    let (line, column) = function
        .target_range
        .as_ref()
        .or(function.base_range.as_ref())
        .map_or((0, 0), |range| (range.start_line, range.start_column));
    Node {
        id: function_node_id(file, function),
        key: String::new(),
        label: function.qualified_name.clone(),
        kind: NodeKind::Function,
        path: path.to_owned(),
        range_start: (line, column),
        status: function_status(function.status),
        depth,
    }
}

/// How a function status reads as a node status.
///
/// The four states mean at function granularity what they mean at file
/// granularity, so the mapping is one to one.
fn function_status(status: FunctionChangeStatus) -> NodeStatus {
    match status {
        FunctionChangeStatus::Added => NodeStatus::Added,
        FunctionChangeStatus::Removed => NodeStatus::Removed,
        FunctionChangeStatus::Modified => NodeStatus::Modified,
        FunctionChangeStatus::Unchanged => NodeStatus::Unchanged,
    }
}

/// The declarations one revision side has, by the name a call would use.
///
/// The name is the leaf of the qualified name, which is what a plain-identifier
/// call writes. A name two definitions share on one side maps to `None`: two
/// same-named functions in one file are the ambiguity the analysis already
/// reports, and a call graph that picked one of them arbitrarily would be worse
/// than one that says nothing.
fn declared_names<'a>(
    functions: &'a [FunctionResult],
    present: fn(&FunctionResult) -> bool,
) -> BTreeMap<&'a str, Option<&'a FunctionResult>> {
    let mut names: BTreeMap<&'a str, Option<&'a FunctionResult>> = BTreeMap::new();
    for function in functions.iter().filter(|function| present(function)) {
        names
            .entry(declared_name(&function.qualified_name))
            .and_modify(|existing| *existing = None)
            .or_insert(Some(function));
    }
    names
}

/// The name a call site would write for a declaration.
fn declared_name(qualified_name: &str) -> &str {
    qualified_name.rsplit('.').next().unwrap_or(qualified_name)
}

/// One call relationship inside the file, and the sides it was seen on.
///
/// Both sides are merged into one entry so that a relationship the two
/// revisions share is one edge with one status, and the earliest line each side
/// recorded is kept as the evidence for it.
#[derive(Debug, Default)]
struct CallEdge {
    in_base: bool,
    in_target: bool,
    base_line: Option<u32>,
    target_line: Option<u32>,
}

/// Every local call relationship of one file, keyed by caller and callee node
/// identity.
type CallEdges = BTreeMap<(String, String), CallEdge>;

/// One direction of a file's call relationships, as node identities.
type Adjacency<'a> = BTreeMap<&'a str, Vec<&'a str>>;

/// Which revision a call site was read from.
#[derive(Clone, Copy)]
enum Side {
    Base,
    Target,
}

/// Every local call relationship in one file, keyed by caller and callee.
///
/// Both revisions' call sites are resolved and merged, so a relationship only
/// one revision has is still an entry: a removed call is as much a fact of the
/// comparison as an added one, and a call both revisions wrote is one entry
/// rather than two.
fn local_calls<'a>(
    functions: &'a [FunctionResult],
    node_ids: &BTreeMap<&'a str, String>,
    base_names: &BTreeMap<&'a str, Option<&'a FunctionResult>>,
    target_names: &BTreeMap<&'a str, Option<&'a FunctionResult>>,
) -> BTreeMap<(String, String), CallEdge> {
    let mut edges: BTreeMap<(String, String), CallEdge> = BTreeMap::new();
    for caller in functions {
        resolve_calls(
            caller,
            &caller.calls_before,
            base_names,
            Side::Base,
            node_ids,
            &mut edges,
        );
        resolve_calls(
            caller,
            &caller.calls_after,
            target_names,
            Side::Target,
            node_ids,
            &mut edges,
        );
    }
    edges
}

/// Record the calls one side of one function wrote.
///
/// A name that side declares twice and a name it declares nowhere both produce
/// nothing: the first is ambiguity, the second is an import, a local, a global,
/// or a parameter. This stage resolves a call exactly or not at all, so neither
/// is guessed at, lowered in confidence, or recorded as a placeholder.
fn resolve_calls<'a>(
    caller: &FunctionResult,
    calls: &[CallSite],
    names: &BTreeMap<&'a str, Option<&'a FunctionResult>>,
    side: Side,
    node_ids: &BTreeMap<&'a str, String>,
    edges: &mut BTreeMap<(String, String), CallEdge>,
) {
    for call in calls {
        let Some(Some(target)) = names.get(call.name.as_str()) else {
            continue;
        };
        let (Some(from), Some(to)) = (
            node_ids.get(caller.id.as_str()),
            node_ids.get(target.id.as_str()),
        ) else {
            continue;
        };
        let entry = edges.entry((from.clone(), to.clone())).or_default();
        match side {
            Side::Base => {
                entry.in_base = true;
                earliest(&mut entry.base_line, call.line);
            }
            Side::Target => {
                entry.in_target = true;
                earliest(&mut entry.target_line, call.line);
            }
        }
    }
}

/// Keep the earliest line a side recorded for one relationship.
///
/// A call written twice is one relationship, and the evidence points at the
/// first place a reader would look for it.
fn earliest(slot: &mut Option<u32>, line: u32) {
    *slot = Some(slot.map_or(line, |existing| existing.min(line)));
}

/// The file's call edges indexed by direction, for the walk.
///
/// The walk asks one node at a time and a file can hold thousands of call
/// sites, so the two directions are indexed once rather than scanned per hop.
fn call_adjacency(calls: &CallEdges) -> (Adjacency<'_>, Adjacency<'_>) {
    let mut outgoing: Adjacency<'_> = BTreeMap::new();
    let mut incoming: Adjacency<'_> = BTreeMap::new();
    for (from, to) in calls.keys() {
        outgoing.entry(from.as_str()).or_default().push(to.as_str());
        incoming.entry(to.as_str()).or_default().push(from.as_str());
    }
    (outgoing, incoming)
}

/// One hop from a function over the file's call edges.
///
/// Both revisions' edges are in the adjacency, so a call only the base revision
/// wrote is still a hop the walk can take and the edge that reached it is
/// reported as removed, the way a removed import is.
fn call_neighbors(
    outgoing: &Adjacency<'_>,
    incoming: &Adjacency<'_>,
    id: &str,
    direction: Direction,
) -> Vec<String> {
    let mut neighbors = Vec::new();
    if direction.follows_downstream() {
        neighbors.extend(
            outgoing
                .get(id)
                .into_iter()
                .flatten()
                .copied()
                .map(str::to_owned),
        );
    }
    if direction.follows_upstream() {
        neighbors.extend(
            incoming
                .get(id)
                .into_iter()
                .flatten()
                .copied()
                .map(str::to_owned),
        );
    }
    neighbors
}

/// Add a `calls` edge for every relationship between functions the walk
/// reached.
///
/// Both endpoints must be reached, for the same reason an import edge needs
/// both: an edge to a function outside the walk would show a relationship the
/// request asked not to follow. A cycle's closing edge is among the reached
/// pairs, so it survives while the walk still terminates.
fn add_call_edges(
    builder: &mut GraphBuilder,
    calls: &BTreeMap<(String, String), CallEdge>,
    reached: &Reached,
    path: &str,
) {
    for ((from, to), call) in calls {
        if !reached.contains_key(from) || !reached.contains_key(to) {
            continue;
        }
        let Some(status) = EdgeStatus::from_membership(call.in_base, call.in_target) else {
            continue;
        };
        // An unchanged or added edge is read from the target, the revision a
        // reader is looking at; a removed edge exists only in the base, so the
        // base is the only place its line can come from.
        let line = match status {
            EdgeStatus::Removed => call.base_line,
            EdgeStatus::Added | EdgeStatus::Unchanged => call.target_line,
        };
        builder.add_edge(Edge {
            from: from.clone(),
            to: to.clone(),
            relation: Relation::Calls,
            status,
            resolution: Resolution::DirectLocalSymbol,
            evidence: line.map(|line| Evidence {
                file: path.to_owned(),
                line,
            }),
        });
    }
}

/// Attach the module to every function the graph includes.
///
/// `contains` is not walked: the module is the container of the functions the
/// walk already reached, so it is attached to each of them rather than used to
/// discover more, and it sits one `contains` hop above the root. The module
/// node carries the status the analysis gave the file, and each edge carries
/// the presence of the function it points at, which is what makes an extracted
/// helper read as an added containment.
///
/// No evidence is attached. The site of a containment is the declaration the
/// function node already carries a position for, so an edge repeating it would
/// say nothing new.
fn add_contains_edges(
    builder: &mut GraphBuilder,
    path: &str,
    statuses: &BTreeMap<String, NodeStatus>,
    reached: &Reached,
    by_id: &BTreeMap<String, &FunctionResult>,
) {
    builder.add_node(module_node(path, statuses, 1));
    for id in reached.keys() {
        let Some(function) = by_id.get(id) else {
            continue;
        };
        let Some(status) = EdgeStatus::from_membership(
            function.base_range.is_some(),
            function.target_range.is_some(),
        ) else {
            continue;
        };
        builder.add_edge(Edge {
            from: Node::module_id(path),
            to: id.clone(),
            relation: Relation::Contains,
            status,
            resolution: Resolution::Declaration,
            evidence: None,
        });
    }
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

/// One module node: the file at `path`, carrying the status the analysis gave
/// it.
fn module_node(path: &str, statuses: &BTreeMap<String, NodeStatus>, depth: u32) -> Node {
    Node {
        id: Node::module_id(path),
        key: String::new(),
        label: Node::basename(path),
        kind: NodeKind::Module,
        path: path.to_owned(),
        // A module has no position inside a file, so it sorts ahead of every
        // function declared in one.
        range_start: (0, 0),
        status: statuses.get(path).copied().unwrap_or(NodeStatus::Unchanged),
        depth,
    }
}

/// The path inside a module node identity.
///
/// The import walk speaks module identities, so undoing [`Node::module_id`] is
/// all an index lookup needs.
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
        analysis::{FunctionChangeStatus, FunctionChurn, MatchConfidence},
        graph::{
            DEFAULT_DEPTH, Direction, Edge, EdgeStatus, Evidence, Graph, Limits, Node, NodeKind,
            NodeStatus, Relation, Resolution, View,
        },
        imports::ImportIndex,
        languages::{CallSite, FunctionKind, SourceRange},
        query,
        result::{
            AnalysisResult, AnalysisSummary, FileResult, FunctionResult, RevisionResult,
            SCHEMA_VERSION,
        },
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

    /// One changed file that carries the functions the graph reads.
    fn modified_with(path: &str, functions: Vec<FunctionResult>) -> FileResult {
        FileResult {
            functions,
            ..modified(path)
        }
    }

    fn range(line: u32) -> SourceRange {
        SourceRange {
            start_line: line,
            start_column: 0,
            end_line: line + 4,
            end_column: 1,
        }
    }

    fn calls(sites: &[(&str, u32)]) -> Vec<CallSite> {
        sites
            .iter()
            .map(|(name, line)| CallSite {
                name: (*name).to_owned(),
                line: *line,
            })
            .collect()
    }

    /// One function record: the fields the graph reads, and the calls each side
    /// wrote. `base` and `target` are the lines the definition starts at on
    /// that side; `None` means the side has no definition.
    fn function(
        id: &str,
        qualified_name: &str,
        status: FunctionChangeStatus,
        base: Option<u32>,
        target: Option<u32>,
        calls_before: &[(&str, u32)],
        calls_after: &[(&str, u32)],
    ) -> FunctionResult {
        FunctionResult {
            id: id.to_owned(),
            status,
            kind: FunctionKind::Function,
            qualified_name: qualified_name.to_owned(),
            base_range: base.map(range),
            target_range: target.map(range),
            metrics_before: None,
            metrics_after: None,
            calls_before: calls(calls_before),
            calls_after: calls(calls_after),
            churn: FunctionChurn {
                lines_removed: 0,
                lines_added: 0,
                changed_hunks: 0,
            },
            match_confidence: MatchConfidence::Exact,
            diagnostics: Vec::new(),
        }
    }

    fn request<'a>(roots: &'a [String], relations: &'a [Relation]) -> Request<'a> {
        Request {
            roots,
            function_root: None,
            direction: Direction::Both,
            relations,
            depth: DEFAULT_DEPTH,
            view: View::Delta,
            limits: Limits::default(),
        }
    }

    fn function_request<'a>(
        function_id: &'a str,
        relations: &'a [Relation],
        depth: u32,
    ) -> Request<'a> {
        Request {
            roots: &[],
            function_root: Some(function_id),
            direction: Direction::Both,
            relations,
            depth,
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

    /// The node a published function identity addresses.
    fn function_node<'a>(graph: &'a Graph, function_id: &str) -> Option<&'a Node> {
        graph.node(&Node::function_id(function_id))
    }

    /// The `calls` edge between two published function identities.
    fn call_edge<'a>(graph: &'a Graph, from: &str, to: &str) -> Option<&'a Edge> {
        graph.edges().iter().find(|edge| {
            edge.relation == Relation::Calls
                && edge.from == Node::function_id(from)
                && edge.to == Node::function_id(to)
        })
    }

    /// The `contains` edge from a module to a published function identity.
    fn contains_edge<'a>(graph: &'a Graph, path: &str, function_id: &str) -> Option<&'a Edge> {
        graph.edges().iter().find(|edge| {
            edge.relation == Relation::Contains
                && edge.from == Node::module_id(path)
                && edge.to == Node::function_id(function_id)
        })
    }

    fn labels(graph: &Graph) -> Vec<&str> {
        graph
            .nodes()
            .iter()
            .map(|node| node.label.as_str())
            .collect()
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

    /// The identity the analysis publishes for the file's `position`-th
    /// function, which is what a function root is named by.
    fn published_id(file: &FileResult, position: usize) -> String {
        query::function_id(file, &file.functions[position])
    }

    /// The two-function fixture the function-root tests share: a modified root
    /// whose target side calls `helper`, and the helper it calls.
    fn root_and_helper() -> FileResult {
        modified_with(
            "src/a.ts",
            vec![
                function(
                    "function-1",
                    "root",
                    FunctionChangeStatus::Modified,
                    Some(1),
                    Some(1),
                    &[],
                    &[("helper", 7)],
                ),
                function(
                    "function-2",
                    "helper",
                    FunctionChangeStatus::Unchanged,
                    Some(10),
                    Some(10),
                    &[],
                    &[],
                ),
            ],
        )
    }

    #[test]
    fn a_call_to_a_function_declared_in_the_same_file_becomes_a_calls_edge() {
        let file = root_and_helper();
        let root_id = published_id(&file, 0);
        let helper_id = published_id(&file, 1);
        let result = analysis(vec![file]);
        let index = ImportIndex::from_edges(&[], &[]);

        let graph = build(
            &result,
            &index,
            &index,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let edge = call_edge(&graph, &root_id, &helper_id).expect("the call resolves");
        assert_eq!(edge.relation, Relation::Calls);
        assert_eq!(edge.resolution, Resolution::DirectLocalSymbol);
        assert_eq!(edge.status, EdgeStatus::Added);
        assert!((edge.confidence() - 1.0).abs() < f64::EPSILON);
        assert_eq!(
            edge.evidence,
            Some(Evidence {
                file: "src/a.ts".to_owned(),
                line: 7,
            })
        );
        assert_eq!(
            graph
                .edges()
                .iter()
                .filter(|edge| edge.relation == Relation::Calls)
                .count(),
            1
        );
        assert!(graph.is_root(&Node::function_id(&root_id)));
        assert_eq!(
            function_node(&graph, &helper_id).map(|node| node.depth),
            Some(1)
        );
    }

    #[test]
    fn a_callee_declared_twice_in_one_file_produces_no_edge() {
        let file = modified_with(
            "src/a.ts",
            vec![
                function(
                    "function-1",
                    "root",
                    FunctionChangeStatus::Modified,
                    Some(1),
                    Some(1),
                    &[],
                    &[("helper", 7)],
                ),
                function(
                    "function-2",
                    "helper",
                    FunctionChangeStatus::Unchanged,
                    Some(10),
                    Some(10),
                    &[],
                    &[],
                ),
                function(
                    "function-3",
                    "helper",
                    FunctionChangeStatus::Unchanged,
                    Some(20),
                    Some(20),
                    &[],
                    &[],
                ),
            ],
        );
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let index = ImportIndex::from_edges(&[], &[]);

        let graph = build(
            &result,
            &index,
            &index,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        // The name resolves to two definitions, and a graph that picked one of
        // them would be worse than one that says nothing.
        assert!(
            graph
                .edges()
                .iter()
                .all(|edge| edge.relation != Relation::Calls)
        );
        assert!(function_node(&graph, &root_id).is_some());
    }

    #[test]
    fn a_call_to_a_name_the_file_does_not_declare_produces_no_edge() {
        let file = modified_with(
            "src/a.ts",
            vec![function(
                "function-1",
                "root",
                FunctionChangeStatus::Modified,
                Some(1),
                Some(1),
                &[("imported", 4)],
                &[("imported", 4), ("parameter", 5)],
            )],
        );
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let index = ImportIndex::from_edges(&[], &[]);

        let graph = build(
            &result,
            &index,
            &index,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        // An import, a local, a global, or a parameter: this stage cannot tell
        // which, so it claims none of them.
        assert!(
            graph
                .edges()
                .iter()
                .all(|edge| edge.relation != Relation::Calls)
        );
    }

    #[test]
    fn an_extracted_helper_is_an_added_node_with_an_added_calls_edge() {
        // The shape `complexity_extraction` reports: the work moves out of the
        // root into a function that did not exist before.
        let file = modified_with(
            "src/a.ts",
            vec![
                function(
                    "function-1",
                    "root",
                    FunctionChangeStatus::Modified,
                    Some(1),
                    Some(1),
                    &[],
                    &[("helper", 3)],
                ),
                function(
                    "function-2",
                    "helper",
                    FunctionChangeStatus::Added,
                    None,
                    Some(10),
                    &[],
                    &[],
                ),
            ],
        );
        let root_id = published_id(&file, 0);
        let helper_id = published_id(&file, 1);
        let result = analysis(vec![file]);
        let index = ImportIndex::from_edges(&[], &[]);

        let graph = build(
            &result,
            &index,
            &index,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let helper = function_node(&graph, &helper_id).expect("the helper is a node");
        assert_eq!(helper.status, NodeStatus::Added);
        assert_eq!(helper.label, "helper");

        let edge = call_edge(&graph, &root_id, &helper_id).expect("the new call resolves");
        assert_eq!(edge.status, EdgeStatus::Added);
        assert_eq!(
            edge.evidence.as_ref().map(|evidence| evidence.line),
            Some(3)
        );

        let contains =
            contains_edge(&graph, "src/a.ts", &helper_id).expect("the module declares the helper");
        assert_eq!(contains.status, EdgeStatus::Added);
        assert_eq!(contains.resolution, Resolution::Declaration);
        assert!((contains.confidence() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_recursive_function_produces_a_self_edge_and_the_walk_terminates() {
        let file = modified_with(
            "src/a.ts",
            vec![function(
                "function-1",
                "root",
                FunctionChangeStatus::Modified,
                Some(1),
                Some(1),
                &[],
                &[("root", 5)],
            )],
        );
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let index = ImportIndex::from_edges(&[], &[]);

        let graph = build(
            &result,
            &index,
            &index,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let edge = call_edge(&graph, &root_id, &root_id).expect("the recursive call is an edge");
        assert_eq!(edge.status, EdgeStatus::Added);
        // The walk leaves the root, returns to it, and stops.
        assert_eq!(labels(&graph), vec!["a.ts", "root"]);
    }

    #[test]
    fn a_call_each_side_wrote_reads_unchanged_and_one_only_the_base_wrote_reads_removed() {
        let file = modified_with(
            "src/a.ts",
            vec![
                function(
                    "function-1",
                    "root",
                    FunctionChangeStatus::Modified,
                    Some(1),
                    Some(1),
                    &[("kept", 3), ("gone", 4)],
                    &[("kept", 8)],
                ),
                function(
                    "function-2",
                    "kept",
                    FunctionChangeStatus::Unchanged,
                    Some(10),
                    Some(10),
                    &[],
                    &[],
                ),
                function(
                    "function-3",
                    "gone",
                    FunctionChangeStatus::Removed,
                    Some(20),
                    None,
                    &[],
                    &[],
                ),
            ],
        );
        let root_id = published_id(&file, 0);
        let kept_id = published_id(&file, 1);
        let gone_id = published_id(&file, 2);
        let result = analysis(vec![file]);
        let index = ImportIndex::from_edges(&[], &[]);

        let graph = build(
            &result,
            &index,
            &index,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let kept = call_edge(&graph, &root_id, &kept_id).expect("the kept call is an edge");
        assert_eq!(kept.status, EdgeStatus::Unchanged);
        // An unchanged edge is read from the target, the revision a reader is
        // looking at.
        assert_eq!(
            kept.evidence.as_ref().map(|evidence| evidence.line),
            Some(8)
        );

        let gone = call_edge(&graph, &root_id, &gone_id).expect("the removed call is an edge");
        assert_eq!(gone.status, EdgeStatus::Removed);
        // A removed edge exists only in the base, so the base is the only place
        // its line can come from.
        assert_eq!(
            gone.evidence.as_ref().map(|evidence| evidence.line),
            Some(4)
        );
    }

    #[test]
    fn a_function_root_walks_calls_in_the_direction_and_to_the_depth_it_was_asked_for() {
        let file = modified_with(
            "src/a.ts",
            vec![
                function(
                    "function-1",
                    "caller",
                    FunctionChangeStatus::Modified,
                    Some(1),
                    Some(1),
                    &[],
                    &[("root", 3)],
                ),
                function(
                    "function-2",
                    "root",
                    FunctionChangeStatus::Modified,
                    Some(10),
                    Some(10),
                    &[],
                    &[("helper", 12)],
                ),
                function(
                    "function-3",
                    "helper",
                    FunctionChangeStatus::Unchanged,
                    Some(20),
                    Some(20),
                    &[],
                    &[("leaf", 22)],
                ),
                function(
                    "function-4",
                    "leaf",
                    FunctionChangeStatus::Unchanged,
                    Some(30),
                    Some(30),
                    &[],
                    &[],
                ),
            ],
        );
        let caller_id = published_id(&file, 0);
        let root_id = published_id(&file, 1);
        let helper_id = published_id(&file, 2);
        let leaf_id = published_id(&file, 3);
        let result = analysis(vec![file]);
        let index = ImportIndex::from_edges(&[], &[]);

        let mut downstream = function_request(&root_id, Relation::SUPPORTED, 1);
        downstream.direction = Direction::Downstream;
        let graph = build(&result, &index, &index, &downstream);
        assert_eq!(labels(&graph), vec!["a.ts", "root", "helper"]);
        assert!(call_edge(&graph, &root_id, &helper_id).is_some());
        assert!(call_edge(&graph, &caller_id, &root_id).is_none());

        let mut two_hops = downstream;
        two_hops.depth = 2;
        let graph = build(&result, &index, &index, &two_hops);
        assert_eq!(labels(&graph), vec!["a.ts", "root", "helper", "leaf"]);
        assert!(call_edge(&graph, &helper_id, &leaf_id).is_some());

        let mut upstream = function_request(&root_id, Relation::SUPPORTED, 1);
        upstream.direction = Direction::Upstream;
        let graph = build(&result, &index, &index, &upstream);
        assert_eq!(labels(&graph), vec!["a.ts", "caller", "root"]);
        assert!(call_edge(&graph, &caller_id, &root_id).is_some());
        assert!(call_edge(&graph, &root_id, &helper_id).is_none());
    }

    #[test]
    fn contains_edges_attach_the_module_unless_the_relation_is_excluded() {
        let file = root_and_helper();
        let root_id = published_id(&file, 0);
        let helper_id = published_id(&file, 1);
        let result = analysis(vec![file]);
        let index = ImportIndex::from_edges(&[], &[]);

        let graph = build(
            &result,
            &index,
            &index,
            &function_request(
                &root_id,
                &[Relation::Calls, Relation::Contains],
                DEFAULT_DEPTH,
            ),
        );
        assert!(contains_edge(&graph, "src/a.ts", &root_id).is_some());
        assert!(contains_edge(&graph, "src/a.ts", &helper_id).is_some());
        assert_eq!(
            node(&graph, "src/a.ts").map(|node| node.kind),
            Some(NodeKind::Module)
        );

        let graph = build(
            &result,
            &index,
            &index,
            &function_request(&root_id, &[Relation::Calls], DEFAULT_DEPTH),
        );
        assert!(
            graph
                .edges()
                .iter()
                .all(|edge| edge.relation == Relation::Calls)
        );
        assert!(
            graph
                .nodes()
                .iter()
                .all(|node| node.kind == NodeKind::Function)
        );
    }

    #[test]
    fn a_file_rooted_graph_keeps_the_module_only_answer() {
        let file = root_and_helper();
        let result = analysis(vec![file]);
        let index = ImportIndex::from_edges(&[], &[]);
        let roots = paths_of(&["src/a.ts"]);

        let graph = build(
            &result,
            &index,
            &index,
            &request(&roots, Relation::SUPPORTED),
        );

        // M2 gives function nodes to a function root alone; a file root is
        // still the module answer M1 delivered, byte for byte.
        assert_eq!(node_paths(&graph), vec!["src/a.ts"]);
        assert!(
            graph
                .nodes()
                .iter()
                .all(|node| node.kind == NodeKind::Module)
        );
        assert!(graph.edges().is_empty());
    }

    #[test]
    fn a_function_root_the_analysis_does_not_contain_is_an_empty_graph() {
        let result = analysis(vec![root_and_helper()]);
        let index = ImportIndex::from_edges(&[], &[]);

        let graph = build(
            &result,
            &index,
            &index,
            &function_request(
                "src/a.ts#fn:absent@target:1:0",
                Relation::SUPPORTED,
                DEFAULT_DEPTH,
            ),
        );

        assert!(graph.nodes().is_empty());
        assert!(graph.edges().is_empty());
    }
}
