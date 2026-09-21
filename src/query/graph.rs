//! Validate an impact-graph request, and project one graph into an answer.
//!
//! The model in [`crate::graph`] holds relationships and knows nothing about
//! serialization or about what a caller asked for. This module is the boundary
//! between the two: a request becomes a validated [`GraphRequest`], one
//! comparison's two import graphs become a [`Graph`], and that graph becomes
//! the `data` object of an answer.
//!
//! The split mirrors [`crate::query::impact`], which owns its domain types,
//! and [`crate::query`], which owns their serialized shape — except that here
//! the views live beside the projection, because what a graph request accepts
//! and what a graph answer carries are one contract and belong in one place.

use serde::Serialize;

use super::display_path;
use crate::{
    AnalysisResult,
    graph::{self, Direction, Graph, Limits, Node, NodeKind, Relation, View},
    imports::ImportIndex,
};

/// Which renderings a request asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Renderings {
    /// The dependency diff.
    pub diff: bool,
    /// The Mermaid diagram.
    pub mermaid: bool,
}

impl Renderings {
    /// The asked-for renderings, named in the model's fixed order.
    ///
    /// Canonical, so two requests that asked for the same renderings in
    /// different orders are echoed — and rendered — identically.
    fn asked(self) -> Vec<&'static str> {
        let mut asked = Vec::with_capacity(2);
        if self.diff {
            asked.push("diff");
        }
        if self.mermaid {
            asked.push("mermaid");
        }
        asked
    }
}

/// One validated impact-graph request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRequest {
    /// The changed file the graph is centered on. Absent centers it on the
    /// changed set.
    pub root: Option<String>,
    pub direction: Direction,
    /// The relations the walk may follow, in the supported order.
    pub relations: Vec<Relation>,
    pub depth: u32,
    pub view: View,
    pub limits: Limits,
    pub render: Renderings,
}

/// The parameters of an impact-graph request, as a transport decoded them.
///
/// The scalar enums arrive already parsed, because the harness parses every
/// method's enums through one helper and an unknown value should be rejected
/// with the message shape it uses everywhere. The list-valued parameters
/// arrive as the caller wrote them, because their accepted set belongs here —
/// relations are [`Relation::SUPPORTED`] and renderings are the two this
/// version produces — and the rejection has to name that set.
#[derive(Debug, Clone, Copy)]
pub struct Requested<'a> {
    /// The changed file to center the graph on.
    pub file: Option<&'a str>,
    /// The function to center the graph on. Rejected by this version.
    pub function_id: Option<&'a str>,
    /// Which way the walk follows edges; [`Direction::default`] when absent.
    pub direction: Option<Direction>,
    /// The relations the walk may follow, as named.
    pub relations: &'a [String],
    /// Hops from the root, already bounded by the caller.
    pub depth: u32,
    /// Which revision's relationships to show; [`View::default`] when absent.
    pub view: Option<View>,
    /// Node and edge budgets, already bounded by the caller.
    pub limits: Limits,
    /// The renderings to include, as named.
    pub render: &'a [String],
}

impl GraphRequest {
    /// Validate one request against the analysis it asks about.
    ///
    /// # Errors
    ///
    /// Returns the first thing that cannot be answered as written, with a
    /// message naming the field and what it accepts: a root this version
    /// cannot resolve, a path the comparison did not change, or a relation or
    /// rendering outside the set this version produces.
    pub fn validate(
        result: &AnalysisResult,
        requested: &Requested<'_>,
    ) -> Result<Self, GraphRequestError> {
        Ok(Self {
            root: root(result, requested)?,
            direction: requested.direction.unwrap_or_default(),
            relations: relations(requested.relations)?,
            depth: requested.depth,
            view: requested.view.unwrap_or_default(),
            limits: requested.limits,
            render: renderings(requested.render)?,
        })
    }

    /// The parameters this request was applied with, after defaults.
    ///
    /// This is the `query` echo: every parameter the method understands, with
    /// the bounded budgets that were used rather than the ones written, and
    /// the relations this version resolved rather than the names they were
    /// named by.
    #[must_use]
    pub fn applied(&self) -> AppliedGraphQuery {
        AppliedGraphQuery {
            file: self.root.clone(),
            // Nothing else can root a graph in this version: `validate`
            // rejects the parameter that would.
            function_id: None,
            direction: self.direction.name(),
            relations: self
                .relations
                .iter()
                .map(|relation| relation.name())
                .collect(),
            depth: self.depth,
            view: self.view.name(),
            max_nodes: self.limits.max_nodes,
            max_edges: self.limits.max_edges,
            render: self.render.asked(),
        }
    }
}

/// Answer one validated request from both revisions' import graphs.
///
/// The graph carries only what the request asked for, and a rendering is
/// produced only when it was asked for: a caller that reasons over the graph
/// should not pay to serialize a diagram it will not show.
#[must_use]
pub fn project(
    result: &AnalysisResult,
    base: &ImportIndex,
    target: &ImportIndex,
    request: &GraphRequest,
) -> GraphAnswer {
    let roots = request.root.iter().cloned().collect::<Vec<_>>();
    let graph = graph::build::build(
        result,
        base,
        target,
        &graph::build::Request {
            roots: &roots,
            direction: request.direction,
            relations: &request.relations,
            depth: request.depth,
            view: request.view,
            limits: request.limits,
        },
    );
    answer_of(&graph, request)
}

/// Project one built graph into the shape an answer carries.
fn answer_of(graph: &Graph, request: &GraphRequest) -> GraphAnswer {
    let recommendation = graph::recommend::evaluate(graph);
    GraphAnswer {
        root: request.root.as_deref().map(root_view),
        graph: GraphView {
            nodes: graph.nodes().iter().map(NodeView::of).collect(),
            edges: graph.edges().iter().map(EdgeView::of).collect(),
            truncated: graph.truncated(),
            omitted: OmittedView {
                nodes: graph.omitted_nodes(),
                edges: graph.omitted_edges(),
            },
            reasons: graph.reasons().iter().map(|reason| reason.name()).collect(),
        },
        dependency_diff: request
            .render
            .diff
            .then(|| graph::render::dependency_diff(graph)),
        mermaid: request
            .render
            .mermaid
            .then(|| graph::render::mermaid(graph)),
        visualization: VisualizationView {
            recommended: recommendation.recommended,
            reasons: recommendation
                .reasons
                .iter()
                .map(|signal| SignalView {
                    code: signal.code,
                    message: signal.message.clone(),
                    value: signal.value,
                })
                .collect(),
        },
    }
}

/// The root a graph is centered on, when it is centered on one file.
#[derive(Debug, Clone, Serialize)]
pub struct RootView {
    pub kind: &'static str,
    pub id: String,
    pub path: String,
}

fn root_view(path: &str) -> RootView {
    RootView {
        kind: NodeKind::Module.name(),
        id: Node::module_id(path),
        path: path.to_owned(),
    }
}

/// One node of the delivered graph.
#[derive(Debug, Clone, Serialize)]
pub struct NodeView {
    pub id: String,
    pub key: String,
    pub label: String,
    pub kind: &'static str,
    pub path: String,
    pub status: &'static str,
}

impl NodeView {
    fn of(node: &Node) -> Self {
        Self {
            id: node.id.clone(),
            key: node.key.clone(),
            label: node.label.clone(),
            kind: node.kind.name(),
            path: node.path.clone(),
            status: node.status.name(),
        }
    }
}

/// One relationship of the delivered graph.
#[derive(Debug, Clone, Serialize)]
pub struct EdgeView {
    pub from: String,
    pub to: String,
    pub relation: &'static str,
    pub status: &'static str,
    pub resolution: &'static str,
    /// A quantity rather than a string, so a caller can compare it. It is
    /// written as the shortest decimal that reads back as the same float, so
    /// `1.0`, `0.9`, and `0.8` round-trip byte identically.
    pub confidence: f64,
    /// Absent when no single site produced the relationship.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<EvidenceView>,
}

impl EdgeView {
    fn of(edge: &graph::Edge) -> Self {
        Self {
            from: edge.from.clone(),
            to: edge.to.clone(),
            relation: edge.relation.name(),
            status: edge.status.name(),
            resolution: edge.resolution.name(),
            confidence: edge.confidence(),
            evidence: edge.evidence.as_ref().map(EvidenceView::of),
        }
    }
}

/// Where a relationship is written, when a single site produced it.
#[derive(Debug, Clone, Serialize)]
pub struct EvidenceView {
    pub file: String,
    pub line: u32,
}

impl EvidenceView {
    fn of(evidence: &graph::Evidence) -> Self {
        Self {
            file: evidence.file.clone(),
            line: evidence.line,
        }
    }
}

/// What a budget dropped, so a small graph is not read as a complete one.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OmittedView {
    pub nodes: u32,
    pub edges: u32,
}

/// The structured graph: every node and relationship the answer carries, and
/// every one it does not.
#[derive(Debug, Clone, Serialize)]
pub struct GraphView {
    pub nodes: Vec<NodeView>,
    pub edges: Vec<EdgeView>,
    pub truncated: bool,
    pub omitted: OmittedView,
    /// Which budget was reached, when one was.
    pub reasons: Vec<&'static str>,
}

/// Whether a diagram is worth drawing, and the signals that decided it.
#[derive(Debug, Clone, Serialize)]
pub struct VisualizationView {
    pub recommended: bool,
    pub reasons: Vec<SignalView>,
}

/// One measured signal behind the recommendation.
///
/// The shape matches the risk models' reasons — a stable code, prose carrying
/// the measurement, and the measured quantity — so a caller reads a diagram's
/// explanation the way it already reads a risk explanation.
#[derive(Debug, Clone, Serialize)]
pub struct SignalView {
    pub code: &'static str,
    pub message: String,
    pub value: u32,
}

/// The answer to one impact-graph query.
#[derive(Debug, Clone, Serialize)]
pub struct GraphAnswer {
    /// The root the walk started from, or `null` when the graph is centered on
    /// the changed set: always present, so a caller never has to tell an absent
    /// root from an absent field.
    pub root: Option<RootView>,
    pub graph: GraphView,
    /// The dependency diff, exactly when it was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_diff: Option<String>,
    /// The Mermaid document, exactly when it was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mermaid: Option<String>,
    pub visualization: VisualizationView,
}

/// The parameters an impact-graph answer was produced with.
///
/// This is the `query` echo: every parameter the method understands, with the
/// defaults that were applied and the bounded values that were used.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedGraphQuery {
    pub file: Option<String>,
    pub function_id: Option<String>,
    pub direction: &'static str,
    pub relations: Vec<&'static str>,
    pub depth: u32,
    pub view: &'static str,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub render: Vec<&'static str>,
}

/// A graph request that cannot be answered as asked.
///
/// Every rejection here is `invalid_params` at the transport: the request names
/// something this version cannot answer with, and the message says which field
/// it is and what that field accepts, so a caller can correct itself in one
/// step instead of guessing again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRequestError {
    pub message: String,
}

impl GraphRequestError {
    fn new(message: String) -> Self {
        Self { message }
    }
}

/// The validated root a request names, if it names one.
///
/// `file` and `function_id` are mutually exclusive because a graph has one
/// center; naming neither centers it on every changed file, which is the
/// default a reviewer most often wants. A function root is rejected rather
/// than approximated: placing it needs the call resolution a later milestone
/// adds, and answering a different question than the one asked is worse than
/// saying so.
fn root(
    result: &AnalysisResult,
    requested: &Requested<'_>,
) -> Result<Option<String>, GraphRequestError> {
    match (requested.file, requested.function_id) {
        (Some(_), Some(_)) => Err(GraphRequestError::new(
            "`file` and `function_id` are mutually exclusive, and this version roots a graph at \
             `file` alone; name one changed `file`, or neither to center the graph on every \
             changed file"
                .to_owned(),
        )),
        (None, Some(_)) => Err(GraphRequestError::new(
            "`function_id` cannot root a graph in this version: function roots need call \
             resolution, and this version roots a graph at a changed `file`; name a changed \
             `file`, or neither to center the graph on every changed file"
                .to_owned(),
        )),
        (None, None) => Ok(None),
        (Some(file), None) => {
            if changed_paths(result).contains(&file) {
                Ok(Some(file.to_owned()))
            } else {
                Err(GraphRequestError::new(describe_unknown_file(file, result)))
            }
        }
    }
}

/// The changed paths of this analysis, in the analysis's own order.
fn changed_paths(result: &AnalysisResult) -> Vec<&str> {
    result.files.iter().map(display_path).collect()
}

/// The message for a `file` the comparison did not change.
///
/// Mirrors `describe_unknown` in the harness: the paths to offer are the
/// changed ones, ordered so the likeliest correction comes first, and the
/// list is summarized rather than truncated silently. A caller that guessed
/// blindly still learns the shape of a real path.
fn describe_unknown_file(requested: &str, result: &AnalysisResult) -> String {
    /// Paths named in the error before it is summarized.
    const SUGGESTIONS: usize = 10;

    if result.files.is_empty() {
        return format!(
            "`file` must name a changed file: no `{requested}` in this analysis, which changed \
             no files"
        );
    }
    let mut known = changed_paths(result);
    // Stable, so paths the ranking cannot separate keep the analysis's order.
    known.sort_by_key(|path| std::cmp::Reverse(affinity(requested, path)));
    let shown = known
        .iter()
        .take(SUGGESTIONS)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let remainder = known.len().saturating_sub(SUGGESTIONS);
    let more = if remainder == 0 {
        String::new()
    } else {
        format!(" and {remainder} more")
    };
    format!(
        "`file` must name a changed file: no `{requested}` in this analysis, which changed \
         {shown}{more}"
    )
}

/// How likely a changed path is the one a caller meant.
///
/// A caller that names a path the change does not contain usually named the
/// right file under a stale directory or extension, so a matching file name —
/// extension aside — is the strongest evidence; after that, the longest shared
/// suffix of the two paths. Ties keep the analysis's own order, which is
/// deterministic.
fn affinity(requested: &str, candidate: &str) -> (bool, usize) {
    let shared_suffix = candidate
        .bytes()
        .rev()
        .zip(requested.bytes().rev())
        .take_while(|(left, right)| left == right)
        .count();
    (file_stem(candidate) == file_stem(requested), shared_suffix)
}

/// A path's file name with its extension removed.
///
/// The first dot ends the extension, which is how `src/query/impact.rs` reads
/// the same name, so `cssVars.spec.ts` and `cssVars.ts` share the stem
/// `cssVars`.
fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.split_once('.').map_or(name, |(stem, _)| stem)
}

/// The relations a walk may follow, in the supported order.
///
/// An empty list means every supported relation, which is the documented
/// default. A name this version does not resolve is rejected rather than
/// dropped: an edge set short by one relation would report "no such
/// relationship" for a relationship this version never looks for.
fn relations(requested: &[String]) -> Result<Vec<Relation>, GraphRequestError> {
    if requested.is_empty() {
        return Ok(Relation::SUPPORTED.to_vec());
    }
    for name in requested {
        if Relation::parse(name).is_none() {
            return Err(GraphRequestError::new(format!(
                "`relations` must be one of {}; received `{name}`",
                Relation::accepted()
            )));
        }
    }
    Ok(Relation::SUPPORTED
        .iter()
        .copied()
        .filter(|relation| requested.iter().any(|name| name == relation.name()))
        .collect())
}

/// The renderings a request asks for.
///
/// A name this version cannot render is rejected, for the same reason an
/// unsupported relation is: a caller that awaited `svg` and got `null` would
/// have to guess whether the answer or its request was at fault.
fn renderings(requested: &[String]) -> Result<Renderings, GraphRequestError> {
    let mut renderings = Renderings::default();
    for name in requested {
        match name.as_str() {
            "diff" => renderings.diff = true,
            "mermaid" => renderings.mermaid = true,
            other => {
                return Err(GraphRequestError::new(format!(
                    "`render` must be one of diff, mermaid; received `{other}`"
                )));
            }
        }
    }
    Ok(renderings)
}

#[cfg(test)]
mod tests {
    use super::{Renderings, affinity, relations, renderings};
    use crate::graph::Relation;

    #[test]
    fn a_relation_list_is_normalized_to_the_supported_order() {
        let named = [
            "tested_by".to_owned(),
            "imports".to_owned(),
            "imports".to_owned(),
        ];
        assert_eq!(
            relations(&named).expect("both relations are supported"),
            vec![Relation::Imports, Relation::TestedBy]
        );
        // An empty list is the documented default, not an empty graph.
        assert_eq!(
            relations(&[]).expect("the default is every supported relation"),
            Relation::SUPPORTED.to_vec()
        );
    }

    #[test]
    fn an_unsupported_relation_is_rejected_by_name() {
        let error = relations(&["calls".to_owned()])
            .expect_err("this version resolves no call relationship");
        assert!(error.message.contains("`relations`"), "{}", error.message);
        assert!(
            error.message.contains("imports, tested_by"),
            "{}",
            error.message
        );
        assert!(error.message.contains("calls"), "{}", error.message);
    }

    #[test]
    fn a_render_list_is_normalized_named_and_bounded() {
        let asked = renderings(&["mermaid".to_owned(), "diff".to_owned(), "diff".to_owned()])
            .expect("both renderings exist");
        assert_eq!(
            asked,
            Renderings {
                diff: true,
                mermaid: true
            }
        );
        // Canonical order, so asking in the other order echoes the same list.
        assert_eq!(asked.asked(), ["diff", "mermaid"]);
        assert_eq!(
            renderings(&[]).expect("nothing asked for"),
            Renderings::default()
        );
        let error =
            renderings(&["svg".to_owned()]).expect_err("this version renders no such document");
        assert!(
            error
                .message
                .contains("`render` must be one of diff, mermaid"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_near_miss_path_ranks_the_file_it_meant_first() {
        let requested = "src/style/cssVars.js";
        let meant = "src/style/cssVars.ts";
        let elsewhere = "packages/x/src/style/cssVars.ts";
        let other = "src/app.ts";
        // A stale extension is a near miss: the file it names comes first.
        assert!(affinity(requested, meant) > affinity(requested, other));
        // So is the right name under another directory.
        assert!(affinity(requested, elsewhere) > affinity(requested, other));
        // A path equal to the requested one ranks above every near miss.
        assert!(affinity(requested, requested) > affinity(requested, meant));
    }
}
