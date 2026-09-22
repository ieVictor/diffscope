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
        Completeness, Direction, Edge, EdgeStatus, Evidence, Graph, GraphBuilder, GroupRole,
        Limits, Node, NodeKind, NodeStatus, Reached, Relation, Resolution, View, walk,
    },
    imports::{ExportedDefinition, ImportIndex, SymbolIndex},
    languages::{CallSite, ImportedName, SourceRange, symbol_id},
    query::{
        self,
        classify::{self, FileClassification},
        impact,
    },
    result::{AnalysisResult, FileResult, FunctionResult},
};

/// The prefix [`Node::module_id`] puts in front of a path.
const MODULE_PREFIX: &str = "module:";

/// Everything one comparison's two revisions know about relationships.
///
/// The import graphs cover every file of each revision; the symbol indexes
/// cover only the files cross-file resolution is allowed to parse, which is
/// the changed files, their direct importers, and what they import. Both sides
/// are always present, because only two revisions can prove that a
/// relationship was removed.
pub struct Revisions<'a> {
    pub base: &'a ImportIndex,
    pub target: &'a ImportIndex,
    pub base_symbols: &'a SymbolIndex,
    pub target_symbols: &'a SymbolIndex,
}

impl Revisions<'_> {
    /// The import graph and symbol index of one side.
    fn side(&self, side: Side) -> (&ImportIndex, &SymbolIndex) {
        match side {
            Side::Base => (self.base, self.base_symbols),
            Side::Target => (self.target, self.target_symbols),
        }
    }
}

/// The files cross-file resolution may parse, for one comparison.
///
/// A file can only call into a changed module if it imports that module, and
/// the import index already names those files, so the set is the changed files
/// themselves, their direct importers on either side, and the modules they
/// import. Parsing a whole revision at full fidelity costs 676 ms on a Vue
/// revision before the function collector runs; this bound is a property of
/// how code is written rather than of how the tool is configured, and it is
/// what makes function-level impact affordable at all.
///
/// Direct importers only. Through a package's barrel module almost every file
/// reaches almost every other — one Vue utility is reached by 166 modules
/// within two hops — so a caller list built from indirect importers is not a
/// caller list.
#[must_use]
pub fn admitted_files(
    result: &AnalysisResult,
    base: &ImportIndex,
    target: &ImportIndex,
) -> BTreeSet<String> {
    let mut admitted = BTreeSet::new();
    for file in &result.files {
        for path in [file.base_path.as_deref(), file.target_path.as_deref()]
            .into_iter()
            .flatten()
        {
            admitted.insert(path.to_owned());
            for index in [base, target] {
                admitted.extend(index.importers(path).iter().cloned());
                admitted.extend(index.dependencies(path).iter().cloned());
            }
        }
    }
    admitted
}

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
pub fn build(result: &AnalysisResult, revisions: &Revisions<'_>, request: &Request<'_>) -> Graph {
    if let Some(function_id) = request.function_root {
        return function_graph(result, revisions, function_id, request);
    }

    let base = revisions.base;
    let target = revisions.target;
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
    if request.relations.contains(&Relation::ReExports) {
        add_re_export_edges(&mut builder, revisions, &reached);
    }

    builder.set_completeness(module_completeness(revisions, request.view, &reached));
    builder.apply_view(request.view);
    collapse_groups(&mut builder, request.limits);
    builder.finish(request.limits)
}

/// Build the graph a function root describes.
///
/// The graph is the function, the calls it takes part in on either side of the
/// change, and the module that declares it. A call resolves in one of three
/// ways, and in no other: against a name declared in the caller's own file,
/// against a name the caller imports from a module that exports it, or against
/// a name reached through re-exports. Each side is resolved separately, so a
/// call only one revision wrote is one edge with one status, and the walk
/// follows both revisions' edges, so a removed call is as visible as an added
/// one.
///
/// Callers are discovered from the direct importers of the root's file, in
/// both revisions, because a file can only call into a module it imports and a
/// caller the change removed exists only in the base. Indirect importers are
/// not parsed: through a barrel module almost every file reaches almost every
/// other, and a caller list built that way is not a caller list.
///
/// The module's own `imports` and `tested_by` relationships are deliberately
/// not walked. They are the file-rooted answer; mixing them in would put two
/// notions of "near the root" in one graph, and a caller who wants the module's
/// relationships asks for a file root.
fn function_graph(
    result: &AnalysisResult,
    revisions: &Revisions<'_>,
    function_id: &str,
    request: &Request<'_>,
) -> Graph {
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
    let mut ids: BTreeMap<&str, String> = BTreeMap::new();
    let mut places: Places<'_> = BTreeMap::new();
    for function in functions {
        let id = function_node_id(file, function);
        ids.insert(function.id.as_str(), id.clone());
        places.insert(
            id,
            Placed::Analyzed {
                file,
                path: path.clone(),
                function,
            },
        );
    }
    let local = &places.keys().cloned().collect::<BTreeSet<_>>();
    let locals = Locals {
        ids,
        base: declared_names(functions, |function| function.base_range.is_some()),
        target: declared_names(functions, |function| function.target_range.is_some()),
    };
    let resolver = Resolver {
        revisions,
        changed: changed_files(result),
        guess: request.relations.contains(&Relation::PossibleCall),
        view: request.view,
    };
    let mut unresolved = UnresolvedCalls::default();
    let mut calls = local_calls(functions, &path, &locals);

    add_cross_file_callees(
        &mut calls,
        &mut unresolved,
        &mut places,
        &resolver,
        &path,
        functions,
        &locals,
    );
    add_cross_file_callers(
        &mut calls,
        &mut unresolved,
        &mut places,
        &resolver,
        file,
        &path,
    );

    let (outgoing, incoming) = call_adjacency(&calls, request.relations);
    let root_id = function_node_id(file, root);

    let mut builder = GraphBuilder::new();
    builder.add_root(root_id.clone());
    let reached = if request.relations.contains(&Relation::Calls) || resolver.guess {
        walk(
            &[root_id],
            request.direction,
            request.depth,
            |id, direction| call_neighbors(&outgoing, &incoming, id, direction),
        )
    } else {
        // A graph that may not follow a call relationship is still centered
        // on the root, so the walk is skipped rather than run over a relation
        // it may not follow.
        BTreeMap::from([(root_id, 0)])
    };
    for (id, depth) in &reached {
        if let Some(place) = places.get(id) {
            builder.add_node(place.node(*depth));
        }
    }

    add_call_edges(&mut builder, &calls, &reached, request.relations);
    if request.relations.contains(&Relation::Contains) {
        add_contains_edges(
            &mut builder,
            &path,
            &changed_statuses(result),
            &reached,
            &places,
            local,
        );
    }

    let mut completeness = function_completeness(revisions, file, &path, request.view);
    completeness.unresolved_calls = unresolved.count();
    builder.set_completeness(completeness);
    builder.apply_view(request.view);
    collapse_groups(&mut builder, request.limits);
    builder.finish(request.limits)
}

/// Completeness for the module paths the request's walk examined.
///
/// The reached set is captured before view filtering, grouping, and delivery
/// budgets. A one-sided view counts only its selected revision; delta counts
/// the union of both side-specific observations.
fn module_completeness(revisions: &Revisions<'_>, view: View, reached: &Reached) -> Completeness {
    let scope = reached
        .keys()
        .map(|id| path_of(id).to_owned())
        .collect::<BTreeSet<_>>();
    index_completeness(revisions, view, &scope, &scope)
}

/// Completeness of named source-file scopes on the relevant revision sides.
fn index_completeness(
    revisions: &Revisions<'_>,
    view: View,
    base_scope: &BTreeSet<String>,
    target_scope: &BTreeSet<String>,
) -> Completeness {
    let mut completeness = Completeness::default();
    for (side, scope) in [(Side::Base, base_scope), (Side::Target, target_scope)] {
        if !view_includes_side(view, side) {
            continue;
        }
        let (index, _) = revisions.side(side);
        completeness.scan_truncated_files = completeness.scan_truncated_files.saturating_add(
            u32::try_from(index.truncated_files().intersection(scope).count()).unwrap_or(u32::MAX),
        );
        completeness.unresolved_specifiers = completeness
            .unresolved_specifiers
            .saturating_add(index.unresolved_specifiers_in(scope));
    }
    completeness
}

/// Whether a response view reports information observed on this revision side.
const fn view_includes_side(view: View, side: Side) -> bool {
    matches!(
        (view, side),
        (View::Delta, _) | (View::Base, Side::Base) | (View::Target, Side::Target)
    )
}

/// Completeness for the source files function resolution inspected.
fn function_completeness(
    revisions: &Revisions<'_>,
    file: &FileResult,
    path: &str,
    view: View,
) -> Completeness {
    let base_scope = file
        .base_path
        .iter()
        .chain(revisions.base.importers(path))
        .cloned()
        .collect::<BTreeSet<_>>();
    let target_scope = file
        .target_path
        .iter()
        .chain(revisions.target.importers(path))
        .cloned()
        .collect::<BTreeSet<_>>();
    index_completeness(revisions, view, &base_scope, &target_scope)
}

/// Collapse what a reader does not need one box per, before a budget drops any
/// of it.
///
/// The rules are the documented ones, in order. A module's related tests are
/// context rather than topology, so eight of them become one group and
/// contribute one `tested_by` edge. Generated, vendored, and lockfile nodes
/// are classified from their path alone by [`classify::classify`], with no
/// filesystem access, so the same graph collapses identically on every
/// machine. Whatever the node budget still cannot fit collapses by direction,
/// so the two sides of the root stay distinguishable.
///
/// An external dependency needs no rule of its own: a specifier that resolved
/// to nothing never became a node — [`ImportIndex::unresolved_specifiers`]
/// counts it instead — and one that resolved into a vendored directory is a
/// node the classification rule already collapses.
///
/// This runs before [`GraphBuilder::finish`] truncates, so a group is never
/// dropped in favour of a node it replaced.
fn collapse_groups(builder: &mut GraphBuilder, limits: Limits) {
    collapse_tests(builder);
    collapse_classified(builder);
    collapse_overflow(builder, limits);
}

/// Collapse the tests that point at anything in the graph.
///
/// Membership is the `tested_by` edge rather than the path, because a test is
/// grouped for the relationship it has to the graph: a test file reached as a
/// plain importer of something else is still a place a reader may want named.
fn collapse_tests(builder: &mut GraphBuilder) {
    let tests = builder
        .edges()
        .filter(|edge| edge.relation == Relation::TestedBy)
        .map(|edge| edge.from.clone())
        .collect::<BTreeSet<_>>();
    collapse(builder, GroupRole::Tests, &tests);
}

/// Collapse the nodes whose path says nobody reads them individually.
fn collapse_classified(builder: &mut GraphBuilder) {
    for (classification, role) in [
        (FileClassification::Generated, GroupRole::Generated),
        (FileClassification::Vendored, GroupRole::Vendored),
        (FileClassification::Lockfile, GroupRole::Lockfile),
    ] {
        let members = builder
            .nodes()
            .into_iter()
            .filter(|node| {
                node.kind != NodeKind::Group && classify::classify(&node.path) == classification
            })
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        collapse(builder, role, &members);
    }
}

/// Collapse whatever the node budget cannot fit, by the side of the root it
/// sits on.
///
/// A group occupies a node slot of its own, so the budget is asked what it
/// would drop with those slots already spent: the smallest reservation whose
/// own groups fit inside it is the one applied, which is why a graph that
/// overflows on one side keeps one more node than a graph that overflows on
/// both.
fn collapse_overflow(builder: &mut GraphBuilder, limits: Limits) {
    for reserve in 0..=2 {
        let (callers, dependencies) = sides(
            builder,
            &builder.overflow(limits.max_nodes.saturating_sub(reserve)),
        );
        if groups_needed(&callers) + groups_needed(&dependencies) > reserve {
            continue;
        }
        let omitted = collapse(builder, GroupRole::Callers, &callers)
            + collapse(builder, GroupRole::Dependencies, &dependencies);
        builder.omit(omitted);
        return;
    }
}

/// How many group nodes a side costs: one, or none when it has nothing to
/// collapse.
fn groups_needed(side: &BTreeSet<String>) -> usize {
    usize::from(side.len() > 1)
}

/// Which side of the root each node sits on.
///
/// A node that points into the graph reaches the root and is a caller;
/// anything else is something the root reaches. The sides are read from the
/// edges the graph kept rather than from the walk, so a node's side is a
/// property of the answer a reader sees.
fn sides(builder: &GraphBuilder, nodes: &BTreeSet<String>) -> (BTreeSet<String>, BTreeSet<String>) {
    let sources = builder
        .edges()
        .map(|edge| edge.from.as_str())
        .collect::<BTreeSet<_>>();
    nodes
        .iter()
        .cloned()
        .partition(|id| sources.contains(id.as_str()))
}

/// Collapse one set of nodes, naming the change area they share when they
/// share one.
///
/// A root is never a member: the graph is the answer to a question about it.
/// A rule that matches a single node leaves it alone, because a group of one
/// replaces a named file with a vaguer node and saves no room at all.
fn collapse(builder: &mut GraphBuilder, role: GroupRole, members: &BTreeSet<String>) -> u32 {
    let members = members
        .iter()
        .filter(|id| !builder.is_root(id))
        .cloned()
        .collect::<BTreeSet<_>>();
    if members.len() < 2 {
        return 0;
    }
    let area = shared_area(builder, &members);
    builder.collapse(role, &members, area)
}

/// The change area every member sits in, when they all sit in one.
///
/// The derivation is [`query::change_area`], so an area named here means what
/// it means in a change summary.
fn shared_area(builder: &GraphBuilder, members: &BTreeSet<String>) -> Option<String> {
    let mut shared: Option<String> = None;
    for node in members.iter().filter_map(|id| builder.node(id)) {
        let area = query::change_area(&node.path);
        match &shared {
            None => shared = Some(area),
            Some(existing) if *existing == area => {}
            Some(_) => return None,
        }
    }
    shared
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
        group: None,
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

/// One call relationship, and the sides it was seen on.
///
/// Both sides are merged into one entry so that a relationship the two
/// revisions share is one edge with one status, and the earliest line each side
/// recorded is kept as the evidence for it.
#[derive(Debug)]
struct CallEdge {
    in_base: bool,
    in_target: bool,
    base_line: Option<u32>,
    target_line: Option<u32>,
    /// File the call is written in, which is always the caller's own.
    site: String,
    /// How the callee was resolved.
    ///
    /// When the two revisions resolved one relationship differently — a name
    /// imported directly in one and through a barrel module in the other — the
    /// less confident resolution is kept, because an edge is only as
    /// trustworthy as the weaker of the two claims behind it.
    resolution: Resolution,
}

impl CallEdge {
    fn new(site: &str, resolution: Resolution) -> Self {
        Self {
            in_base: false,
            in_target: false,
            base_line: None,
            target_line: None,
            site: site.to_owned(),
            resolution,
        }
    }

    /// Record one side's sighting of this relationship.
    fn saw(&mut self, side: Side, line: u32, resolution: Resolution) {
        match side {
            Side::Base => {
                self.in_base = true;
                earliest(&mut self.base_line, line);
            }
            Side::Target => {
                self.in_target = true;
                earliest(&mut self.target_line, line);
            }
        }
        if resolution.confidence() < self.resolution.confidence() {
            self.resolution = resolution;
        }
    }
}

/// Every call relationship a function graph holds, keyed by caller, callee,
/// and the relation it is reported under.
///
/// The relation is part of the key because a guess and a proof about one pair
/// of functions are two different statements: a `possible_call` never merges
/// into the `calls` edge beside it, and neither takes the other's confidence.
type CallEdges = BTreeMap<(String, String, Relation), CallEdge>;

/// One direction of a file's call relationships, as node identities.
type Adjacency<'a> = BTreeMap<&'a str, Vec<&'a str>>;

/// Which revision a call site was read from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    Base,
    Target,
}
type UnresolvedCallSite = (Side, String, u32, u32, String, Option<String>, u32);

/// Call sites no exact rule, nor an opted-in possible-call rule, resolved.
///
/// Site identity includes its function range and revision side, so passes that
/// inspect the same source site cannot inflate the report.
#[derive(Default)]
struct UnresolvedCalls {
    sites: BTreeSet<UnresolvedCallSite>,
}

impl UnresolvedCalls {
    fn record(&mut self, side: Side, path: &str, range: &SourceRange, call: &CallSite, view: View) {
        if view_includes_side(view, side) {
            self.sites.insert((
                side,
                path.to_owned(),
                range.start_line,
                range.start_column,
                call.name.clone(),
                call.receiver.clone(),
                call.line,
            ));
        }
    }

    fn count(&self) -> u32 {
        u32::try_from(self.sites.len()).unwrap_or(u32::MAX)
    }
}
/// Every local call relationship in one file, keyed by caller and callee.
///
/// Both revisions' call sites are resolved and merged, so a relationship only
/// one revision has is still an entry: a removed call is as much a fact of the
/// comparison as an added one, and a call both revisions wrote is one entry
/// rather than two.
fn local_calls<'a>(functions: &'a [FunctionResult], path: &str, locals: &Locals<'a>) -> CallEdges {
    let mut edges: CallEdges = BTreeMap::new();
    for caller in functions {
        resolve_calls(
            caller,
            &caller.calls_before,
            path,
            &locals.base,
            Side::Base,
            &locals.ids,
            &mut edges,
        );
        resolve_calls(
            caller,
            &caller.calls_after,
            path,
            &locals.target,
            Side::Target,
            &locals.ids,
            &mut edges,
        );
    }
    edges
}

/// Record the calls one side of one function wrote to names its own file
/// declares.
///
/// A name that side declares twice and a name it declares nowhere both produce
/// nothing here: the first is ambiguity, the second is an import, a local, a
/// global, or a parameter, and an import is resolved by
/// [`add_cross_file_callees`] instead. This stage resolves a call exactly or
/// not at all, so neither is guessed at, lowered in confidence, or recorded as
/// a placeholder.
///
/// A member call is never local: `receiver.name()` says the callee belongs to
/// something, and only a namespace import says what.
fn resolve_calls<'a>(
    caller: &FunctionResult,
    calls: &[CallSite],
    path: &str,
    names: &BTreeMap<&'a str, Option<&'a FunctionResult>>,
    side: Side,
    node_ids: &BTreeMap<&'a str, String>,
    edges: &mut CallEdges,
) {
    for call in calls {
        if call.receiver.is_some() {
            continue;
        }
        let Some(Some(target)) = names.get(call.name.as_str()) else {
            continue;
        };
        let (Some(from), Some(to)) = (
            node_ids.get(caller.id.as_str()),
            node_ids.get(target.id.as_str()),
        ) else {
            continue;
        };
        edges
            .entry((from.clone(), to.clone(), Relation::Calls))
            .or_insert_with(|| CallEdge::new(path, Resolution::DirectLocalSymbol))
            .saw(side, call.line, Resolution::DirectLocalSymbol);
    }
}

/// Whether one side of one file declares the name a call writes.
///
/// A call that resolves locally is not looked up across files: the file's own
/// declaration is the definition, and TypeScript would not let an import share
/// the name.
fn declares(names: &BTreeMap<&str, Option<&FunctionResult>>, call: &CallSite) -> bool {
    call.receiver.is_none() && matches!(names.get(call.name.as_str()), Some(Some(_)))
}

/// The definition one call site names in another module, when it names one.
///
/// The three shapes that resolve are a named import, a default import, and a
/// call on a namespace import. A bare call on a namespace (`ns()`), a member
/// call on anything else, and a name the file does not import resolve to
/// nothing: this stage reports a call it can prove or none at all.
fn cross_file_callee(
    revisions: &Revisions<'_>,
    side: Side,
    from: &str,
    call: &CallSite,
) -> Option<(ExportedDefinition, Resolution)> {
    let (imports, symbols) = revisions.side(side);
    let bindings = imports.bindings(from);
    let (binding, exported) = if let Some(receiver) = &call.receiver {
        let binding = bindings.get(receiver)?;
        // `ns.name()` resolves exactly, because the receiver is a module whose
        // exported names are known.
        if binding.imported != ImportedName::Namespace {
            return None;
        }
        (binding, call.name.clone())
    } else {
        let binding = bindings.get(&call.name)?;
        let exported = match &binding.imported {
            ImportedName::Named(name) => name.clone(),
            ImportedName::Default => "default".to_owned(),
            // Calling a namespace object is not a call into any one of the
            // names it holds.
            ImportedName::Namespace => return None,
        };
        (binding, exported)
    };
    let module = imports.resolve_specifier(from, &binding.specifier)?;
    let definition = symbols.definition(imports, module, &exported)?;
    // A definition reached through re-exports crossed one resolution per hop,
    // each of which could be wrong.
    let resolution = if definition.hops == 0 {
        Resolution::ImportedSymbol
    } else {
        Resolution::ReExportedSymbol
    };
    Some((definition, resolution))
}

/// Where one definition a property call could mean is written.
///
/// A path and a range are what [`place_function`] needs, and they are what
/// makes two sightings of one function one candidate rather than two.
type Candidate = (String, SourceRange);

/// The function a property call's name could mean, when it can only mean one.
///
/// This is the heuristic, and it resolves nothing: `handlers.parse()` says
/// the callee is a property of something, and nothing in the file says what
/// that something holds. What the name can be checked against is the scope
/// the calling file can see a function through — the functions it declares
/// itself, and the functions the modules it imports export — which is the
/// same reach the exact resolutions have and reads no file they do not.
///
/// A name that scope answers with two or more definitions produces nothing.
/// Picking one of them would be the arbitrary choice
/// [`declared_names`] already refuses for a local name, and a heuristic that
/// fires on an ambiguous name is noise rather than a lead.
fn possible_callee(
    revisions: &Revisions<'_>,
    side: Side,
    from: &str,
    call: &CallSite,
) -> Option<Candidate> {
    // Only a property or computed access is guessed at. A bare `name()` is
    // resolved exactly or not at all, and lowering it to a guess would put a
    // second, weaker answer beside an exact one.
    call.receiver.as_ref()?;
    let (imports, symbols) = revisions.side(side);
    // Keyed by where a definition is written, so one function reached both as
    // a declaration and through an export is one candidate.
    let mut candidates: BTreeMap<(String, u32, u32), Candidate> = BTreeMap::new();
    let mut remember = |path: String, range: SourceRange| {
        let _existing = candidates.insert(
            (path.clone(), range.start_line, range.start_column),
            (path, range),
        );
    };
    if let Some(file) = symbols.file(from) {
        for function in &file.functions {
            if declared_name(&function.qualified_name) == call.name {
                remember(from.to_owned(), function.range.clone());
            }
        }
    }
    for module in imports.dependencies(from) {
        if let Some(definition) = symbols.definition(imports, module, &call.name) {
            remember(definition.path, definition.range);
        }
    }
    if candidates.len() == 1 {
        candidates.into_values().next()
    } else {
        None
    }
}

/// A function node the graph can place, by identity.
///
/// A function in a file the diff contains is named by the record the analysis
/// produced, so a node here and a detail answer's node for one function are
/// the same string with the same status. A function in a file the diff does
/// not contain has no such record, and is named from the definition the symbol
/// index read.
enum Placed<'a> {
    Analyzed {
        file: &'a FileResult,
        path: String,
        function: &'a FunctionResult,
    },
    External(External),
}

/// Every function node the graph may hold, keyed by node identity.
type Places<'a> = BTreeMap<String, Placed<'a>>;

/// A function definition read from a revision rather than from the analysis.
struct External {
    id: String,
    path: String,
    label: String,
    range_start: (u32, u32),
    in_base: bool,
    in_target: bool,
}

impl Placed<'_> {
    fn node(&self, depth: u32) -> Node {
        match self {
            Self::Analyzed {
                file,
                path,
                function,
            } => function_node(file, path, function, depth),
            Self::External(external) => Node {
                id: external.id.clone(),
                key: String::new(),
                label: external.label.clone(),
                kind: NodeKind::Function,
                path: external.path.clone(),
                range_start: external.range_start,
                // Membership is all an unchanged file can say, and all it
                // needs to: a caller the change removed is in the base alone.
                status: NodeStatus::from_membership(external.in_base, external.in_target),
                depth,
                group: None,
            },
        }
    }
}

/// The files the comparison contains, by the path the graph names them under.
fn changed_files(result: &AnalysisResult) -> BTreeMap<&str, &FileResult> {
    let mut changed = BTreeMap::new();
    for file in &result.files {
        for path in [file.target_path.as_deref(), file.base_path.as_deref()]
            .into_iter()
            .flatten()
        {
            let _existing = changed.entry(path).or_insert(file);
        }
    }
    changed
}

/// Name the node one definition belongs to, registering it if it is new.
///
/// A definition in a changed file is matched back to the analysis' record for
/// it, by the range that side reported, so the graph and every other answer
/// name one function identically. A definition anywhere else is identified the
/// same way [`query::function_id`] identifies one — path, symbol, side, and
/// position — with the side taken from the revision that still has it, so a
/// function both revisions share is one node rather than two.
fn place_function<'a>(
    places: &mut Places<'a>,
    revisions: &Revisions<'_>,
    changed: &BTreeMap<&'a str, &'a FileResult>,
    path: &str,
    range: &SourceRange,
) -> Option<String> {
    if let Some(file) = changed.get(path) {
        let function = file.functions.iter().find(|function| {
            function.base_range.as_ref() == Some(range)
                || function.target_range.as_ref() == Some(range)
        })?;
        let id = function_node_id(file, function);
        places.entry(id.clone()).or_insert(Placed::Analyzed {
            file,
            path: path.to_owned(),
            function,
        });
        return Some(id);
    }

    let in_base = holds(revisions.base_symbols, path, range);
    let in_target = holds(revisions.target_symbols, path, range);
    let function = revisions
        .target_symbols
        .file(path)
        .into_iter()
        .chain(revisions.base_symbols.file(path))
        .flat_map(|file| file.functions.iter())
        .find(|function| &function.range == range)?;
    let side = if in_target { "target" } else { "base" };
    let id = Node::function_id(&format!(
        "{path}#{}@{side}:{}:{}",
        symbol_id(function.kind, &function.qualified_name),
        range.start_line,
        range.start_column
    ));
    places
        .entry(id.clone())
        .or_insert(Placed::External(External {
            id: id.clone(),
            path: path.to_owned(),
            label: function.qualified_name.clone(),
            range_start: (range.start_line, range.start_column),
            in_base,
            in_target,
        }));
    Some(id)
}

/// Whether one revision holds a function at this path and range.
fn holds(symbols: &SymbolIndex, path: &str, range: &SourceRange) -> bool {
    symbols
        .file(path)
        .is_some_and(|file| file.functions.iter().any(|f| &f.range == range))
}

/// What the root's own file declares: the node identity of every record, and
/// the definitions each side has by the name a call would write.
///
/// The three move together because every local resolution needs all three, and
/// each is derived once per graph rather than per call site.
struct Locals<'a> {
    ids: BTreeMap<&'a str, String>,
    base: BTreeMap<&'a str, Option<&'a FunctionResult>>,
    target: BTreeMap<&'a str, Option<&'a FunctionResult>>,
}

/// What cross-file call resolution reads from, for one comparison.
///
/// The two revisions to resolve against, the files the diff contains, whether
/// this request admits a guess, and the requested view travel together because
/// every cross-file resolution needs them. Keeping the view here also prevents
/// unresolved-call accounting from drifting from the edge resolution it
/// observes.
struct Resolver<'a> {
    revisions: &'a Revisions<'a>,
    changed: BTreeMap<&'a str, &'a FileResult>,
    guess: bool,
    view: View,
}

/// Add an edge for every call the root's file makes into another module.
fn add_cross_file_callees<'a>(
    calls: &mut CallEdges,
    unresolved: &mut UnresolvedCalls,
    places: &mut Places<'a>,
    resolver: &Resolver<'a>,
    path: &str,
    functions: &'a [FunctionResult],
    locals: &Locals<'_>,
) {
    let node_ids = &locals.ids;
    let (base_names, target_names) = (&locals.base, &locals.target);
    for caller in functions {
        let Some(from) = node_ids.get(caller.id.as_str()).cloned() else {
            continue;
        };
        for (side, sites, declared) in [
            (Side::Base, &caller.calls_before, base_names),
            (Side::Target, &caller.calls_after, target_names),
        ] {
            for call in sites {
                if declares(declared, call) {
                    continue;
                }
                let Some((relation, resolution, target)) = callee(resolver, side, path, call)
                else {
                    if let Some(range) = side_range(caller, side) {
                        unresolved.record(side, path, range, call, resolver.view);
                    }
                    continue;
                };
                let Some(to) = place_function(
                    places,
                    resolver.revisions,
                    &resolver.changed,
                    &target.0,
                    &target.1,
                ) else {
                    continue;
                };
                calls
                    .entry((from.clone(), to, relation))
                    .or_insert_with(|| CallEdge::new(path, resolution))
                    .saw(side, call.line, resolution);
            }
        }
    }
}

/// The declaration range identifying one function on the side its call came
/// from. A call without a declaration on that side cannot occur in normal
/// analysis output, but is ignored rather than making malformed input panic.
fn side_range(function: &FunctionResult, side: Side) -> Option<&SourceRange> {
    match side {
        Side::Base => function.base_range.as_ref(),
        Side::Target => function.target_range.as_ref(),
    }
}

/// What one call site points at, exactly or at a guess.
///
/// The exact rules are asked first and the heuristic only sees what they
/// leave, so a call that resolves exactly is never also reported as a guess
/// and `calls` says exactly what it said before this relation existed. A
/// request that did not name `possible_call` never reaches the heuristic at
/// all: its search costs one lookup per module the file imports, and an
/// answer nobody asked for should not pay it.
fn callee(
    resolver: &Resolver<'_>,
    side: Side,
    from: &str,
    call: &CallSite,
) -> Option<(Relation, Resolution, Candidate)> {
    if let Some((definition, resolution)) = cross_file_callee(resolver.revisions, side, from, call)
    {
        return Some((
            Relation::Calls,
            resolution,
            (definition.path, definition.range),
        ));
    }
    if !resolver.guess {
        return None;
    }
    let candidate = possible_callee(resolver.revisions, side, from, call)?;
    Some((
        Relation::PossibleCall,
        Resolution::PropertyNameMatch,
        candidate,
    ))
}

/// Add an edge for every call into the root's file from a module that imports
/// it.
///
/// The importers come from each revision's import graph, so a caller only the
/// base has is discovered from the base, which is what makes a removed caller
/// visible. Their bodies come from the analysis when the diff contains them and
/// from the revision's symbol index otherwise, which is the only place files
/// outside the diff are read at all.
///
/// A property call in an importing file is guessed at under the same rule and
/// the same request: the importer can see the root's exported names, so a
/// method call whose name only one of them matches is a lead about who calls
/// the changed function.
fn add_cross_file_callers<'a>(
    calls: &mut CallEdges,
    unresolved: &mut UnresolvedCalls,
    places: &mut Places<'a>,
    resolver: &Resolver<'a>,
    file: &'a FileResult,
    path: &'a str,
) {
    let revisions = resolver.revisions;
    let functions = file.functions.as_slice();
    for side in [Side::Base, Side::Target] {
        let (imports, _) = revisions.side(side);
        for importer in imports.importers(path) {
            for caller in callers_of(revisions, &resolver.changed, importer, side) {
                for call in caller.calls {
                    let Some((relation, resolution, target)) =
                        callee(resolver, side, importer, call)
                    else {
                        unresolved.record(side, caller.path, caller.range, call, resolver.view);
                        continue;
                    };
                    if target.0 != path {
                        continue;
                    }
                    let Some(to) = root_file_function(file, functions, &target.1, side) else {
                        continue;
                    };
                    let Some(from) = place_function(
                        places,
                        revisions,
                        &resolver.changed,
                        caller.path,
                        caller.range,
                    ) else {
                        continue;
                    };
                    calls
                        .entry((from, to, relation))
                        .or_insert_with(|| CallEdge::new(importer, resolution))
                        .saw(side, call.line, resolution);
                }
            }
        }
    }
}

/// One function that may call into the root's file, wherever it was read from.
///
/// A caller is a position and a list of call sites; whether the analysis or a
/// revision's symbol index produced it matters only when the node is named,
/// which [`place_function`] decides from the path alone.
struct Caller<'a> {
    path: &'a str,
    range: &'a SourceRange,
    calls: &'a [CallSite],
}

/// The functions one importing file declares on one side.
///
/// The analysis is preferred when the diff contains the file, because its
/// records are what every other answer names that file's functions by. Only a
/// file the diff does not contain is read from the symbol index, and only
/// because it imports something that changed.
fn callers_of<'a>(
    revisions: &'a Revisions<'a>,
    changed: &BTreeMap<&'a str, &'a FileResult>,
    importer: &'a str,
    side: Side,
) -> Vec<Caller<'a>> {
    if let Some(file) = changed.get(importer) {
        return file
            .functions
            .iter()
            .filter_map(|function| {
                let (range, calls) = match side {
                    Side::Base => (function.base_range.as_ref()?, &function.calls_before),
                    Side::Target => (function.target_range.as_ref()?, &function.calls_after),
                };
                Some(Caller {
                    path: importer,
                    range,
                    calls,
                })
            })
            .collect();
    }

    let (_, symbols) = revisions.side(side);
    symbols
        .file(importer)
        .into_iter()
        .flat_map(|file| file.functions.iter())
        .map(|function| Caller {
            path: importer,
            range: &function.range,
            calls: &function.calls,
        })
        .collect()
}

/// The root file's function a call points at, by the range that side
/// reported.
fn root_file_function(
    file: &FileResult,
    functions: &[FunctionResult],
    definition: &SourceRange,
    side: Side,
) -> Option<String> {
    let function = functions.iter().find(|function| {
        let range = match side {
            Side::Base => function.base_range.as_ref(),
            Side::Target => function.target_range.as_ref(),
        };
        range == Some(definition)
    })?;
    Some(function_node_id(file, function))
}

/// Add a `re_exports` edge for every module a reached module forwards from.
///
/// A re-export is a structural relationship of its own and one that breaks
/// callers when it goes away: a barrel module that stops forwarding a name
/// breaks every importer of that name. Both endpoints must be reached, for the
/// same reason an import edge needs both.
///
/// The evidence is the statement's own line, which is the `export ... from`
/// the import edge was built from as well.
fn add_re_export_edges(builder: &mut GraphBuilder, revisions: &Revisions<'_>, reached: &Reached) {
    for id in reached.keys() {
        let path = path_of(id);
        let mut forwarded: BTreeSet<&str> = BTreeSet::new();
        for side in [Side::Base, Side::Target] {
            let (imports, symbols) = revisions.side(side);
            let Some(file) = symbols.file(path) else {
                continue;
            };
            for specifier in &file.re_exported_modules {
                if let Some(target) = imports.resolve_specifier(path, specifier) {
                    forwarded.insert(target);
                }
            }
        }
        for target in forwarded {
            let to = Node::module_id(target);
            if !reached.contains_key(&to) {
                continue;
            }
            let in_base = forwards(revisions, Side::Base, path, target);
            let in_target = forwards(revisions, Side::Target, path, target);
            let Some(status) = EdgeStatus::from_membership(in_base, in_target) else {
                continue;
            };
            builder.add_edge(Edge {
                from: id.clone(),
                to,
                relation: Relation::ReExports,
                status,
                resolution: Resolution::ExportClause,
                evidence: import_evidence(path, target, status, revisions.base, revisions.target),
            });
        }
    }
}

/// Whether one side forwards names from `target` out of `path`.
fn forwards(revisions: &Revisions<'_>, side: Side, path: &str, target: &str) -> bool {
    let (imports, symbols) = revisions.side(side);
    symbols.file(path).is_some_and(|file| {
        file.re_exported_modules
            .iter()
            .any(|specifier| imports.resolve_specifier(path, specifier) == Some(target))
    })
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
///
/// Only the relations the request follows are indexed, so a walk that may not
/// follow guesses does not reach a function through one.
fn call_adjacency<'a>(
    calls: &'a CallEdges,
    relations: &[Relation],
) -> (Adjacency<'a>, Adjacency<'a>) {
    let mut outgoing: Adjacency<'_> = BTreeMap::new();
    let mut incoming: Adjacency<'_> = BTreeMap::new();
    for (from, to, relation) in calls.keys() {
        if !relations.contains(relation) {
            continue;
        }
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

/// Add an edge for every call relationship between functions the walk
/// reached.
///
/// Both endpoints must be reached, for the same reason an import edge needs
/// both: an edge to a function outside the walk would show a relationship the
/// request asked not to follow. A cycle's closing edge is among the reached
/// pairs, so it survives while the walk still terminates.
///
/// Each relationship keeps the relation it was resolved under, so a guess is
/// delivered as `possible_call` and is delivered at all only to a request
/// that named it.
fn add_call_edges(
    builder: &mut GraphBuilder,
    calls: &CallEdges,
    reached: &Reached,
    relations: &[Relation],
) {
    for ((from, to, relation), call) in calls {
        if !relations.contains(relation) {
            continue;
        }
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
            relation: *relation,
            status,
            resolution: call.resolution,
            // The site is the caller's file, which is not the root's file for
            // a call that reaches in from another module.
            evidence: line.map(|line| Evidence {
                file: call.site.clone(),
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
///
/// Only the root module's own functions are attached. A caller reached in
/// another file is declared by a module this graph does not describe, and
/// pulling that module in would answer the file-rooted question inside the
/// function-rooted one.
fn add_contains_edges(
    builder: &mut GraphBuilder,
    path: &str,
    statuses: &BTreeMap<String, NodeStatus>,
    reached: &Reached,
    places: &Places<'_>,
    local: &BTreeSet<String>,
) {
    builder.add_node(module_node(path, statuses, 1));
    for id in reached.keys() {
        if !local.contains(id) {
            continue;
        }
        let Some(Placed::Analyzed { function, .. }) = places.get(id) else {
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
        group: None,
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
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::LazyLock,
    };

    use super::{Request, Revisions, build};
    use crate::{
        FileStatus,
        analysis::{FunctionChangeStatus, FunctionChurn, MatchConfidence},
        graph::{
            DEFAULT_DEPTH, DEFAULT_MAX_EDGES, Direction, Edge, EdgeStatus, Evidence, Graph, Group,
            GroupRole, Limits, Node, NodeKind, NodeStatus, Relation, Resolution, TruncationReason,
            View, render,
        },
        imports::{FileSymbols, ImportIndex, SymbolIndex},
        languages::{
            CallSite, ExportedSymbol, FunctionDefinition, FunctionKind, ImportedName, Language,
            SourceRange,
        },
        metrics::FunctionMetrics,
        query,
        result::{
            AnalysisResult, AnalysisSummary, FileResult, FunctionResult, RevisionResult,
            SCHEMA_VERSION,
        },
    };

    /// A revision that was never parsed for symbols.
    ///
    /// Module-rooted graphs and local call graphs resolve nothing across
    /// files, so their tests say so by handing over an index covering no file.
    static NO_SYMBOLS: LazyLock<SymbolIndex> = LazyLock::new(SymbolIndex::default);

    fn revisions<'a>(base: &'a ImportIndex, target: &'a ImportIndex) -> Revisions<'a> {
        Revisions {
            base,
            target,
            base_symbols: &NO_SYMBOLS,
            target_symbols: &NO_SYMBOLS,
        }
    }

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
                receiver: None,
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

    /// The edge one group states about a module.
    fn group_edge<'a>(
        graph: &'a Graph,
        group: &str,
        to: &str,
        resolution: Resolution,
    ) -> Option<&'a Edge> {
        graph.edges().iter().find(|edge| {
            edge.from == group && edge.to == Node::module_id(to) && edge.resolution == resolution
        })
    }

    /// The group node one role is addressed by, and what it collapses.
    fn group(graph: &Graph, role: GroupRole) -> Option<&Group> {
        graph
            .node(&Node::group_id(role))
            .and_then(|node| node.group.as_ref())
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
            &revisions(&base, &target),
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
            &revisions(&index, &index),
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
        let graph = build(&result, &revisions(&index, &index), &upstream);
        assert_eq!(node_paths(&graph), vec!["src/app.ts", "src/core.ts"]);

        let mut downstream = request(&roots, Relation::SUPPORTED);
        downstream.direction = Direction::Downstream;
        let graph = build(&result, &revisions(&index, &index), &downstream);
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
        let graph = build(&result, &revisions(&index, &index), &one_hop);
        assert_eq!(node_paths(&graph), vec!["src/a.ts", "src/b.ts"]);

        let mut two_hops = one_hop;
        two_hops.depth = 2;
        let graph = build(&result, &revisions(&index, &index), &two_hops);
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
            &revisions(&index, &index),
            &request(&roots, Relation::SUPPORTED),
        );

        assert_eq!(node_paths(&graph), vec!["src/a.ts", "src/b.ts"]);
        assert!(edge(&graph, "src/a.ts", "src/b.ts").is_some());
        assert!(edge(&graph, "src/b.ts", "src/a.ts").is_some());
    }

    #[test]
    fn a_built_cycle_renders_inside_a_subgraph_identically_across_builds() {
        let index = ImportIndex::from_edges(
            &[
                ("src/a.ts", "src/b.ts"),
                ("src/b.ts", "src/c.ts"),
                ("src/c.ts", "src/a.ts"),
            ],
            &[],
        );
        let result = analysis(vec![modified("src/a.ts")]);
        let roots = paths_of(&["src/a.ts"]);
        let build_graph = || {
            let mut deep = request(&roots, Relation::SUPPORTED);
            deep.depth = 2;
            build(&result, &revisions(&index, &index), &deep)
        };

        let graph = build_graph();
        // The walk terminated, and the loop it closed is one cycle over the
        // three modules it runs through.
        assert_eq!(node_paths(&graph), vec!["src/a.ts", "src/b.ts", "src/c.ts"]);
        let cycles = graph.cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].size(), 3);

        let drawn = render::mermaid(&graph);
        assert!(
            drawn.contains(concat!(
                "    subgraph c0[\"cycle · 3 nodes\"]\n",
                "        n0[\"a.ts · modified\"]\n",
                "        n1[\"b.ts\"]\n",
                "        n2[\"c.ts\"]\n",
                "    end\n",
            )),
            "the loop is drawn as one box: {drawn}"
        );
        assert_eq!(drawn, render::mermaid(&build_graph()));
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
            &revisions(&index, &index),
            &request(&roots, &[Relation::TestedBy]),
        );

        // The two tests are one group, and each resolution is a different claim
        // about the module, so each keeps its own edge and its own confidence.
        let tests = Node::group_id(GroupRole::Tests);
        let imports = group_edge(&graph, &tests, "src/core.ts", Resolution::TestImportsModule)
            .expect("a test imports the module");
        assert_eq!(imports.relation, Relation::TestedBy);
        assert_eq!(imports.status, EdgeStatus::Unchanged);
        assert!((imports.confidence() - 0.9).abs() < f64::EPSILON);

        let convention = group_edge(
            &graph,
            &tests,
            "src/core.ts",
            Resolution::TestNameMatchesModule,
        )
        .expect("a test is named after the module");
        assert!((convention.confidence() - 0.8).abs() < f64::EPSILON);

        // The group sits one hop past the module its tests cover.
        let group = graph.node(&tests).expect("the tests are a node");
        assert_eq!(group.depth, 1);
        assert_eq!(group.status, NodeStatus::Unchanged);
    }

    #[test]
    fn relations_without_tested_by_leave_the_tests_out() {
        let index = ImportIndex::from_edges(&[("src/__tests__/core.spec.ts", "src/core.ts")], &[]);
        let result = analysis(vec![modified("src/core.ts")]);
        let roots = paths_of(&["src/core.ts"]);

        let graph = build(
            &result,
            &revisions(&index, &index),
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

        let graph = build(
            &result,
            &revisions(&index, &index),
            &request(&[], Relation::SUPPORTED),
        );

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
            &revisions(&index, &index),
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
            &revisions(&index, &index),
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
            &revisions(&index, &index),
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
            &revisions(&index, &index),
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
            &revisions(&index, &index),
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
            &revisions(&index, &index),
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
        let graph = build(&result, &revisions(&index, &index), &downstream);
        assert_eq!(labels(&graph), vec!["a.ts", "root", "helper"]);
        assert!(call_edge(&graph, &root_id, &helper_id).is_some());
        assert!(call_edge(&graph, &caller_id, &root_id).is_none());

        let mut two_hops = downstream;
        two_hops.depth = 2;
        let graph = build(&result, &revisions(&index, &index), &two_hops);
        assert_eq!(labels(&graph), vec!["a.ts", "root", "helper", "leaf"]);
        assert!(call_edge(&graph, &helper_id, &leaf_id).is_some());

        let mut upstream = function_request(&root_id, Relation::SUPPORTED, 1);
        upstream.direction = Direction::Upstream;
        let graph = build(&result, &revisions(&index, &index), &upstream);
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
            &revisions(&index, &index),
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
            &revisions(&index, &index),
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
            &revisions(&index, &index),
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
            &revisions(&index, &index),
            &function_request(
                "src/a.ts#fn:absent@target:1:0",
                Relation::SUPPORTED,
                DEFAULT_DEPTH,
            ),
        );

        assert!(graph.nodes().is_empty());
        assert!(graph.edges().is_empty());
    }

    // ------------------------------------------------- cross-file calls ---

    /// A definition as a revision's symbol index records it, with the calls
    /// its body writes.
    fn definition(name: &str, line: u32, sites: Vec<CallSite>) -> FunctionDefinition {
        FunctionDefinition {
            language: Language::TypeScript,
            kind: FunctionKind::Function,
            qualified_name: name.to_owned(),
            range: range(line),
            metrics: FunctionMetrics {
                physical_loc: 1,
                source_loc: 1,
                cyclomatic_complexity: 1,
                cognitive_complexity: 0,
            },
            calls: sites,
            body_hash: 0,
        }
    }

    fn site(name: &str, receiver: Option<&str>, line: u32) -> CallSite {
        CallSite {
            name: name.to_owned(),
            receiver: receiver.map(str::to_owned),
            line,
        }
    }

    /// A file that declares one exported function, and may call others.
    fn declares(name: &str, line: u32, sites: Vec<CallSite>) -> FileSymbols {
        FileSymbols {
            exports: BTreeMap::from([(
                name.to_owned(),
                ExportedSymbol {
                    local: name.to_owned(),
                    from: None,
                    function: Some(range(line)),
                    line,
                },
            )]),
            re_exported_modules: BTreeSet::new(),
            functions: vec![definition(name, line, sites)],
        }
    }

    /// A barrel module forwarding one name from another.
    fn barrel(exported: &str, local: &str, specifier: &str) -> FileSymbols {
        FileSymbols {
            exports: BTreeMap::from([(
                exported.to_owned(),
                ExportedSymbol {
                    local: local.to_owned(),
                    from: Some(specifier.to_owned()),
                    function: None,
                    line: 1,
                },
            )]),
            re_exported_modules: BTreeSet::from([specifier.to_owned()]),
            functions: Vec::new(),
        }
    }

    /// The changed file every cross-file test is rooted in: `src/a.ts`
    /// declaring `root`, which writes the calls given.
    fn rooted(sites_before: &[CallSite], sites_after: &[CallSite]) -> FileResult {
        let mut function = function(
            "function-1",
            "root",
            FunctionChangeStatus::Modified,
            Some(1),
            Some(1),
            &[],
            &[],
        );
        function.calls_before = sites_before.to_vec();
        function.calls_after = sites_after.to_vec();
        modified_with("src/a.ts", vec![function])
    }

    /// The symbol index of a revision where `src/a.ts` declares `root`.
    fn with_root(files: &[(&str, FileSymbols)]) -> SymbolIndex {
        let mut symbols = SymbolIndex::default();
        symbols.insert("src/a.ts", declares("root", 1, Vec::new()));
        for (path, file) in files {
            symbols.insert(path, file.clone());
        }
        symbols
    }

    /// The identity an external definition is addressed by.
    fn external_id(path: &str, name: &str, side: &str, line: u32) -> String {
        format!("{path}#fn:{name}@{side}:{line}:0")
    }

    #[test]
    fn a_named_import_called_directly_resolves_exactly() {
        let file = rooted(&[], &[site("strip", None, 7)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut index = ImportIndex::default();
        index.bind(
            "src/a.ts",
            "strip",
            ImportedName::Named("stripComments".to_owned()),
            "./parse",
            "src/parse.ts",
            2,
        );
        let symbols = with_root(&[("src/parse.ts", declares("stripComments", 12, Vec::new()))]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let callee = external_id("src/parse.ts", "stripComments", "target", 12);
        let edge = call_edge(&graph, &root_id, &callee).expect("the imported call resolves");
        assert_eq!(edge.resolution, Resolution::ImportedSymbol);
        assert!((edge.confidence() - 1.0).abs() < f64::EPSILON);
        // The call is only in the target, and its site is the caller's file.
        assert_eq!(edge.status, EdgeStatus::Added);
        assert_eq!(
            edge.evidence
                .as_ref()
                .map(|evidence| evidence.file.as_str()),
            Some("src/a.ts")
        );

        let node = function_node(&graph, &callee).expect("the callee is a node");
        assert_eq!(node.label, "stripComments");
        assert_eq!(node.path, "src/parse.ts");
        // A file the diff does not contain is unchanged, which is all
        // membership can say about it.
        assert_eq!(node.status, NodeStatus::Unchanged);
    }

    #[test]
    fn a_renamed_import_resolves_to_the_name_the_module_exports() {
        let file = rooted(&[], &[site("b", None, 4)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut index = ImportIndex::default();
        index.bind(
            "src/a.ts",
            "b",
            ImportedName::Named("a".to_owned()),
            "./x",
            "src/x.ts",
            1,
        );
        let symbols = with_root(&[("src/x.ts", declares("a", 3, Vec::new()))]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let callee = external_id("src/x.ts", "a", "target", 3);
        assert!(call_edge(&graph, &root_id, &callee).is_some());
        assert_eq!(
            function_node(&graph, &callee).map(|node| node.label.as_str()),
            Some("a")
        );
    }

    #[test]
    fn a_default_import_resolves_to_the_default_export() {
        let file = rooted(&[], &[site("compile", None, 6)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut index = ImportIndex::default();
        index.bind(
            "src/a.ts",
            "compile",
            ImportedName::Default,
            "./compiler",
            "src/compiler.ts",
            1,
        );
        let mut compiler = declares("compileStyle", 9, Vec::new());
        compiler.exports = BTreeMap::from([(
            "default".to_owned(),
            ExportedSymbol {
                local: "compileStyle".to_owned(),
                from: None,
                function: Some(range(9)),
                line: 9,
            },
        )]);
        let symbols = with_root(&[("src/compiler.ts", compiler)]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let callee = external_id("src/compiler.ts", "compileStyle", "target", 9);
        assert!(call_edge(&graph, &root_id, &callee).is_some());
    }

    #[test]
    fn a_namespace_import_resolves_only_through_a_named_property() {
        let file = rooted(
            &[],
            &[site("parse", Some("ns"), 5), site("run", Some("other"), 6)],
        );
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut index = ImportIndex::default();
        index.bind(
            "src/a.ts",
            "ns",
            ImportedName::Namespace,
            "./parse",
            "src/parse.ts",
            1,
        );
        let symbols = with_root(&[("src/parse.ts", declares("parse", 4, Vec::new()))]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        // `ns.parse()` names a module whose exports are known, so it resolves.
        let callee = external_id("src/parse.ts", "parse", "target", 4);
        assert!(call_edge(&graph, &root_id, &callee).is_some());
        // `other.run()` names a receiver that is not an import, so it resolves
        // to nothing rather than to a guess.
        assert_eq!(graph.edges().len(), 2);
    }

    #[test]
    fn a_re_export_chain_resolves_less_confidently_than_a_direct_import() {
        let file = rooted(&[], &[site("strip", None, 7)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut index = ImportIndex::default();
        index.bind(
            "src/a.ts",
            "strip",
            ImportedName::Named("stripComments".to_owned()),
            "./index",
            "src/index.ts",
            1,
        );
        index.forward("src/index.ts", "./parse", "src/parse.ts", 1);
        let symbols = with_root(&[
            (
                "src/index.ts",
                barrel("stripComments", "stripComments", "./parse"),
            ),
            ("src/parse.ts", declares("stripComments", 30, Vec::new())),
        ]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let callee = external_id("src/parse.ts", "stripComments", "target", 30);
        let edge = call_edge(&graph, &root_id, &callee).expect("the chain resolves");
        assert_eq!(edge.resolution, Resolution::ReExportedSymbol);
        assert!((edge.confidence() - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn a_name_reached_only_through_a_whole_module_re_export_produces_no_edge() {
        let file = rooted(&[], &[site("strip", None, 7)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut index = ImportIndex::default();
        index.bind(
            "src/a.ts",
            "strip",
            ImportedName::Named("stripComments".to_owned()),
            "./index",
            "src/index.ts",
            1,
        );
        index.forward("src/index.ts", "./parse", "src/parse.ts", 1);
        let star = FileSymbols {
            exports: BTreeMap::new(),
            re_exported_modules: BTreeSet::from(["./parse".to_owned()]),
            functions: Vec::new(),
        };
        let symbols = with_root(&[
            ("src/index.ts", star),
            ("src/parse.ts", declares("stripComments", 30, Vec::new())),
        ]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        // `export *` forwards names from a file the scan of the barrel never
        // read, so the call stays unresolved rather than being guessed at.
        assert!(
            graph
                .edges()
                .iter()
                .all(|edge| edge.relation != Relation::Calls)
        );
    }

    #[test]
    fn a_specifier_this_revision_does_not_have_produces_no_edge() {
        let file = rooted(&[], &[site("debounce", None, 7)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut index = ImportIndex::default();
        // An installed package: the binding exists, the specifier names no
        // file in this revision.
        index.bind_external(
            "src/a.ts",
            "debounce",
            ImportedName::Named("debounce".to_owned()),
            "lodash",
            1,
        );
        let symbols = with_root(&[]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        assert!(
            graph
                .edges()
                .iter()
                .all(|edge| edge.relation != Relation::Calls)
        );
    }

    #[test]
    fn a_caller_the_change_removed_is_a_removed_edge() {
        let file = rooted(&[], &[]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut base = ImportIndex::default();
        base.bind(
            "src/caller.ts",
            "root",
            ImportedName::Named("root".to_owned()),
            "./a",
            "src/a.ts",
            1,
        );
        let target = ImportIndex::default();
        let base_symbols = with_root(&[(
            "src/caller.ts",
            declares("caller", 5, vec![site("root", None, 6)]),
        )]);
        let target_symbols = with_root(&[]);
        let revisions = Revisions {
            base: &base,
            target: &target,
            base_symbols: &base_symbols,
            target_symbols: &target_symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let caller = external_id("src/caller.ts", "caller", "base", 5);
        let edge = call_edge(&graph, &caller, &root_id).expect("the removed caller is an edge");
        assert_eq!(edge.status, EdgeStatus::Removed);
        assert_eq!(
            edge.evidence
                .as_ref()
                .map(|evidence| evidence.file.as_str()),
            Some("src/caller.ts")
        );
        let node = function_node(&graph, &caller).expect("the caller is a node");
        assert_eq!(node.status, NodeStatus::Removed);
    }

    #[test]
    fn only_direct_importers_are_read_for_callers() {
        let file = rooted(&[], &[]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut index = ImportIndex::default();
        index.bind(
            "src/direct.ts",
            "root",
            ImportedName::Named("root".to_owned()),
            "./a",
            "src/a.ts",
            1,
        );
        // An indirect importer that writes the same call: it imports the
        // direct importer, not the changed file.
        index.bind(
            "src/indirect.ts",
            "root",
            ImportedName::Named("root".to_owned()),
            "./direct",
            "src/direct.ts",
            1,
        );
        let symbols = with_root(&[
            (
                "src/direct.ts",
                declares("direct", 5, vec![site("root", None, 6)]),
            ),
            (
                "src/indirect.ts",
                declares("indirect", 5, vec![site("root", None, 6)]),
            ),
        ]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        assert!(
            call_edge(
                &graph,
                &external_id("src/direct.ts", "direct", "target", 5),
                &root_id
            )
            .is_some()
        );
        // Through a barrel module almost every file reaches almost every
        // other, so a caller list built from indirect importers is not a
        // caller list.
        assert!(
            function_node(
                &graph,
                &external_id("src/indirect.ts", "indirect", "target", 5)
            )
            .is_none()
        );
    }

    #[test]
    fn a_module_that_forwards_another_reports_a_re_exports_edge() {
        let result = analysis(vec![modified("src/index.ts")]);
        let mut index = ImportIndex::default();
        index.forward("src/index.ts", "./parse", "src/parse.ts", 3);
        let symbols = {
            let mut symbols = SymbolIndex::default();
            symbols.insert(
                "src/index.ts",
                barrel("stripComments", "stripComments", "./parse"),
            );
            symbols.insert("src/parse.ts", declares("stripComments", 30, Vec::new()));
            symbols
        };
        let empty = SymbolIndex::default();
        let base = ImportIndex::default();
        let revisions = Revisions {
            base: &base,
            target: &index,
            base_symbols: &empty,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &request(&paths_of(&["src/index.ts"]), Relation::SUPPORTED),
        );

        let edge = graph
            .edges()
            .iter()
            .find(|edge| edge.relation == Relation::ReExports)
            .expect("the forwarding is an edge");
        assert_eq!(edge.from, Node::module_id("src/index.ts"));
        assert_eq!(edge.to, Node::module_id("src/parse.ts"));
        assert_eq!(edge.resolution, Resolution::ExportClause);
        // Only the target revision forwards it, so the relationship is added.
        assert_eq!(edge.status, EdgeStatus::Added);
        assert_eq!(
            edge.evidence.as_ref().map(|evidence| evidence.line),
            Some(3)
        );
    }

    // ------------------------------------------------- heuristic calls ---

    /// The `possible_call` edge between two published function identities.
    fn guessed_edge<'a>(graph: &'a Graph, from: &str, to: &str) -> Option<&'a Edge> {
        graph.edges().iter().find(|edge| {
            edge.relation == Relation::PossibleCall
                && edge.from == Node::function_id(from)
                && edge.to == Node::function_id(to)
        })
    }

    /// An index in which `src/a.ts` calls a method on a default import of
    /// every module given, which no exact rule can resolve.
    fn imports_objects(modules: &[&str]) -> ImportIndex {
        let mut index = ImportIndex::default();
        for (position, module) in modules.iter().enumerate() {
            index.bind(
                "src/a.ts",
                &format!("handlers{position}"),
                ImportedName::Default,
                &format!("./{position}"),
                module,
                2,
            );
        }
        index
    }

    #[test]
    fn a_property_call_matching_one_function_is_a_possible_call_at_half_confidence() {
        let file = rooted(&[], &[site("stripComments", Some("handlers0"), 7)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let index = imports_objects(&["src/parse.ts"]);
        let symbols = with_root(&[("src/parse.ts", declares("stripComments", 12, Vec::new()))]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let callee = external_id("src/parse.ts", "stripComments", "target", 12);
        let edge = guessed_edge(&graph, &root_id, &callee).expect("one name, one candidate");
        assert_eq!(edge.resolution, Resolution::PropertyNameMatch);
        assert!((edge.confidence() - 0.5).abs() < f64::EPSILON);
        // Only the target side writes the call, and the site is the caller's
        // own file.
        assert_eq!(edge.status, EdgeStatus::Added);
        assert_eq!(
            edge.evidence.as_ref().map(|evidence| evidence.line),
            Some(7)
        );
        // A guess never doubles as a proof.
        assert!(call_edge(&graph, &root_id, &callee).is_none());
    }

    #[test]
    fn a_property_name_two_modules_share_produces_nothing() {
        let file = rooted(&[], &[site("stripComments", Some("handlers0"), 7)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let index = imports_objects(&["src/parse.ts", "src/legacy.ts"]);
        let symbols = with_root(&[
            ("src/parse.ts", declares("stripComments", 12, Vec::new())),
            ("src/legacy.ts", declares("stripComments", 30, Vec::new())),
        ]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        // Two candidates are no answer: naming one of them would be a coin
        // toss a reader cannot check.
        assert!(
            graph
                .edges()
                .iter()
                .all(|edge| edge.relation != Relation::PossibleCall)
        );
        assert!(
            function_node(
                &graph,
                &external_id("src/parse.ts", "stripComments", "target", 12)
            )
            .is_none()
        );
        assert!(
            function_node(
                &graph,
                &external_id("src/legacy.ts", "stripComments", "target", 30)
            )
            .is_none()
        );
    }

    #[test]
    fn a_guess_is_delivered_only_to_a_request_that_names_it() {
        let file = rooted(&[], &[site("stripComments", Some("handlers0"), 7)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let index = imports_objects(&["src/parse.ts"]);
        let symbols = with_root(&[("src/parse.ts", declares("stripComments", 12, Vec::new()))]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };
        let callee = external_id("src/parse.ts", "stripComments", "target", 12);

        let default = Relation::by_default();
        let proven = build(
            &result,
            &revisions,
            &function_request(&root_id, &default, DEFAULT_DEPTH),
        );
        assert!(
            proven
                .edges()
                .iter()
                .all(|edge| edge.relation != Relation::PossibleCall)
        );
        // The guessed callee is not in the graph at all: nothing proven
        // reaches it.
        assert!(function_node(&proven, &callee).is_none());

        let asked = build(
            &result,
            &revisions,
            &function_request(&root_id, &[Relation::PossibleCall], DEFAULT_DEPTH),
        );
        assert!(guessed_edge(&asked, &root_id, &callee).is_some());
    }

    #[test]
    fn a_property_call_in_an_importer_is_a_possible_caller_of_the_root() {
        let file = rooted(&[], &[]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let mut index = ImportIndex::default();
        index.bind(
            "src/direct.ts",
            "core",
            ImportedName::Default,
            "./a",
            "src/a.ts",
            1,
        );
        let symbols = with_root(&[(
            "src/direct.ts",
            declares("direct", 5, vec![site("root", Some("core"), 6)]),
        )]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };

        let graph = build(
            &result,
            &revisions,
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        let caller = external_id("src/direct.ts", "direct", "target", 5);
        let edge = guessed_edge(&graph, &caller, &root_id).expect("the importer may call the root");
        assert_eq!(edge.resolution, Resolution::PropertyNameMatch);
        // The site is the importing file, not the root's.
        assert_eq!(
            edge.evidence
                .as_ref()
                .map(|evidence| evidence.file.as_str()),
            Some("src/direct.ts")
        );
        assert!(call_edge(&graph, &caller, &root_id).is_none());
    }

    #[test]
    fn a_guessed_graph_is_identical_across_two_builds_of_one_input() {
        let file = rooted(&[], &[site("stripComments", Some("handlers0"), 7)]);
        let root_id = published_id(&file, 0);
        let result = analysis(vec![file]);
        let index = imports_objects(&["src/parse.ts"]);
        let symbols = with_root(&[("src/parse.ts", declares("stripComments", 12, Vec::new()))]);
        let revisions = Revisions {
            base: &index,
            target: &index,
            base_symbols: &symbols,
            target_symbols: &symbols,
        };
        let build_once = || {
            let graph = build(
                &result,
                &revisions,
                &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
            );
            (
                graph.edges().to_vec(),
                render::dependency_diff(&graph),
                render::mermaid(&graph),
            )
        };

        let once = build_once();
        assert_eq!(build_once(), once);
        assert!(once.1.contains("-[possible_call]->"));
        assert!(once.2.contains("-. \"~0.5\" .->"));
    }

    /// One index in which every path imports `src/core.ts`.
    fn importers_of_core(importers: &[String]) -> ImportIndex {
        let edges = importers
            .iter()
            .map(|importer| (importer.as_str(), "src/core.ts"))
            .collect::<Vec<_>>();
        ImportIndex::from_edges(&edges, &[])
    }

    fn importer_paths(count: u32, prefix: &str) -> Vec<String> {
        (0..count)
            .map(|number| format!("src/{prefix}{number:02}.ts"))
            .collect()
    }

    /// A request with a node budget of its own.
    fn bounded_request<'a>(
        roots: &'a [String],
        relations: &'a [Relation],
        max_nodes: usize,
    ) -> Request<'a> {
        Request {
            limits: Limits {
                max_nodes,
                max_edges: DEFAULT_MAX_EDGES,
            },
            ..request(roots, relations)
        }
    }

    #[test]
    fn a_budget_replaces_the_overflow_with_one_group_that_counts_it() {
        let importers = importer_paths(40, "importer");
        let index = importers_of_core(&importers);
        let result = analysis(vec![modified("src/core.ts")]);
        let roots = paths_of(&["src/core.ts"]);

        let graph = build(
            &result,
            &revisions(&index, &index),
            &bounded_request(&roots, Relation::SUPPORTED, 10),
        );

        // The root and forty importers are forty-one nodes; a budget of ten
        // admits nine of them and spends its last slot on the group that
        // stands for the other thirty-two.
        assert_eq!(graph.nodes().len(), 10);
        assert_eq!(
            group(&graph, GroupRole::Callers),
            Some(&Group {
                size: 32,
                role: GroupRole::Callers,
            })
        );
        // The collapsed nodes are what the budget cost, and a caller that wants
        // them can raise it.
        assert!(graph.truncated());
        assert_eq!(graph.omitted_nodes(), 32);
        assert_eq!(graph.reasons(), [TruncationReason::MaxNodes]);

        let again = build(
            &result,
            &revisions(&index, &index),
            &bounded_request(&roots, Relation::SUPPORTED, 10),
        );
        assert_eq!(graph.nodes(), again.nodes());
        assert_eq!(graph.edges(), again.edges());
    }

    #[test]
    fn tests_collapse_into_one_group_although_the_budget_would_have_taken_them() {
        let index = ImportIndex::from_edges(
            &[
                ("src/__tests__/core.spec.ts", "src/core.ts"),
                ("src/__tests__/parse.spec.ts", "src/core.ts"),
                ("src/__tests__/render.spec.ts", "src/core.ts"),
            ],
            &[],
        );
        let result = analysis(vec![modified("src/core.ts")]);
        let roots = paths_of(&["src/core.ts"]);

        let graph = build(
            &result,
            &revisions(&index, &index),
            &request(&roots, Relation::SUPPORTED),
        );

        // The default budget is thirty; the three tests are grouped because
        // they are context, not because they did not fit.
        assert!(!graph.truncated());
        assert_eq!(node_paths(&graph), vec!["src/core.ts", "src"]);
        assert_eq!(
            group(&graph, GroupRole::Tests),
            Some(&Group {
                size: 3,
                role: GroupRole::Tests,
            })
        );
        let tested_by = graph
            .edges()
            .iter()
            .filter(|edge| edge.relation == Relation::TestedBy)
            .collect::<Vec<_>>();
        assert_eq!(tested_by.len(), 1);
        assert_eq!(tested_by[0].from, Node::group_id(GroupRole::Tests));
        assert_eq!(tested_by[0].to, Node::module_id("src/core.ts"));
    }

    #[test]
    fn a_classification_collapses_what_a_shared_name_does_not() {
        let importers = paths_of(&[
            "dist/parse.ts",
            "coverage/parse.ts",
            "node_modules/left-pad/parse.ts",
            "vendor/parse.ts",
            "src/parse.ts",
        ]);
        let index = importers_of_core(&importers);
        let result = analysis(vec![modified("src/core.ts")]);
        let roots = paths_of(&["src/core.ts"]);

        let graph = build(
            &result,
            &revisions(&index, &index),
            &request(&roots, Relation::SUPPORTED),
        );

        assert_eq!(
            group(&graph, GroupRole::Generated).map(|group| group.size),
            Some(2)
        );
        assert_eq!(
            group(&graph, GroupRole::Vendored).map(|group| group.size),
            Some(2)
        );
        // The source file shares its name with two collapsed ones and stays a
        // place a reader can open.
        assert!(node(&graph, "src/parse.ts").is_some());
        assert!(node(&graph, "dist/parse.ts").is_none());
        assert!(node(&graph, "vendor/parse.ts").is_none());
    }

    #[test]
    fn a_group_outlives_the_changed_nodes_it_replaced() {
        let importers = importer_paths(10, "changed");
        let index = importers_of_core(&importers);
        let mut files = vec![modified("src/core.ts")];
        files.extend(importers.iter().map(|path| modified(path)));
        let result = analysis(files);
        let roots = paths_of(&["src/core.ts"]);

        let graph = build(
            &result,
            &revisions(&index, &index),
            &bounded_request(&roots, Relation::SUPPORTED, 5),
        );

        // Every importer is changed, so truncation on its own would have kept
        // them and dropped the group. Grouping runs first, so the group is
        // there and the nodes it replaced are not.
        assert_eq!(graph.nodes().len(), 5);
        let collapsed = group(&graph, GroupRole::Callers).expect("the overflow is one group");
        assert_eq!(collapsed.size, 7);
        assert_eq!(
            graph
                .nodes()
                .iter()
                .filter(|node| node.kind == NodeKind::Module && node.path != "src/core.ts")
                .count(),
            3
        );
        // Every member was modified, so the group says so too.
        assert_eq!(
            graph
                .node(&Node::group_id(GroupRole::Callers))
                .map(|node| node.status),
            Some(NodeStatus::Modified)
        );
    }

    #[test]
    fn a_grouped_graph_renders_identically_from_the_same_input() {
        let importers = importer_paths(12, "importer");
        let index = importers_of_core(&importers);
        let result = analysis(vec![modified("src/core.ts")]);
        let roots = paths_of(&["src/core.ts"]);
        let render_once = || {
            let graph = build(
                &result,
                &revisions(&index, &index),
                &bounded_request(&roots, Relation::SUPPORTED, 6),
            );
            let label = graph
                .node(&Node::group_id(GroupRole::Callers))
                .expect("the overflow is one group")
                .label
                .clone();
            (
                label,
                render::dependency_diff(&graph),
                render::mermaid(&graph),
            )
        };

        let (label, diff, mermaid) = render_once();
        assert_eq!(
            render_once(),
            (label.clone(), diff.clone(), mermaid.clone())
        );
        // Both renderings carry the group rather than quietly leaving a gap.
        assert!(diff.contains(&label));
        assert!(mermaid.contains(&label));
    }
    #[test]
    fn completeness_counts_scan_and_specifier_gaps_in_the_walk_scope() {
        let mut index = ImportIndex::from_edges(&[], &["src/core.ts"]);
        index.truncate("src/core.ts");
        index.bind_external(
            "src/core.ts",
            "package",
            ImportedName::Default,
            "package",
            1,
        );
        let result = analysis(vec![modified("src/core.ts")]);
        let roots = paths_of(&["src/core.ts"]);

        let graph = build(
            &result,
            &revisions(&index, &index),
            &request(&roots, Relation::SUPPORTED),
        );

        assert_eq!(graph.completeness().scan_truncated_files, 2);
        assert_eq!(graph.completeness().unresolved_specifiers, 2);
        assert_eq!(
            graph.completeness().relations_supported,
            Relation::SUPPORTED
        );
    }

    #[test]
    fn completeness_counts_only_unresolved_call_sites() {
        let root = function(
            "root",
            "root",
            FunctionChangeStatus::Added,
            None,
            Some(1),
            &[],
            &[("missing", 2), ("helper", 3)],
        );
        let helper = function(
            "helper",
            "helper",
            FunctionChangeStatus::Added,
            None,
            Some(10),
            &[],
            &[],
        );
        let result = analysis(vec![modified_with("src/core.ts", vec![root, helper])]);
        let root_id = query::function_id(&result.files[0], &result.files[0].functions[0]);
        let index = ImportIndex::from_edges(&[], &["src/core.ts"]);

        let graph = build(
            &result,
            &revisions(&index, &index),
            &function_request(&root_id, Relation::SUPPORTED, DEFAULT_DEPTH),
        );

        assert_eq!(graph.completeness().unresolved_calls, 1);
    }
}
