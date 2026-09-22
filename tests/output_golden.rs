use diffscope::{
    AnalysisResult, AnalysisSummary, Diagnostic, DiagnosticCode, DiagnosticCounts, FileResult,
    RevisionResult,
    graph::{
        Edge, EdgeStatus, Graph, GraphBuilder, Limits, Node, NodeKind, NodeStatus, Relation,
        Resolution,
        render::{dependency_diff, mermaid},
    },
    languages::DiagnosticSeverity,
    output::{render_human, render_json},
    result::SCHEMA_VERSION,
};

/// Version fields are rendered from the analysis itself, so the fixture uses
/// the identifiers the crate reports instead of copies that drift from them.
fn result() -> AnalysisResult {
    AnalysisResult {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        repository: "/repo".to_owned(),
        base: RevisionResult {
            id: "1111111".to_owned(),
            display_name: "main".to_owned(),
        },
        target: RevisionResult {
            id: "2222222".to_owned(),
            display_name: "feature".to_owned(),
        },
        summary: AnalysisSummary {
            changed_files: 1,
            added_lines: 1,
            removed_lines: 0,
            supported_files: 0,
            unsupported_files: 1,
            diagnostics: DiagnosticCounts {
                info: 1,
                warning: 0,
                error: 0,
            },
        },
        files: vec![FileResult {
            base_path: None,
            target_path: Some("README.md".to_owned()),
            status: diffscope::FileStatus::Added,
            language: None,
            is_binary: false,
            added_lines: 1,
            removed_lines: 0,
            hunks: vec![diffscope::DiffHunk {
                base_start: 0,
                base_count: 0,
                target_start: 1,
                target_count: 1,
                added_lines: 1,
                removed_lines: 0,
            }],
            functions: Vec::new(),
            exports_added: Vec::new(),
            exports_removed: Vec::new(),
            diagnostics: vec![Diagnostic {
                code: DiagnosticCode::UnsupportedLanguage,
                severity: DiagnosticSeverity::Info,
                message: "unsupported language".to_owned(),
                path: Some("README.md".to_owned()),
                range: None,
                related_entity_ids: Vec::new(),
            }],
        }],
        diagnostics: Vec::new(),
    }
}

#[test]
fn human_output_matches_golden() {
    assert_eq!(
        render_human(&result()),
        concat!(
            "DiffScope main..feature\n",
            "Repository: /repo\n",
            "1 changed files, +1 -0 (0 supported, 1 unsupported)\n",
            "added README.md (+1 -0)\n",
            "  info unsupported_language: unsupported language\n",
        )
    );
}

#[test]
fn json_output_matches_golden() {
    let expected = GOLDEN
        .replace("__SCHEMA_VERSION__", &SCHEMA_VERSION.to_string())
        .replace("__TOOL_VERSION__", env!("CARGO_PKG_VERSION"));
    assert_eq!(render_json(&result()).expect("JSON renders"), expected);
}

/// The complete schema-versioned document the CLI and the adapter's `analyze`
/// answer both emit, with the two version fields left to the crate.
const GOLDEN: &str = r#"{
  "schema_version": __SCHEMA_VERSION__,
  "tool_version": "__TOOL_VERSION__",
  "repository": "/repo",
  "base": {
    "id": "1111111",
    "display_name": "main"
  },
  "target": {
    "id": "2222222",
    "display_name": "feature"
  },
  "summary": {
    "changed_files": 1,
    "added_lines": 1,
    "removed_lines": 0,
    "supported_files": 0,
    "unsupported_files": 1,
    "diagnostics": {
      "info": 1,
      "warning": 0,
      "error": 0
    }
  },
  "files": [
    {
      "target_path": "README.md",
      "status": "added",
      "is_binary": false,
      "added_lines": 1,
      "removed_lines": 0,
      "hunks": [
        {
          "base_start": 0,
          "base_count": 0,
          "target_start": 1,
          "target_count": 1,
          "added_lines": 1,
          "removed_lines": 0
        }
      ],
      "functions": [],
      "diagnostics": [
        {
          "code": "unsupported_language",
          "severity": "info",
          "message": "unsupported language",
          "path": "README.md"
        }
      ]
    }
  ],
  "diagnostics": []
}
"#;

/// A module graph in the shape M1 builds for one comparison: a modified module
/// whose dependency was replaced, the unchanged importer that reaches it, and
/// the test that covers it. It goes through `GraphBuilder` rather than being
/// assembled by hand, so the goldens pin what a caller receives, render keys
/// included.
fn graph() -> Graph {
    let module = |path: &str, status: NodeStatus, depth: u32| Node {
        id: Node::module_id(path),
        key: String::new(),
        label: Node::basename(path),
        kind: NodeKind::Module,
        path: path.to_owned(),
        range_start: (0, 0),
        status,
        depth,
        group: None,
    };
    let edge =
        |from: &str, to: &str, relation: Relation, status: EdgeStatus, resolution: Resolution| {
            Edge {
                from: Node::module_id(from),
                to: Node::module_id(to),
                relation,
                status,
                resolution,
                evidence: None,
            }
        };

    let root = "packages/compiler-sfc/src/style/cssVars.ts";
    let mut builder = GraphBuilder::new();
    builder.add_node(module(root, NodeStatus::Modified, 0));
    builder.add_root(Node::module_id(root));
    builder.add_node(module(
        "packages/compiler-sfc/src/compileStyle.ts",
        NodeStatus::Unchanged,
        1,
    ));
    builder.add_node(module(
        "packages/compiler-sfc/src/parse.ts",
        NodeStatus::Added,
        1,
    ));
    builder.add_node(module(
        "packages/compiler-sfc/src/legacyParser.ts",
        NodeStatus::Removed,
        1,
    ));
    builder.add_node(module(
        "packages/compiler-sfc/__tests__/cssVars.spec.ts",
        NodeStatus::Added,
        1,
    ));
    builder.add_edge(edge(
        "packages/compiler-sfc/src/compileStyle.ts",
        root,
        Relation::Imports,
        EdgeStatus::Unchanged,
        Resolution::ResolvedSpecifier,
    ));
    builder.add_edge(edge(
        root,
        "packages/compiler-sfc/src/legacyParser.ts",
        Relation::Imports,
        EdgeStatus::Removed,
        Resolution::ResolvedSpecifier,
    ));
    builder.add_edge(edge(
        root,
        "packages/compiler-sfc/src/parse.ts",
        Relation::Imports,
        EdgeStatus::Added,
        Resolution::ResolvedSpecifier,
    ));
    builder.add_edge(edge(
        "packages/compiler-sfc/__tests__/cssVars.spec.ts",
        root,
        Relation::TestedBy,
        EdgeStatus::Added,
        Resolution::TestImportsModule,
    ));
    builder.finish(Limits::default())
}

#[test]
fn dependency_diff_matches_golden() {
    assert_eq!(dependency_diff(&graph()), DEPENDENCY_DIFF_GOLDEN);
}

#[test]
fn mermaid_matches_golden() {
    assert_eq!(mermaid(&graph()), MERMAID_GOLDEN);
}

/// The compact rendering of the fixture: one line per relationship, the removed
/// dependency beside the added one that replaced it.
const DEPENDENCY_DIFF_GOLDEN: &str = r"+ packages/compiler-sfc/__tests__/cssVars.spec.ts -[tested_by]-> packages/compiler-sfc/src/style/cssVars.ts
  packages/compiler-sfc/src/compileStyle.ts -> packages/compiler-sfc/src/style/cssVars.ts
- packages/compiler-sfc/src/style/cssVars.ts -> packages/compiler-sfc/src/legacyParser.ts
+ packages/compiler-sfc/src/style/cssVars.ts -> packages/compiler-sfc/src/parse.ts
";

/// The diagram of the fixture, with each node's shape declared where it first
/// appears. Only this source is deterministic; geometry belongs to whichever
/// Mermaid version draws it.
const MERMAID_GOLDEN: &str = r#"flowchart LR
    classDef added fill:#e6ffed,stroke:#22863a
    classDef removed fill:#ffeef0,stroke:#cb2431
    classDef modified fill:#fff5b1,stroke:#b08800
    classDef unchanged fill:#f6f8fa,stroke:#d1d5db
    n0["cssVars.spec.ts · added"] -. "~0.9" .-> n4["cssVars.ts · modified"]
    n1["compileStyle.ts"] --> n4
    n4 -. "removed" .-> n2["legacyParser.ts · removed"]
    n4 --> n3["parse.ts · added"]
    class n0 added
    class n1 unchanged
    class n2 removed
    class n3 added
    class n4 modified
"#;
