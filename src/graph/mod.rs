//! The shape of what a change touches, as a graph of one comparison's
//! relationships.
//!
//! Everything else in `DiffScope` answers "how large is this change?" with a
//! number attached to something the diff contains. This module answers "what
//! does it reach, and what changed about that?" — which is a question about
//! relationships, and therefore about both revisions rather than one.
//!
//! The model holds nodes, edges, and the rules that make a graph reproducible:
//! identities derived from paths and function identities, statuses taken from
//! membership or from the analysis, a bounded walk, a fixed ordering, and a
//! fixed truncation preference. It knows nothing about serialization, Git, the
//! filesystem, or any transport: building a graph from an analysis lives in
//! [`build`], turning one into text in [`render`], and deciding whether it is
//! worth drawing in [`recommend`].

pub mod build;
pub mod recommend;
pub mod render;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Hops from the root walked when a caller does not ask for a depth.
///
/// One level of callers and one of dependencies is what a reviewer reads; a
/// deeper walk is available by request rather than by default, because depth
/// grows a graph far faster than it grows what the graph says.
pub const DEFAULT_DEPTH: u32 = 1;
/// Shallowest walk that can say anything: the root's direct relationships.
pub const MIN_DEPTH: u32 = 1;
/// Deepest walk offered. Beyond three hops a module graph reaches most of a
/// package and stops describing the change.
pub const MAX_DEPTH: u32 = 3;

/// Nodes delivered when a caller does not ask for a budget.
pub const DEFAULT_MAX_NODES: usize = 30;
pub const MIN_MAX_NODES: usize = 3;
pub const MAX_MAX_NODES: usize = 100;

/// Edges delivered when a caller does not ask for a budget.
pub const DEFAULT_MAX_EDGES: usize = 60;
pub const MIN_MAX_EDGES: usize = 3;
pub const MAX_MAX_EDGES: usize = 200;

/// What a relationship can point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    Module,
    Function,
    /// Several collapsed nodes, standing in for what a budget dropped.
    Group,
}

impl NodeKind {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Function => "function",
            Self::Group => "group",
        }
    }
}

/// What a group node stands for.
///
/// A role fixes both the identity a group is addressed by and the noun its
/// label counts in, so a collapse is reproducible and no rendering has to
/// invent prose for it. The order is the order the grouping rules are applied
/// in: context first, then what a reader does not review, then what a budget
/// could not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupRole {
    /// A module's related tests.
    Tests,
    /// Machine-written files.
    Generated,
    /// Files owned by another project.
    Vendored,
    /// Dependency lock files.
    Lockfile,
    /// What reaches the root and did not fit the node budget.
    Callers,
    /// What the root reaches and did not fit the node budget.
    Dependencies,
}

impl GroupRole {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Tests => "tests",
            Self::Generated => "generated",
            Self::Vendored => "vendored",
            Self::Lockfile => "lockfile",
            Self::Callers => "callers",
            Self::Dependencies => "dependencies",
        }
    }

    /// The noun a group of `size` members is counted in.
    ///
    /// Fixed per role and per number rather than derived from a name, because
    /// a label is part of a rendering a caller compares byte for byte and a
    /// written description is neither reproducible nor checkable.
    #[must_use]
    pub fn noun(self, size: u32) -> &'static str {
        let one = size == 1;
        match self {
            Self::Tests if one => "test",
            Self::Tests => "tests",
            Self::Generated if one => "generated file",
            Self::Generated => "generated files",
            Self::Vendored if one => "vendored file",
            Self::Vendored => "vendored files",
            Self::Lockfile if one => "lockfile",
            Self::Lockfile => "lockfiles",
            Self::Callers if one => "caller",
            Self::Callers => "callers",
            Self::Dependencies if one => "dependency",
            Self::Dependencies => "dependencies",
        }
    }
}

/// What one group node collapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    /// How many nodes this one stands for.
    pub size: u32,
    pub role: GroupRole,
}

impl Group {
    /// The text a rendering shows: the count, what was collapsed, and the
    /// change area the members share when they share one.
    #[must_use]
    pub fn label(self, area: Option<&str>) -> String {
        let counted = format!("{} {}", self.size, self.role.noun(self.size));
        match area {
            Some(area) => format!("{counted} · {area}"),
            None => counted,
        }
    }
}

/// Whether a node is in one revision, the other, or both — and, for something
/// the analysis examined, what the comparison said about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeStatus {
    Added,
    Removed,
    Modified,
    Unchanged,
}

impl NodeStatus {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Modified => "modified",
            Self::Unchanged => "unchanged",
        }
    }

    /// Whether the comparison touched this node at all.
    #[must_use]
    pub fn is_changed(self) -> bool {
        !matches!(self, Self::Unchanged)
    }

    /// The status membership alone decides.
    ///
    /// A node the analysis examined takes the status the comparison reported,
    /// which can say `modified`; a node read from a revision rather than from
    /// the diff cannot be modified by definition, because the diff does not
    /// contain it. A thing in neither revision is `removed`, which is the only
    /// reading left for a node that was reached from the base and is gone.
    #[must_use]
    pub fn from_membership(in_base: bool, in_target: bool) -> Self {
        match (in_base, in_target) {
            (true, true) => Self::Unchanged,
            (false, true) => Self::Added,
            (_, false) => Self::Removed,
        }
    }
}

/// Whether a relationship exists in one revision, the other, or both.
///
/// There is deliberately no `modified`: a relationship whose target changed is
/// one removed edge and one added edge, which is what a reader needs to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeStatus {
    Added,
    Removed,
    Unchanged,
}

impl EdgeStatus {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Unchanged => "unchanged",
        }
    }

    /// The status of a relationship present in the revisions named.
    #[must_use]
    pub fn from_membership(in_base: bool, in_target: bool) -> Option<Self> {
        match (in_base, in_target) {
            (true, true) => Some(Self::Unchanged),
            (false, true) => Some(Self::Added),
            (true, false) => Some(Self::Removed),
            (false, false) => None,
        }
    }
}

/// What kind of relationship an edge reports.
///
/// There is no `called_by`: it is a `calls` edge read backwards, and storing
/// the reverse as its own relation makes two facts out of one. Direction of
/// travel belongs to the walk, not to the edge. There is no generic
/// `depends_on` either, because a caller cannot tell what evidence produced one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Relation {
    Calls,
    Imports,
    ReExports,
    ReferencesType,
    Extends,
    Implements,
    TestedBy,
    Contains,
    PossibleCall,
}

impl Relation {
    /// The relations this version resolves, in the order they are reported.
    ///
    /// The vocabulary is larger than the set; a relation outside this list is
    /// rejected by name rather than answered with silence. A newly resolved
    /// relation is appended, so a default request keeps the prefix it already
    /// reported in the order it reported it.
    pub const SUPPORTED: &'static [Self] = &[
        Self::Imports,
        Self::TestedBy,
        Self::Calls,
        Self::Contains,
        Self::ReExports,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::Imports => "imports",
            Self::ReExports => "re_exports",
            Self::ReferencesType => "references_type",
            Self::Extends => "extends",
            Self::Implements => "implements",
            Self::TestedBy => "tested_by",
            Self::Contains => "contains",
            Self::PossibleCall => "possible_call",
        }
    }

    /// The supported relation this name refers to.
    ///
    /// A name in the vocabulary but outside [`Relation::SUPPORTED`] is not
    /// accepted: answering with an empty edge set would report "no such
    /// relationship" for a relationship this version never looks for.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::SUPPORTED
            .iter()
            .copied()
            .find(|relation| relation.name() == value)
    }

    /// The supported relations, named for an error message.
    #[must_use]
    pub fn accepted() -> String {
        Self::SUPPORTED
            .iter()
            .map(|relation| relation.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// How an edge was resolved, and therefore how much it can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Resolution {
    /// An import statement whose specifier resolved to a path in the revision.
    ResolvedSpecifier,
    /// A call whose callee is a plain identifier declared in the caller's own
    /// file, resolved against the names that revision declares.
    DirectLocalSymbol,
    /// A containment the analysis itself declares: a module declares a
    /// function.
    Declaration,
    /// A test file imports the module directly.
    TestImportsModule,
    /// A test file's name matches the module's, after extensions and suffixes.
    TestNameMatchesModule,
    /// A call whose callee is a name the caller imports, resolving to a
    /// function the imported module exports itself.
    ImportedSymbol,
    /// An export clause that forwards names from another module.
    ExportClause,
    /// A call whose callee resolves through one or more re-export hops.
    ReExportedSymbol,
}

impl Resolution {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::ResolvedSpecifier => "resolved_specifier",
            Self::DirectLocalSymbol => "direct_local_symbol",
            Self::Declaration => "declaration",
            Self::TestImportsModule => "test_imports_module",
            Self::TestNameMatchesModule => "test_name_matches_module",
            Self::ImportedSymbol => "imported_symbol",
            Self::ExportClause => "export_clause",
            Self::ReExportedSymbol => "re_exported_symbol",
        }
    }

    /// Confidence that the relationship this edge reports is real.
    ///
    /// Fixed per resolution rather than computed, so two runs cannot disagree
    /// and a reader can look up what a number meant. A local call and a
    /// declared containment are facts of the one parsed file, so both are
    /// exact; an imported symbol is one resolution of a specifier the import
    /// index already resolved, against a name the exporting module publishes,
    /// so it is exact too. The two test values are the ones
    /// [`crate::query::impact::TestLink`] already publishes, so a test reported
    /// at `0.9` by a detail answer cannot appear here at some other number.
    ///
    /// A re-exported symbol is the one call resolution below 1.0: each hop is
    /// a separate specifier resolution that could be wrong, and a barrel
    /// module's `export *` forwards names from a file the scan has not read,
    /// so a chain is believed slightly less than the name it ends at.
    #[must_use]
    pub fn confidence(self) -> f64 {
        match self {
            Self::ResolvedSpecifier
            | Self::DirectLocalSymbol
            | Self::Declaration
            | Self::ImportedSymbol
            | Self::ExportClause => 1.0,
            Self::TestImportsModule | Self::ReExportedSymbol => 0.9,
            Self::TestNameMatchesModule => 0.8,
        }
    }
}

/// Which way a walk follows edges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Direction {
    /// What reaches the root.
    Upstream,
    /// What the root reaches.
    Downstream,
    #[default]
    Both,
}

impl Direction {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Downstream => "downstream",
            Self::Both => "both",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "upstream" => Some(Self::Upstream),
            "downstream" => Some(Self::Downstream),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    /// Whether a walk in this direction follows edges backwards.
    #[must_use]
    pub fn follows_upstream(self) -> bool {
        matches!(self, Self::Upstream | Self::Both)
    }

    /// Whether a walk in this direction follows edges forwards.
    #[must_use]
    pub fn follows_downstream(self) -> bool {
        matches!(self, Self::Downstream | Self::Both)
    }
}

/// Which revision's relationships are shown.
///
/// A view narrows what is shown; it never rewrites what is true, so an edge
/// shown under `target` still reads `added`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Delta,
    Base,
    Target,
}

impl View {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Delta => "delta",
            Self::Base => "base",
            Self::Target => "target",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "delta" => Some(Self::Delta),
            "base" => Some(Self::Base),
            "target" => Some(Self::Target),
            _ => None,
        }
    }

    /// Whether an edge of this status belongs in this view.
    #[must_use]
    pub fn includes(self, status: EdgeStatus) -> bool {
        match self {
            Self::Delta => true,
            Self::Base => matches!(status, EdgeStatus::Removed | EdgeStatus::Unchanged),
            Self::Target => matches!(status, EdgeStatus::Added | EdgeStatus::Unchanged),
        }
    }
}

/// Where a relationship is written, when a single site produced it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Evidence {
    pub file: String,
    pub line: u32,
}

/// Something a relationship can point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Stable identity within one analysis: `module:<path>`, or
    /// `function:<function_id>`.
    pub id: String,
    /// Short render key, `n0`…`nN`, assigned in the delivered graph's order.
    pub key: String,
    /// A file basename, or a qualified function name.
    pub label: String,
    pub kind: NodeKind,
    /// Repository-relative path: the target side when there is one.
    pub path: String,
    /// Where the node starts in its file, as `(start_line, start_column)`, or
    /// `(0, 0)` for a module, which has no position of its own.
    ///
    /// Two functions of one file share a kind, a path, and sometimes a name;
    /// their positions are what separate them, and ordering by position is the
    /// order a reader meets them in the file.
    pub range_start: (u32, u32),
    pub status: NodeStatus,
    /// Fewest hops by which the walk reached this node. Zero for a root.
    pub depth: u32,
    /// What this node collapses, when it is a `group`. Absent for a module and
    /// for a function, which stand for themselves.
    pub group: Option<Group>,
}

impl Node {
    /// The identity of the module at `path`.
    #[must_use]
    pub fn module_id(path: &str) -> String {
        format!("module:{path}")
    }

    /// The identity of the function the analysis calls `function_id`.
    #[must_use]
    pub fn function_id(function_id: &str) -> String {
        format!("function:{function_id}")
    }

    /// The identity of the group that collapses `role`.
    ///
    /// Derived from what the group collapses rather than from its members'
    /// number or order, so one graph holds one group per role and the same
    /// collapse produces the same identity on every run.
    #[must_use]
    pub fn group_id(role: GroupRole) -> String {
        format!("group:{}", role.name())
    }

    /// The last path segment, which is what a diagram shows.
    #[must_use]
    pub fn basename(path: &str) -> String {
        path.rsplit('/').next().unwrap_or(path).to_owned()
    }

    /// Total order over nodes: kind, then path, then source position, then
    /// label.
    fn order(&self) -> (NodeKind, &str, (u32, u32), &str) {
        (
            self.kind,
            self.path.as_str(),
            self.range_start,
            self.label.as_str(),
        )
    }
}

/// One resolved relationship, carrying the evidence for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub relation: Relation,
    pub status: EdgeStatus,
    pub resolution: Resolution,
    pub evidence: Option<Evidence>,
}

impl Edge {
    #[must_use]
    pub fn confidence(&self) -> f64 {
        self.resolution.confidence()
    }

    /// Total order over edges: source, then relation, then target, then
    /// resolution. A removal and the addition that replaced it therefore sit
    /// together.
    fn order(&self) -> (&str, Relation, &str, Resolution) {
        (
            self.from.as_str(),
            self.relation,
            self.to.as_str(),
            self.resolution,
        )
    }
}

/// Which budget a graph ran into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationReason {
    MaxNodes,
    MaxEdges,
}

impl TruncationReason {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::MaxNodes => "max_nodes",
            Self::MaxEdges => "max_edges",
        }
    }
}

/// How much of a graph an answer will carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_nodes: usize,
    pub max_edges: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nodes: DEFAULT_MAX_NODES,
            max_edges: DEFAULT_MAX_EDGES,
        }
    }
}

impl Limits {
    /// The budgets a request is applied with, after defaults and bounds.
    ///
    /// Canonical, the way [`crate::query::canonical_limit`] is canonical: a
    /// caller is told exactly what was applied, and two requests differing only
    /// in an out-of-range budget mean the same graph.
    #[must_use]
    pub fn canonical(max_nodes: Option<usize>, max_edges: Option<usize>) -> Self {
        Self {
            max_nodes: max_nodes
                .unwrap_or(DEFAULT_MAX_NODES)
                .clamp(MIN_MAX_NODES, MAX_MAX_NODES),
            max_edges: max_edges
                .unwrap_or(DEFAULT_MAX_EDGES)
                .clamp(MIN_MAX_EDGES, MAX_MAX_EDGES),
        }
    }
}

/// The depth a request is applied with, after defaults and bounds.
#[must_use]
pub fn canonical_depth(depth: Option<u32>) -> u32 {
    depth.unwrap_or(DEFAULT_DEPTH).clamp(MIN_DEPTH, MAX_DEPTH)
}

/// One delivered graph: ordered, bounded, and keyed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    roots: Vec<String>,
    truncated: bool,
    omitted_nodes: u32,
    omitted_edges: u32,
    reasons: Vec<TruncationReason>,
}

impl Graph {
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// Identities of the nodes the walk started from, in the graph's order.
    #[must_use]
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    #[must_use]
    pub fn omitted_nodes(&self) -> u32 {
        self.omitted_nodes
    }

    #[must_use]
    pub fn omitted_edges(&self) -> u32 {
        self.omitted_edges
    }

    #[must_use]
    pub fn reasons(&self) -> &[TruncationReason] {
        &self.reasons
    }

    /// The node an identity refers to.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// Whether an identity is one the walk started from.
    #[must_use]
    pub fn is_root(&self, id: &str) -> bool {
        self.roots.iter().any(|root| root == id)
    }
}

/// A graph under construction: nodes and edges as they are discovered, before
/// ordering, truncation, and keying make them an answer.
#[derive(Debug, Default)]
pub struct GraphBuilder {
    nodes: BTreeMap<String, Node>,
    edges: BTreeMap<(String, Relation, String, Resolution), Edge>,
    roots: BTreeSet<String>,
    /// Nodes a budget, rather than a grouping rule, is why a caller cannot see
    /// individually. Reported as omitted, because raising the budget brings
    /// them back.
    omitted_by_budget: u32,
}

impl GraphBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node, or keep the one already present.
    ///
    /// A node reached twice keeps the fewest hops it was reached by, so depth
    /// is a property of the graph rather than of the order the walk happened to
    /// visit in.
    pub fn add_node(&mut self, node: Node) {
        match self.nodes.get_mut(&node.id) {
            Some(existing) => existing.depth = existing.depth.min(node.depth),
            None => {
                self.nodes.insert(node.id.clone(), node);
            }
        }
    }

    /// Mark an identity already added as a node the walk started from.
    pub fn add_root(&mut self, id: String) {
        self.roots.insert(id);
    }

    /// Add an edge, or keep the one already present.
    ///
    /// Two sites producing the same relationship under the same resolution are
    /// one edge; the first one's evidence is kept, and because edges arrive in
    /// the deterministic order of the indexes they were read from, "first" is
    /// not an accident of iteration.
    pub fn add_edge(&mut self, edge: Edge) {
        self.edges
            .entry((
                edge.from.clone(),
                edge.relation,
                edge.to.clone(),
                edge.resolution,
            ))
            .or_insert(edge);
    }

    #[must_use]
    pub fn contains_node(&self, id: &str) -> bool {
        self.nodes.contains_key(id)
    }

    /// The node an identity refers to, while the graph is being built.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Whether an identity is one the walk started from.
    #[must_use]
    pub fn is_root(&self, id: &str) -> bool {
        self.roots.contains(id)
    }

    /// Every edge added so far, in the graph's edge order.
    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.values()
    }

    /// Every node added so far, in the graph's order.
    #[must_use]
    pub fn nodes(&self) -> Vec<&Node> {
        let mut nodes = self.nodes.values().collect::<Vec<_>>();
        nodes.sort_by(|left, right| left.order().cmp(&right.order()));
        nodes
    }

    /// The nodes a budget of `max_nodes` would not admit.
    ///
    /// Neither a root nor a group is ever among them: a root is what the graph
    /// was asked about, and a group already stands for nodes, so collapsing
    /// either would answer a different question than the one that was asked.
    #[must_use]
    pub fn overflow(&self, max_nodes: usize) -> BTreeSet<String> {
        let ordered = self.nodes();
        let kept = keep_nodes(&ordered, &self.roots, max_nodes);
        ordered
            .into_iter()
            .filter(|node| {
                !kept.contains(&node.id)
                    && node.kind != NodeKind::Group
                    && !self.roots.contains(&node.id)
            })
            .map(|node| node.id.clone())
            .collect()
    }

    /// Replace `members` with one group node standing for them.
    ///
    /// The group takes the fewest hops any member was reached by and the status
    /// every member shares, or `unchanged` when they do not share one: a mixed
    /// set has no single status, and a group is context rather than a claim
    /// about one file. Its label is built from what it collapses and the change
    /// area `area` names, so nothing in a rendering is written prose.
    ///
    /// Edges are rewired onto the group and merged by relation, status, and
    /// resolution, so eight tests that import a module contribute one edge
    /// while a test that merely matches the module's name keeps its own,
    /// different claim. A relationship between two members is internal to the
    /// group and is dropped, having no endpoints left to draw, and evidence
    /// goes with the member it pointed at, because a group has no single site.
    ///
    /// A root is never collapsed. Returns how many nodes the group replaced.
    pub fn collapse(
        &mut self,
        role: GroupRole,
        members: &BTreeSet<String>,
        area: Option<String>,
    ) -> u32 {
        let mut collapsed = BTreeSet::new();
        let mut statuses = BTreeSet::new();
        let mut depth = u32::MAX;
        for id in members {
            if self.roots.contains(id) {
                continue;
            }
            let Some(node) = self.nodes.remove(id) else {
                continue;
            };
            depth = depth.min(node.depth);
            statuses.insert(node.status);
            collapsed.insert(node.id);
        }
        let size = count(collapsed.len());
        if size == 0 {
            return 0;
        }
        let mut shared = statuses.into_iter();
        let status = match (shared.next(), shared.next()) {
            (Some(status), None) => status,
            _ => NodeStatus::Unchanged,
        };

        let group = Group { size, role };
        let id = Node::group_id(role);
        let path = area.unwrap_or_default();
        let label = group.label((!path.is_empty()).then_some(path.as_str()));
        self.nodes.insert(
            id.clone(),
            Node {
                id: id.clone(),
                key: String::new(),
                label,
                kind: NodeKind::Group,
                path,
                range_start: (0, 0),
                status,
                depth,
                group: Some(group),
            },
        );

        for (_, mut edge) in std::mem::take(&mut self.edges) {
            let from = collapsed.contains(&edge.from);
            let to = collapsed.contains(&edge.to);
            if from && to {
                continue;
            }
            if from {
                edge.from.clone_from(&id);
            }
            if to {
                edge.to.clone_from(&id);
            }
            if from || to {
                edge.evidence = None;
            }
            self.add_edge(edge);
        }
        size
    }

    /// Record that a budget is why `nodes` nodes are not individually present.
    pub fn omit(&mut self, nodes: u32) {
        self.omitted_by_budget = self.omitted_by_budget.saturating_add(nodes);
    }

    /// Drop the edges a view does not show, and the nodes that leaves isolated.
    ///
    /// Statuses are not rewritten: the view decides what is shown, not what is
    /// true.
    pub fn apply_view(&mut self, view: View) {
        if view == View::Delta {
            return;
        }
        self.edges.retain(|_, edge| view.includes(edge.status));
        let connected = self
            .edges
            .values()
            .flat_map(|edge| [edge.from.clone(), edge.to.clone()])
            .collect::<BTreeSet<_>>();
        self.nodes
            .retain(|id, _| connected.contains(id) || self.roots.contains(id));
    }

    /// Order, truncate, and key the graph.
    ///
    /// The three happen in that sequence for one reason: keys are assigned last
    /// so that `n0`…`nN` are dense and are a function of the graph a caller
    /// actually receives, not of the walk that produced it.
    #[must_use]
    pub fn finish(self, limits: Limits) -> Graph {
        let Self {
            nodes,
            edges,
            roots,
            omitted_by_budget,
        } = self;

        let mut ordered = nodes.into_values().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.order().cmp(&right.order()));

        let total_nodes = ordered.len();
        let kept_ids = keep_nodes(
            &ordered.iter().collect::<Vec<_>>(),
            &roots,
            limits.max_nodes,
        );
        ordered.retain(|node| kept_ids.contains(&node.id));

        let mut ordered_edges = edges
            .into_values()
            .filter(|edge| kept_ids.contains(&edge.from) && kept_ids.contains(&edge.to))
            .collect::<Vec<_>>();
        ordered_edges.sort_by(|left, right| left.order().cmp(&right.order()));

        let total_edges = ordered_edges.len();
        let mut reasons = Vec::new();
        if total_nodes > ordered.len() || omitted_by_budget > 0 {
            reasons.push(TruncationReason::MaxNodes);
        }
        if total_edges > limits.max_edges {
            reasons.push(TruncationReason::MaxEdges);
            ordered_edges.truncate(limits.max_edges);
        }

        for (position, node) in ordered.iter_mut().enumerate() {
            node.key = format!("n{position}");
        }

        let mut roots = roots
            .into_iter()
            .filter(|root| kept_ids.contains(root))
            .collect::<Vec<_>>();
        roots.sort();

        Graph {
            omitted_nodes: count(total_nodes - ordered.len()).saturating_add(omitted_by_budget),
            omitted_edges: count(total_edges - ordered_edges.len()),
            truncated: !reasons.is_empty(),
            reasons,
            nodes: ordered,
            edges: ordered_edges,
            roots,
        }
    }
}

/// Which nodes survive a budget.
///
/// The preference order is the documented one: roots first, then anything the
/// comparison changed, then the nearest, then the graph's own order as a total
/// tiebreak. Dropping the far unchanged nodes first keeps a truncated graph
/// centered on what was asked about.
///
/// Groups rank directly behind the roots: one stands for several nodes at
/// once, so dropping it costs a reader more than dropping any single node it
/// replaced — which is also why a group can never be dropped in favour of a
/// node it collapsed.
fn keep_nodes(ordered: &[&Node], roots: &BTreeSet<String>, max_nodes: usize) -> BTreeSet<String> {
    if ordered.len() <= max_nodes {
        return ordered.iter().map(|node| node.id.clone()).collect();
    }
    let mut ranked = ordered.iter().enumerate().collect::<Vec<_>>();
    ranked.sort_by_key(|(position, node)| {
        (
            u8::from(!roots.contains(&node.id)),
            u8::from(node.kind != NodeKind::Group),
            u8::from(!node.status.is_changed()),
            node.depth,
            *position,
        )
    });
    ranked
        .into_iter()
        .take(max_nodes)
        .map(|(_, node)| node.id.clone())
        .collect()
}

/// One node a walk reached, and the fewest hops it took to reach it.
pub type Reached = BTreeMap<String, u32>;

/// Walk outward from every root, bounded by depth.
///
/// Breadth-first, so a node records the fewest hops by which it was reached,
/// matching [`crate::imports::ImportIndex::reachable_importers`]. A node
/// already seen is not re-queued; the edges that reached it are collected by
/// the caller from the reached set, so a cycle's closing edge survives while
/// the walk still terminates.
///
/// `neighbors` answers what lies one hop from an identity in one direction; it
/// is the only thing that knows where relationships come from, which keeps the
/// model free of any particular index.
#[must_use]
pub fn walk<F>(roots: &[String], direction: Direction, depth: u32, neighbors: F) -> Reached
where
    F: Fn(&str, Direction) -> Vec<String>,
{
    let mut reached: Reached = roots.iter().map(|root| (root.clone(), 0)).collect();
    let mut queue = roots
        .iter()
        .map(|root| (root.clone(), 0_u32))
        .collect::<VecDeque<_>>();

    while let Some((current, hops)) = queue.pop_front() {
        if hops >= depth {
            continue;
        }
        let step = |one_way: Direction, reached: &mut Reached, queue: &mut VecDeque<_>| {
            for neighbor in neighbors(&current, one_way) {
                if reached.contains_key(&neighbor) {
                    continue;
                }
                reached.insert(neighbor.clone(), hops + 1);
                queue.push_back((neighbor, hops + 1));
            }
        };
        if direction.follows_upstream() {
            step(Direction::Upstream, &mut reached, &mut queue);
        }
        if direction.follows_downstream() {
            step(Direction::Downstream, &mut reached, &mut queue);
        }
    }
    reached
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(path: &str, status: NodeStatus, depth: u32) -> Node {
        Node {
            id: Node::module_id(path),
            key: String::new(),
            label: Node::basename(path),
            kind: NodeKind::Module,
            path: path.to_owned(),
            range_start: (0, 0),
            status,
            depth,
            group: None,
        }
    }

    fn edge(from: &str, to: &str, status: EdgeStatus) -> Edge {
        Edge {
            from: Node::module_id(from),
            to: Node::module_id(to),
            relation: Relation::Imports,
            status,
            resolution: Resolution::ResolvedSpecifier,
            evidence: None,
        }
    }

    #[test]
    fn edge_status_comes_from_membership_in_the_two_revisions() {
        assert_eq!(
            EdgeStatus::from_membership(false, true),
            Some(EdgeStatus::Added)
        );
        assert_eq!(
            EdgeStatus::from_membership(true, false),
            Some(EdgeStatus::Removed)
        );
        assert_eq!(
            EdgeStatus::from_membership(true, true),
            Some(EdgeStatus::Unchanged)
        );
        assert_eq!(EdgeStatus::from_membership(false, false), None);
    }

    #[test]
    fn a_view_narrows_the_edges_without_rewriting_their_status() {
        assert!(View::Base.includes(EdgeStatus::Removed));
        assert!(!View::Base.includes(EdgeStatus::Added));
        assert!(View::Target.includes(EdgeStatus::Added));
        assert!(!View::Target.includes(EdgeStatus::Removed));
        assert!(View::Delta.includes(EdgeStatus::Removed));
        assert!(View::Delta.includes(EdgeStatus::Added));
    }

    #[test]
    fn only_the_supported_relations_parse() {
        assert_eq!(Relation::parse("imports"), Some(Relation::Imports));
        assert_eq!(Relation::parse("tested_by"), Some(Relation::TestedBy));
        assert_eq!(Relation::parse("calls"), Some(Relation::Calls));
        assert_eq!(Relation::parse("contains"), Some(Relation::Contains));
        assert_eq!(Relation::parse("re_exports"), Some(Relation::ReExports));
        assert_eq!(Relation::parse("possible_call"), None);
        assert_eq!(Relation::parse("depends_on"), None);
        assert_eq!(
            Relation::accepted(),
            "imports, tested_by, calls, contains, re_exports"
        );
    }

    #[test]
    fn the_local_resolutions_are_exact_and_named_as_published() {
        assert_eq!(Resolution::DirectLocalSymbol.name(), "direct_local_symbol");
        assert!((Resolution::DirectLocalSymbol.confidence() - 1.0).abs() < f64::EPSILON);
        assert_eq!(Resolution::Declaration.name(), "declaration");
        assert!((Resolution::Declaration.confidence() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_re_exported_symbol_is_the_one_call_resolution_below_certainty() {
        assert_eq!(Resolution::ImportedSymbol.name(), "imported_symbol");
        assert!((Resolution::ImportedSymbol.confidence() - 1.0).abs() < f64::EPSILON);
        assert_eq!(Resolution::ExportClause.name(), "export_clause");
        assert!((Resolution::ExportClause.confidence() - 1.0).abs() < f64::EPSILON);
        // Each hop is a separate resolution that could be wrong, so a chain is
        // believed exactly as much as a test that imports its module.
        assert_eq!(Resolution::ReExportedSymbol.name(), "re_exported_symbol");
        assert!(
            (Resolution::ReExportedSymbol.confidence()
                - Resolution::TestImportsModule.confidence())
            .abs()
                < f64::EPSILON
        );
        assert!(
            Resolution::ReExportedSymbol.confidence() < Resolution::ImportedSymbol.confidence()
        );
    }

    #[test]
    fn budgets_and_depth_are_clamped_to_what_can_be_delivered() {
        assert_eq!(canonical_depth(None), DEFAULT_DEPTH);
        assert_eq!(canonical_depth(Some(0)), MIN_DEPTH);
        assert_eq!(canonical_depth(Some(9)), MAX_DEPTH);
        let limits = Limits::canonical(Some(1), Some(10_000));
        assert_eq!(limits.max_nodes, MIN_MAX_NODES);
        assert_eq!(limits.max_edges, MAX_MAX_EDGES);
    }

    #[test]
    fn ordering_and_keys_do_not_depend_on_insertion_order() {
        let paths = ["src/b.ts", "src/a.ts", "src/c.ts"];
        let build = |reverse: bool| {
            let mut builder = GraphBuilder::new();
            let mut ordered = paths.to_vec();
            if reverse {
                ordered.reverse();
            }
            for path in ordered {
                builder.add_node(node(path, NodeStatus::Unchanged, 1));
            }
            builder.add_edge(edge("src/a.ts", "src/b.ts", EdgeStatus::Added));
            builder.add_edge(edge("src/c.ts", "src/b.ts", EdgeStatus::Unchanged));
            builder.finish(Limits::default())
        };

        let forward = build(false);
        assert_eq!(forward, build(true));
        assert_eq!(
            forward
                .nodes()
                .iter()
                .map(|node| (node.key.as_str(), node.path.as_str()))
                .collect::<Vec<_>>(),
            vec![("n0", "src/a.ts"), ("n1", "src/b.ts"), ("n2", "src/c.ts")]
        );
    }

    #[test]
    fn two_functions_of_one_file_order_by_source_position() {
        let function = |line: u32, name: &str| Node {
            id: Node::function_id(&format!("src/a.ts#fn:{name}@target:{line}:0")),
            key: String::new(),
            label: name.to_owned(),
            kind: NodeKind::Function,
            path: "src/a.ts".to_owned(),
            range_start: (line, 0),
            status: NodeStatus::Unchanged,
            depth: 1,
            group: None,
        };

        let mut builder = GraphBuilder::new();
        // Inserted latest-first, so the delivered order can only come from the
        // nodes' own positions.
        builder.add_node(function(40, "second"));
        builder.add_node(function(10, "first"));
        builder.add_node(node("src/a.ts", NodeStatus::Unchanged, 0));

        let graph = builder.finish(Limits::default());
        assert_eq!(
            graph
                .nodes()
                .iter()
                .map(|node| node.label.as_str())
                .collect::<Vec<_>>(),
            vec!["a.ts", "first", "second"]
        );
    }

    #[test]
    fn truncation_keeps_the_root_then_changed_then_nearest() {
        let mut builder = GraphBuilder::new();
        builder.add_node(node("src/root.ts", NodeStatus::Modified, 0));
        builder.add_root(Node::module_id("src/root.ts"));
        builder.add_node(node("src/added.ts", NodeStatus::Added, 2));
        builder.add_node(node("src/near.ts", NodeStatus::Unchanged, 1));
        builder.add_node(node("src/far.ts", NodeStatus::Unchanged, 3));
        builder.add_edge(edge("src/far.ts", "src/root.ts", EdgeStatus::Unchanged));

        let graph = builder.finish(Limits {
            max_nodes: 3,
            max_edges: 60,
        });

        assert_eq!(
            graph
                .nodes()
                .iter()
                .map(|node| node.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/added.ts", "src/near.ts", "src/root.ts"]
        );
        assert!(graph.truncated());
        assert_eq!(graph.omitted_nodes(), 1);
        assert_eq!(graph.reasons(), [TruncationReason::MaxNodes]);
        // The dropped node took its edge with it rather than leaving a dangling
        // endpoint.
        assert!(graph.edges().is_empty());
    }

    #[test]
    fn an_edge_budget_is_reported_separately_from_a_node_budget() {
        let mut builder = GraphBuilder::new();
        for path in ["src/a.ts", "src/b.ts", "src/c.ts", "src/d.ts"] {
            builder.add_node(node(path, NodeStatus::Unchanged, 1));
        }
        builder.add_edge(edge("src/a.ts", "src/b.ts", EdgeStatus::Added));
        builder.add_edge(edge("src/b.ts", "src/c.ts", EdgeStatus::Added));
        builder.add_edge(edge("src/c.ts", "src/d.ts", EdgeStatus::Added));
        builder.add_edge(edge("src/d.ts", "src/a.ts", EdgeStatus::Added));

        let graph = builder.finish(Limits {
            max_nodes: 30,
            max_edges: 3,
        });

        assert_eq!(graph.edges().len(), 3);
        assert_eq!(graph.omitted_edges(), 1);
        assert_eq!(graph.reasons(), [TruncationReason::MaxEdges]);
    }

    #[test]
    fn a_view_drops_the_edges_that_revision_does_not_have() {
        let mut builder = GraphBuilder::new();
        builder.add_node(node("src/root.ts", NodeStatus::Modified, 0));
        builder.add_root(Node::module_id("src/root.ts"));
        builder.add_node(node("src/new.ts", NodeStatus::Added, 1));
        builder.add_node(node("src/old.ts", NodeStatus::Removed, 1));
        builder.add_edge(edge("src/root.ts", "src/new.ts", EdgeStatus::Added));
        builder.add_edge(edge("src/root.ts", "src/old.ts", EdgeStatus::Removed));
        builder.apply_view(View::Target);

        let graph = builder.finish(Limits::default());
        assert_eq!(graph.edges().len(), 1);
        assert_eq!(graph.edges()[0].status, EdgeStatus::Added);
        assert_eq!(
            graph
                .nodes()
                .iter()
                .map(|node| node.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/new.ts", "src/root.ts"]
        );
    }

    #[test]
    fn a_walk_is_bounded_by_depth_and_terminates_through_a_cycle() {
        let edges = [("a", "b"), ("b", "c"), ("c", "a")];
        let neighbors = |id: &str, direction: Direction| match direction {
            Direction::Downstream => edges
                .iter()
                .filter(|(from, _)| *from == id)
                .map(|(_, to)| (*to).to_owned())
                .collect(),
            _ => edges
                .iter()
                .filter(|(_, to)| *to == id)
                .map(|(from, _)| (*from).to_owned())
                .collect(),
        };

        let roots = vec!["a".to_owned()];
        let one_hop = walk(&roots, Direction::Downstream, 1, neighbors);
        assert_eq!(one_hop.keys().collect::<Vec<_>>(), vec!["a", "b"]);

        let whole_cycle = walk(&roots, Direction::Downstream, 3, neighbors);
        assert_eq!(whole_cycle.get("a"), Some(&0));
        assert_eq!(whole_cycle.get("c"), Some(&2));

        let upstream = walk(&roots, Direction::Upstream, 1, neighbors);
        assert_eq!(upstream.keys().collect::<Vec<_>>(), vec!["a", "c"]);

        let both = walk(&roots, Direction::Both, 1, neighbors);
        assert_eq!(both.keys().collect::<Vec<_>>(), vec!["a", "b", "c"]);
    }
}
