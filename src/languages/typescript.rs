use std::collections::{BTreeSet, HashMap, HashSet};

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
                    exports: BTreeSet::new(),
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
            exports: collect_exports(root, source_text),
            diagnostics,
        })
    }
}

struct FunctionCollector<'source> {
    language: Language,
    source: &'source str,
    functions: Vec<FunctionDefinition>,
    seen_starts: HashSet<usize>,
    anonymous_counts: HashMap<String, u32>,
}

impl<'source> FunctionCollector<'source> {
    fn new(language: Language, source: &'source str) -> Self {
        Self {
            language,
            source,
            functions: Vec::new(),
            seen_starts: HashSet::new(),
            anonymous_counts: HashMap::new(),
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
            body_hash: super::body_hash(node_text(node, self.source).unwrap_or_default()),
        });
        Ok(())
    }

    /// Name a function by its position in the source structure.
    ///
    /// The containing constructs are resolved first, because they scope both a
    /// named function's qualified name and an anonymous function's ordinal. An
    /// ordinal counted across the whole file makes a function's identity depend
    /// on how many anonymous functions happen to precede it, so inserting one
    /// callback renames every later one and matching cross-matches unrelated
    /// bodies. Counting within the containing construct keeps an identity
    /// stable under edits elsewhere in the file.
    fn qualified_name(&mut self, node: Node<'_>, kind: FunctionKind) -> String {
        let containers = container_path(node, self.source);
        let leaf = self
            .declared_name(node, kind)
            .unwrap_or_else(|| self.next_anonymous_name(&containers));

        let mut segments = containers;
        segments.push(leaf);
        segments.join(".")
    }

    fn declared_name(&self, node: Node<'_>, kind: FunctionKind) -> Option<String> {
        if kind == FunctionKind::Constructor {
            return Some("constructor".to_owned());
        }
        if let Some(name) = explicit_name(node, self.source) {
            return Some(name);
        }
        if matches!(kind, FunctionKind::ArrowFunction | FunctionKind::Function) {
            return assigned_name(node, self.source);
        }
        None
    }

    fn next_anonymous_name(&mut self, containers: &[String]) -> String {
        let scope = containers.join(".");
        let ordinal = self.anonymous_counts.entry(scope).or_insert(0);
        *ordinal += 1;
        format!("{ANONYMOUS_NAME}#{ordinal}")
    }
}

/// Leaf name given to a function that declares no name of its own.
const ANONYMOUS_NAME: &str = "<anonymous>";

/// Longest container segment kept before it is truncated.
///
/// Call labels come from source string literals, which are untrusted and can be
/// arbitrarily long. Truncation is deterministic so the same source always
/// produces the same identity.
const MAX_SEGMENT_BYTES: usize = 64;

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

/// Resolve the constructs that contain a function, outermost first.
///
/// Type and module declarations contribute their own name. A call expression
/// contributes the callee and, when the call leads with a string literal, that
/// literal: `describe("parser")` and `it("rejects empty input")` identify a
/// test callback far more stably than its position among the file's anonymous
/// functions, and survive both reordering and insertion.
fn container_path(node: Node<'_>, source: &str) -> Vec<String> {
    let mut containers = Vec::new();
    let mut child = node;
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
            "arguments" => {
                if let Some(segment) = call_segment(candidate, child, source) {
                    containers.push(segment);
                }
            }
            _ => {}
        }
        child = candidate;
        parent = candidate.parent();
    }

    containers.reverse();
    containers
}

/// Describe the call that receives `argument` as one identity segment.
fn call_segment(arguments: Node<'_>, argument: Node<'_>, source: &str) -> Option<String> {
    let call = arguments.parent()?;
    if !matches!(call.kind(), "call_expression" | "new_expression") {
        return None;
    }
    let callee = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("constructor"))
        .and_then(|callee| node_text(callee, source))
        .map(normalize_segment)?;

    let mut cursor = arguments.walk();
    let named = arguments.named_children(&mut cursor).collect::<Vec<_>>();

    if let Some(label) = named
        .iter()
        .find_map(|node| string_literal_text(*node, source))
    {
        return Some(format!("{callee}({label})"));
    }

    let index = named.iter().position(|node| node.id() == argument.id())?;
    Some(format!("{callee}#{index}"))
}

/// Read a plain string or substitution-free template literal as its text.
///
/// A template literal with substitutions is not a constant, so it cannot
/// identify anything stably and is deliberately not used as a label.
fn string_literal_text(node: Node<'_>, source: &str) -> Option<String> {
    let text = node_text(node, source)?;
    match node.kind() {
        "string" => Some(quote(&normalize_segment(text.trim_matches(['\'', '"'])))),
        // A template literal with substitutions is not a constant, so it cannot
        // identify anything stably and is deliberately not used as a label.
        "template_string" if node.named_child_count() == 0 => {
            Some(quote(&normalize_segment(text.trim_matches('`'))))
        }
        _ => None,
    }
}

fn quote(text: &str) -> String {
    format!("\"{text}\"")
}

/// Reduce source text to one deterministic, bounded identity segment.
fn normalize_segment(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= MAX_SEGMENT_BYTES {
        return collapsed;
    }
    let mut end = MAX_SEGMENT_BYTES;
    while end > 0 && !collapsed.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = collapsed[..end].to_owned();
    truncated.push_str("...");
    truncated
}

fn node_text<'source>(node: Node<'_>, source: &'source str) -> Option<&'source str> {
    source.get(node.byte_range())
}

fn clean_property_name(name: &str) -> String {
    name.trim_matches(['\'', '"', '`']).to_owned()
}

fn function_metrics(node: Node<'_>, source: &str) -> Result<FunctionMetrics, DiffScopeError> {
    // One traversal yields both metrics. Walking the function twice, once per
    // metric, doubles the most expensive step of the analyzer.
    let points = complexity_points(node, 0);
    Ok(FunctionMetrics {
        physical_loc: physical_loc(node, source)?,
        source_loc: source_loc(node, source)?,
        cyclomatic_complexity: 1 + points.cyclomatic,
        cognitive_complexity: points.cognitive,
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

/// Collect the names a module exports, as an importer would write them.
///
/// Only the export surface is described, never what the exported thing is: a
/// function that becomes a const of the same name is still the same name to
/// every caller, and a change that keeps the surface intact does not break
/// them. Re-exports of a whole module are recorded as `*` because their names
/// live in a file this analysis has not read.
fn collect_exports(root: Node<'_>, source: &str) -> BTreeSet<String> {
    let mut exports = BTreeSet::new();
    collect_exports_from(root, source, &mut exports);
    exports
}

fn collect_exports_from(node: Node<'_>, source: &str, exports: &mut BTreeSet<String>) {
    if node.kind() == "export_statement" {
        add_export_names(node, source, exports);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_exports_from(child, source, exports);
    }
}

fn add_export_names(statement: Node<'_>, source: &str, exports: &mut BTreeSet<String>) {
    if let Some(declaration) = statement.child_by_field_name("declaration") {
        add_declared_names(declaration, source, exports);
        return;
    }

    let mut cursor = statement.walk();
    let mut named_any = false;
    for child in statement.named_children(&mut cursor) {
        match child.kind() {
            "export_clause" => {
                let mut clause = child.walk();
                for specifier in child.named_children(&mut clause) {
                    let exported = specifier
                        .child_by_field_name("alias")
                        .or_else(|| specifier.child_by_field_name("name"));
                    if let Some(name) = exported.and_then(|name| node_text(name, source)) {
                        exports.insert(clean_property_name(name));
                        named_any = true;
                    }
                }
            }
            "namespace_export" => {
                exports.insert("*".to_owned());
                named_any = true;
            }
            _ => {}
        }
    }

    if named_any {
        return;
    }
    // `export default ...` and bare `export * from "..."` name nothing of their
    // own, so they are recorded by what they are.
    let text = node_text(statement, source).unwrap_or_default();
    if text.starts_with("export default") {
        exports.insert("default".to_owned());
    } else if text.starts_with("export *") {
        exports.insert("*".to_owned());
    }
}

fn add_declared_names(declaration: Node<'_>, source: &str, exports: &mut BTreeSet<String>) {
    match declaration.kind() {
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = declaration.walk();
            for declarator in declaration.named_children(&mut cursor) {
                if let Some(name) = declarator
                    .child_by_field_name("name")
                    .and_then(|name| node_text(name, source))
                {
                    exports.insert(clean_property_name(name));
                }
            }
        }
        _ => {
            if let Some(name) = explicit_name(declaration, source) {
                exports.insert(name);
            }
        }
    }
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
    fn identifies_callbacks_by_their_call_context() {
        let source = br"
describe('parser', () => {
  test('rejects empty input', () => {
    if (a) { return 1; }
  })
})
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let names = analysis
            .functions
            .iter()
            .map(|function| function.qualified_name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"describe(\"parser\").<anonymous>#1"));
        assert!(
            names.contains(&"describe(\"parser\").test(\"rejects empty input\").<anonymous>#1")
        );
    }

    #[test]
    fn counts_anonymous_functions_within_their_container_not_the_file() {
        // Each container restarts at #1. A file-wide counter would number the
        // second container's callback #2 and make its identity depend on how
        // many anonymous functions happen to precede it.
        let source = br"
describe('first', () => {
  test('a', () => { return 1; })
})
describe('second', () => {
  test('b', () => { return 2; })
})
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let names = analysis
            .functions
            .iter()
            .map(|function| function.qualified_name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"describe(\"first\").test(\"a\").<anonymous>#1"));
        assert!(names.contains(&"describe(\"second\").test(\"b\").<anonymous>#1"));
        assert!(!names.iter().any(|name| name.ends_with("#2")));
    }

    #[test]
    fn names_callback_arguments_without_a_label_by_position() {
        let source = br"
useEffect(() => { doThing(); }, [dep]);
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");

        assert_eq!(analysis.functions.len(), 1);
        assert_eq!(
            analysis.functions[0].qualified_name,
            "useEffect#0.<anonymous>#1"
        );
    }

    #[test]
    fn distinguishes_same_named_locals_declared_in_different_tests() {
        // Before identities were scoped, every `makeComp` in a spec file shared
        // one identity, collapsed into a single ambiguous group, and was
        // dropped from the inventory entirely.
        let source = br"
describe('suite', () => {
  test('a', () => { const makeComp = () => 1; })
  test('b', () => { const makeComp = () => 2; })
})
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let names = analysis
            .functions
            .iter()
            .map(|function| function.qualified_name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"describe(\"suite\").test(\"a\").makeComp"));
        assert!(names.contains(&"describe(\"suite\").test(\"b\").makeComp"));
    }

    #[test]
    fn ignores_template_literals_with_substitutions_as_labels() {
        let source = br"
test(`case ${index}`, () => { return 1; })
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");

        assert_eq!(analysis.functions[0].qualified_name, "test#1.<anonymous>#1");
    }

    #[test]
    fn truncates_overlong_call_labels_deterministically() {
        let label = "x".repeat(200);
        let source = format!("test('{label}', () => {{ return 1; }})\n");

        let analysis =
            analyze_source(Path::new("sample.ts"), source.as_bytes()).expect("analysis succeeds");
        let name = analysis.functions[0].qualified_name.as_str();

        assert!(name.starts_with("test(\"xxx"));
        assert!(name.contains("...\")"));
        assert!(name.len() < 120);
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
    fn reports_oversized_files_without_parsing_them() {
        use super::super::MAX_ANALYZED_BLOB_BYTES;

        let mut source = b"export function kept(): number { return 1; }\n".to_vec();
        source.resize(MAX_ANALYZED_BLOB_BYTES + 1, b'\n');

        let analysis =
            analyze_source(Path::new("generated.ts"), &source).expect("analysis succeeds");

        assert_eq!(analysis.language, Some(Language::TypeScript));
        assert_eq!(analysis.functions, Vec::new());
        assert!(analysis.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == LanguageDiagnosticCode::OversizedFile
                && diagnostic.severity == DiagnosticSeverity::Warning
        }));
    }

    #[test]
    fn analyzes_files_at_the_size_limit() {
        use super::super::MAX_ANALYZED_BLOB_BYTES;

        let mut source = b"export function kept(): number { return 1; }\n".to_vec();
        source.resize(MAX_ANALYZED_BLOB_BYTES, b'\n');

        let analysis =
            analyze_source(Path::new("generated.ts"), &source).expect("analysis succeeds");

        assert_eq!(analysis.functions.len(), 1);
        assert_eq!(analysis.functions[0].qualified_name, "kept");
        assert!(analysis.diagnostics.is_empty());
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
