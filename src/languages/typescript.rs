use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use crate::{DiffScopeError, metrics::FunctionMetrics};

use super::{
    DiagnosticSeverity, FunctionDefinition, FunctionKind, Language, LanguageDiagnostic,
    LanguageDiagnosticCode, SourceAnalysis, SourceRange,
};

pub struct TypeScriptAnalyzer {
    language: Language,
    parser: Parser,
}

impl TypeScriptAnalyzer {
    /// Create a TypeScript or TSX analyzer.
    ///
    /// # Errors
    ///
    /// Returns an error when the tree-sitter grammar cannot be loaded.
    pub fn new(language: Language) -> Result<Self, DiffScopeError> {
        let mut parser = Parser::new();
        let grammar = match language {
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX,
        };
        parser.set_language(&grammar.into()).map_err(|error| {
            DiffScopeError::Language(format!("load TypeScript grammar: {error}"))
        })?;
        Ok(Self { language, parser })
    }

    /// Analyze a TypeScript source blob.
    ///
    /// # Errors
    ///
    /// Returns an error when tree-sitter does not produce a syntax tree or when
    /// source coordinates exceed the public result type.
    pub fn analyze(&mut self, source: &[u8]) -> Result<SourceAnalysis, DiffScopeError> {
        let source_text = match std::str::from_utf8(source) {
            Ok(source_text) => source_text,
            Err(error) => {
                return Ok(SourceAnalysis {
                    language: Some(self.language),
                    functions: Vec::new(),
                    diagnostics: vec![LanguageDiagnostic {
                        code: LanguageDiagnosticCode::InvalidUtf8,
                        severity: DiagnosticSeverity::Error,
                        message: format!("source is not valid UTF-8: {error}"),
                        range: None,
                    }],
                });
            }
        };

        let tree = self
            .parser
            .parse(source_text, None)
            .ok_or_else(|| DiffScopeError::Language("tree-sitter returned no tree".to_owned()))?;
        let root = tree.root_node();
        let mut diagnostics = Vec::new();
        if root.has_error() {
            diagnostics.push(LanguageDiagnostic {
                code: LanguageDiagnosticCode::MalformedSource,
                severity: DiagnosticSeverity::Warning,
                message: "source contains syntax errors; function inventory may be incomplete"
                    .to_owned(),
                range: Some(SourceRange::from_tree_sitter(root.range())?),
            });
        }

        let mut collector = FunctionCollector::new(self.language, source_text);
        collector.visit(root)?;
        let mut functions = collector.functions;
        functions.sort_by(|left, right| {
            left.range
                .start_line
                .cmp(&right.range.start_line)
                .then_with(|| left.range.start_column.cmp(&right.range.start_column))
                .then_with(|| left.qualified_name.cmp(&right.qualified_name))
                .then_with(|| format!("{:?}", left.kind).cmp(&format!("{:?}", right.kind)))
        });

        Ok(SourceAnalysis {
            language: Some(self.language),
            functions,
            diagnostics,
        })
    }
}

struct FunctionCollector<'source> {
    language: Language,
    source: &'source str,
    functions: Vec<FunctionDefinition>,
    seen_starts: HashSet<usize>,
    anonymous_count: u32,
}

impl<'source> FunctionCollector<'source> {
    fn new(language: Language, source: &'source str) -> Self {
        Self {
            language,
            source,
            functions: Vec::new(),
            seen_starts: HashSet::new(),
            anonymous_count: 0,
        }
    }

    fn visit(&mut self, node: Node<'_>) -> Result<(), DiffScopeError> {
        self.collect_node(node)?;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child)?;
        }
        Ok(())
    }

    fn collect_node(&mut self, node: Node<'_>) -> Result<(), DiffScopeError> {
        let Some(kind) = function_kind(node, self.source) else {
            return Ok(());
        };
        if !self.seen_starts.insert(node.start_byte()) {
            return Ok(());
        }

        let qualified_name = self.qualified_name(node, kind);
        self.functions.push(FunctionDefinition {
            language: self.language,
            kind,
            qualified_name,
            range: SourceRange::from_tree_sitter(node.range())?,
            metrics: function_metrics(node, self.source)?,
        });
        Ok(())
    }

    fn qualified_name(&mut self, node: Node<'_>, kind: FunctionKind) -> String {
        if kind == FunctionKind::Constructor {
            return qualify_with_containers(node, self.source, "constructor");
        }

        if let Some(name) = explicit_name(node, self.source) {
            return qualify_with_containers(node, self.source, &name);
        }

        if matches!(kind, FunctionKind::ArrowFunction | FunctionKind::Function)
            && let Some(name) = assigned_name(node, self.source)
        {
            return qualify_with_containers(node, self.source, &name);
        }

        self.anonymous_count += 1;
        qualify_with_containers(
            node,
            self.source,
            &format!("<anonymous>#{}", self.anonymous_count),
        )
    }
}

fn function_kind(node: Node<'_>, source: &str) -> Option<FunctionKind> {
    match node.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "function_expression"
        | "generator_function" => Some(FunctionKind::Function),
        "method_definition" | "abstract_method_signature" | "method_signature" => {
            if explicit_name(node, source).as_deref() == Some("constructor") {
                Some(FunctionKind::Constructor)
            } else {
                Some(FunctionKind::Method)
            }
        }
        "arrow_function" => Some(FunctionKind::ArrowFunction),
        _ => None,
    }
}

fn explicit_name(node: Node<'_>, source: &str) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    node_text(name, source).map(clean_property_name)
}

fn assigned_name(node: Node<'_>, source: &str) -> Option<String> {
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        match candidate.kind() {
            "variable_declarator"
            | "assignment_expression"
            | "pair"
            | "public_field_definition" => {
                if let Some(name) = candidate
                    .child_by_field_name("name")
                    .or_else(|| candidate.child_by_field_name("left"))
                    .or_else(|| candidate.child_by_field_name("key"))
                    .and_then(|name| node_text(name, source))
                {
                    return Some(clean_property_name(name));
                }
            }
            "statement_block" | "program" | "class_body" => return None,
            _ => {}
        }
        parent = candidate.parent();
    }
    None
}

fn qualify_with_containers(node: Node<'_>, source: &str, name: &str) -> String {
    let mut containers = Vec::new();
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        match candidate.kind() {
            "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "module"
            | "internal_module" => {
                if let Some(container) = explicit_name(candidate, source) {
                    containers.push(container);
                }
            }
            _ => {}
        }
        parent = candidate.parent();
    }
    containers.reverse();
    containers.push(name.to_owned());
    containers.join(".")
}

fn node_text<'source>(node: Node<'_>, source: &'source str) -> Option<&'source str> {
    source.get(node.byte_range())
}

fn clean_property_name(name: &str) -> String {
    name.trim_matches(['\'', '"', '`']).to_owned()
}

fn function_metrics(node: Node<'_>, source: &str) -> Result<FunctionMetrics, DiffScopeError> {
    Ok(FunctionMetrics {
        physical_loc: physical_loc(node, source)?,
        source_loc: source_loc(node, source)?,
        cyclomatic_complexity: cyclomatic_complexity(node),
        cognitive_complexity: cognitive_complexity(node),
    })
}

fn physical_loc(node: Node<'_>, source: &str) -> Result<u32, DiffScopeError> {
    let text = node_text(node, source).ok_or_else(|| {
        DiffScopeError::Language("function source range is not a UTF-8 boundary".to_owned())
    })?;
    if text.is_empty() {
        return Ok(0);
    }
    u32::try_from(text.lines().count()).map_err(|error| {
        DiffScopeError::Language(format!("physical LOC does not fit in u32: {error}"))
    })
}

fn source_loc(node: Node<'_>, source: &str) -> Result<u32, DiffScopeError> {
    let text = node_text(node, source).ok_or_else(|| {
        DiffScopeError::Language("function source range is not a UTF-8 boundary".to_owned())
    })?;
    let uncommented = remove_comments(text);
    let count = uncommented
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    u32::try_from(count).map_err(|error| {
        DiffScopeError::Language(format!("source LOC does not fit in u32: {error}"))
    })
}

fn remove_comments(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_string: Option<char> = None;
    let mut escaped = false;

    while let Some(character) = chars.next() {
        if in_line_comment {
            if character == '\n' {
                in_line_comment = false;
                output.push('\n');
            } else {
                output.push(' ');
            }
            continue;
        }

        if in_block_comment {
            if character == '*' && chars.peek() == Some(&'/') {
                let _ignored = chars.next();
                in_block_comment = false;
                output.push(' ');
                output.push(' ');
            } else if character == '\n' {
                output.push('\n');
            } else {
                output.push(' ');
            }
            continue;
        }

        if let Some(quote) = in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote {
                in_string = None;
            }
            continue;
        }

        if matches!(character, '\'' | '"' | '`') {
            in_string = Some(character);
            output.push(character);
        } else if character == '/' && chars.peek() == Some(&'/') {
            let _ignored = chars.next();
            in_line_comment = true;
            output.push(' ');
            output.push(' ');
        } else if character == '/' && chars.peek() == Some(&'*') {
            let _ignored = chars.next();
            in_block_comment = true;
            output.push(' ');
            output.push(' ');
        } else {
            output.push(character);
        }
    }
    output
}

fn cyclomatic_complexity(node: Node<'_>) -> u32 {
    1 + complexity_points(node, 0).cyclomatic
}

fn cognitive_complexity(node: Node<'_>) -> u32 {
    complexity_points(node, 0).cognitive
}

#[derive(Debug, Default)]
struct ComplexityPoints {
    cyclomatic: u32,
    cognitive: u32,
}

fn complexity_points(node: Node<'_>, nesting: u32) -> ComplexityPoints {
    let mut points = ComplexityPoints::default();
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if child.start_byte() != node.start_byte() && function_kind(child, "").is_some() {
            continue;
        }

        let kind = child.kind();
        if is_cyclomatic_decision(kind) {
            points.cyclomatic += 1;
        }
        if is_cognitive_decision(kind) {
            points.cognitive += 1 + nesting;
            let child_points = complexity_points(child, nesting + 1);
            points.cyclomatic += child_points.cyclomatic;
            points.cognitive += child_points.cognitive;
            continue;
        }
        if kind == "binary_expression" && is_short_circuit_operator(child) {
            points.cyclomatic += 1;
            points.cognitive += 1;
        }

        let child_points = complexity_points(child, nesting);
        points.cyclomatic += child_points.cyclomatic;
        points.cognitive += child_points.cognitive;
    }

    points
}

fn is_cyclomatic_decision(kind: &str) -> bool {
    matches!(
        kind,
        "if_statement"
            | "for_statement"
            | "for_in_statement"
            | "while_statement"
            | "do_statement"
            | "catch_clause"
            | "ternary_expression"
            | "switch_case"
    )
}

fn is_cognitive_decision(kind: &str) -> bool {
    matches!(
        kind,
        "if_statement"
            | "for_statement"
            | "for_in_statement"
            | "while_statement"
            | "do_statement"
            | "catch_clause"
            | "ternary_expression"
            | "switch_case"
    )
}

fn is_short_circuit_operator(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| matches!(child.kind(), "&&" | "||" | "??"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::{
        DiagnosticSeverity, FunctionKind, Language, LanguageDiagnosticCode, analyze_source,
        detect_language,
    };

    #[test]
    fn detects_typescript_extensions() {
        assert_eq!(
            detect_language(Path::new("index.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(detect_language(Path::new("index.tsx")), Some(Language::Tsx));
        assert_eq!(detect_language(Path::new("index.js")), None);
    }

    #[test]
    fn inventories_typescript_functions_and_methods() {
        let source = br"
function topLevel(value: number): number { return value + 1; }
const assigned = (name: string) => name.toUpperCase();
class Greeter {
  constructor(private name: string) {}
  greet(): string { return this.name; }
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let names = analysis
            .functions
            .iter()
            .map(|function| (function.qualified_name.as_str(), function.kind))
            .collect::<Vec<_>>();

        assert_eq!(analysis.language, Some(Language::TypeScript));
        assert!(analysis.diagnostics.is_empty());
        assert!(names.contains(&("topLevel", FunctionKind::Function)));
        assert!(names.contains(&("assigned", FunctionKind::ArrowFunction)));
        assert!(names.contains(&("Greeter.constructor", FunctionKind::Constructor)));
        assert!(names.contains(&("Greeter.greet", FunctionKind::Method)));
    }

    #[test]
    fn calculates_loc_metrics() {
        let source = br"
function measured() {
  // comment only

  return 1; // mixed code and comment
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let metrics = &analysis.functions[0].metrics;

        assert_eq!(metrics.physical_loc, 5);
        assert_eq!(metrics.source_loc, 3);
    }

    #[test]
    fn calculates_complexity_metrics() {
        let source = br"
function complex(a: boolean, b: boolean, items: number[]) {
  if (a && b) {
    for (const item of items) { console.log(item); }
  } else {
    while (b) { break; }
  }
  return a ? 1 : 0;
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let metrics = &analysis.functions[0].metrics;

        assert_eq!(metrics.cyclomatic_complexity, 6);
        assert_eq!(metrics.cognitive_complexity, 7);
    }

    #[test]
    fn inventories_function_expressions_over_their_whole_range() {
        let source = br"
const named = function (flag: boolean): number {
  if (flag) {
    return 1;
  }
  return 0;
};
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");

        assert_eq!(analysis.functions.len(), 1);
        let function = &analysis.functions[0];
        assert_eq!(function.qualified_name, "named");
        assert_eq!(function.kind, FunctionKind::Function);
        assert_eq!(function.metrics.physical_loc, 6);
        assert_eq!(function.metrics.cyclomatic_complexity, 2);
        assert_eq!(function.metrics.cognitive_complexity, 1);
    }

    #[test]
    fn inventories_async_declarations_once() {
        let source = br"
async function loadAll(urls: string[]): Promise<string[]> {
  return urls;
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");

        assert_eq!(analysis.functions.len(), 1);
        assert_eq!(analysis.functions[0].qualified_name, "loadAll");
        assert_eq!(analysis.functions[0].metrics.physical_loc, 3);
    }

    #[test]
    fn counts_switch_case_clauses_as_decisions() {
        let source = br#"
function classify(kind: string): number {
  switch (kind) {
    case "a":
      return 1;
    case "b":
      return 2;
    case "c":
      return 3;
    default:
      return 0;
  }
}
"#;

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let metrics = &analysis.functions[0].metrics;

        assert_eq!(metrics.cyclomatic_complexity, 4);
        assert_eq!(metrics.cognitive_complexity, 3);
    }

    #[test]
    fn nests_decisions_inside_switch_case_clauses() {
        let source = br#"
function classify(kind: string, flag: boolean): number {
  switch (kind) {
    case "a":
      if (flag) {
        return 1;
      }
      return 2;
    default:
      return 0;
  }
}
"#;

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let metrics = &analysis.functions[0].metrics;

        assert_eq!(metrics.cyclomatic_complexity, 3);
        assert_eq!(metrics.cognitive_complexity, 3);
    }

    #[test]
    fn excludes_nested_functions_from_complexity_metrics() {
        let source = br"
function outer() {
  function inner() {
    if (true) { return 1; }
  }
  return 0;
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let outer = analysis
            .functions
            .iter()
            .find(|function| function.qualified_name == "outer")
            .expect("outer function exists");

        assert_eq!(outer.metrics.cyclomatic_complexity, 1);
        assert_eq!(outer.metrics.cognitive_complexity, 0);
    }

    #[test]
    fn reports_malformed_source_without_panicking() {
        let analysis = analyze_source(Path::new("broken.ts"), b"function broken( {")
            .expect("analysis succeeds");

        assert!(analysis.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == LanguageDiagnosticCode::MalformedSource
                && diagnostic.severity == DiagnosticSeverity::Warning
        }));
    }

    #[test]
    fn reports_invalid_utf8_without_panicking() {
        let analysis =
            analyze_source(Path::new("bad.ts"), &[0xff, 0xfe]).expect("analysis succeeds");

        assert_eq!(analysis.functions, Vec::new());
        assert!(analysis.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == LanguageDiagnosticCode::InvalidUtf8
                && diagnostic.severity == DiagnosticSeverity::Error
        }));
    }

    #[test]
    fn reports_unsupported_language() {
        let analysis =
            analyze_source(Path::new("readme.md"), b"# title").expect("analysis succeeds");

        assert_eq!(analysis.language, None);
        assert!(analysis.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == LanguageDiagnosticCode::UnsupportedLanguage
                && diagnostic.severity == DiagnosticSeverity::Info
        }));
    }
}
