use diffscope::{
    AnalysisResult, AnalysisSummary, Diagnostic, DiagnosticCode, DiagnosticCounts, FileResult,
    RevisionResult,
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
