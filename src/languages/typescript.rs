use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use tree_sitter::{Node, Parser};

use crate::{DiffScopeError, metrics::FunctionMetrics};

use super::{
    CallSite, DiagnosticSeverity, ExportedSymbol, FunctionDefinition, FunctionKind, ImportBinding,
    ImportFacts, ImportedName, Language, LanguageDiagnostic, LanguageDiagnosticCode,
    SourceAnalysis, SourceRange,
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
                    exported_symbols: BTreeMap::new(),
                    re_exported_modules: BTreeSet::new(),
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

        let exported = collect_exported_symbols(root, source_text, &functions);
        Ok(SourceAnalysis {
            language: Some(self.language),
            functions,
            exports: collect_exports(root, source_text),
            exported_symbols: exported.symbols,
            re_exported_modules: exported.modules,
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
        let facts = function_facts(node, self.source)?;
        self.functions.push(FunctionDefinition {
            language: self.language,
            kind,
            qualified_name,
            range: SourceRange::from_tree_sitter(node.range())?,
            metrics: facts.metrics,
            calls: facts.calls,
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

/// Everything the function collector reads from one function in one walk.
struct FunctionFacts {
    metrics: FunctionMetrics,
    calls: Vec<CallSite>,
}

fn function_facts(node: Node<'_>, source: &str) -> Result<FunctionFacts, DiffScopeError> {
    // One traversal yields the metrics and the call sites. Walking the function
    // twice, once per concern, doubles the most expensive step of the analyzer,
    // which `PERFORMANCE.md` attributes 53% of analysis time to.
    let facts = body_facts(node, source, 0);
    Ok(FunctionFacts {
        metrics: FunctionMetrics {
            physical_loc: physical_loc(node, source)?,
            source_loc: source_loc(node, source)?,
            cyclomatic_complexity: 1 + facts.cyclomatic,
            cognitive_complexity: facts.cognitive,
        },
        calls: facts.calls,
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

/// What one recursive walk of a function body yields.
///
/// Complexity and call sites are read together because this walk is the
/// analyzer's most expensive step: a second pass over every body would pay it
/// twice for facts that are already in hand at each node.
#[derive(Debug, Default)]
struct BodyFacts {
    cyclomatic: u32,
    cognitive: u32,
    /// Calls reached by this walk, in source order, duplicates kept.
    calls: Vec<CallSite>,
}

fn body_facts(node: Node<'_>, source: &str, nesting: u32) -> BodyFacts {
    let mut facts = BodyFacts::default();
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        // recurse, but stop at a nested function's boundary
        if child.start_byte() != node.start_byte() && function_kind(child, "").is_some() {
            continue;
        }

        // A call is read from the child this walk already holds, so collecting
        // it costs no traversal of its own.
        if let Some(call) = call_site(child, source) {
            facts.calls.push(call);
        }

        let kind = child.kind();
        if is_cyclomatic_decision(kind) {
            facts.cyclomatic += 1;
        }
        if is_cognitive_decision(kind) {
            facts.cognitive += 1 + nesting;
            let child_facts = body_facts(child, source, nesting + 1);
            facts.cyclomatic += child_facts.cyclomatic;
            facts.cognitive += child_facts.cognitive;
            facts.calls.extend(child_facts.calls);
            continue;
        }
        if kind == "binary_expression" && is_short_circuit_operator(child) {
            facts.cyclomatic += 1;
            facts.cognitive += 1;
        }

        let child_facts = body_facts(child, source, nesting);
        facts.cyclomatic += child_facts.cyclomatic;
        facts.cognitive += child_facts.cognitive;
        facts.calls.extend(child_facts.calls);
    }

    facts
}

/// Read the call one node records, when it is a call with a plain identifier
/// callee.
///
/// `new Foo()` names its callee in a `constructor` field and `Foo()` in a
/// `function` field; that is the only difference between the two here. Every
/// other callee shape -- a member expression, a computed access, a call on a
/// call -- records nothing rather than a lower-confidence guess: the call graph
/// shows a relationship it is sure of or shows none.
///
/// The line is the call expression's own, not the callee's: that is the
/// position a reader jumps to to see the call, and a call spread over several
/// lines starts where its callee does not.
fn call_site(node: Node<'_>, source: &str) -> Option<CallSite> {
    let field = match node.kind() {
        "call_expression" => "function",
        "new_expression" => "constructor",
        _ => return None,
    };
    let callee = node.child_by_field_name(field)?;
    if callee.kind() != "identifier" {
        return None;
    }
    let name = node_text(callee, source)?;
    Some(CallSite {
        name: name.to_owned(),
        line: start_line(node),
    })
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

impl TypeScriptAnalyzer {
    /// Collect the module specifiers a source prefix imports or re-exports,
    /// each with the 1-based line its statement sits on, and the local names
    /// the file binds from them.
    ///
    /// The input may be a truncated file, so the tree is expected to contain
    /// errors near its end. Tree-sitter still yields the statements it did
    /// parse, which is every import that fitted in the prefix.
    ///
    /// # Errors
    ///
    /// Returns an error when tree-sitter does not produce a syntax tree.
    pub fn scan_imports(&mut self, source: &[u8]) -> Result<ImportFacts, DiffScopeError> {
        let Ok(source_text) = std::str::from_utf8(source) else {
            return Ok(ImportFacts::default());
        };
        let tree = self
            .parser
            .parse(source_text, None)
            .ok_or_else(|| DiffScopeError::Language("tree-sitter returned no tree".to_owned()))?;

        let mut facts = ImportFacts::default();
        collect_specifiers(tree.root_node(), source_text, &mut facts);
        Ok(facts)
    }
}

/// Walk for the `source` of every import and re-export statement, and for the
/// names each import binds.
///
/// A plain `export { a }` has no source and adds no edge; only a statement that
/// names another module does. Dynamic `import(...)` is deliberately not
/// followed: its argument is an expression that need not be a literal, and
/// guessing at one would invent edges that may not exist.
///
/// The line recorded is the specifier's own, not the statement's first line:
/// that is the token a reader looks at to confirm the edge, and a multiline
/// import puts it somewhere the statement's start does not point to. Two
/// statements importing one module are one edge, so the earliest line wins.
///
/// Bindings are read from the same statement in the same visit. An
/// `export ... from` clause binds no local name, so it contributes a specifier
/// and nothing else.
fn collect_specifiers(node: Node<'_>, source: &str, facts: &mut ImportFacts) {
    if matches!(node.kind(), "import_statement" | "export_statement")
        && let Some(module) = node.child_by_field_name("source")
        && let Some(text) = node_text(module, source)
    {
        let line = start_line(module);
        let specifier = clean_property_name(text);
        facts
            .specifiers
            .entry(specifier.clone())
            .and_modify(|existing| *existing = (*existing).min(line))
            .or_insert(line);
        if node.kind() == "import_statement" {
            collect_bindings(node, source, &specifier, line, &mut facts.bindings);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_specifiers(child, source, facts);
    }
}

/// Record the local names one import statement binds.
///
/// A type-only import is recorded like any other named import. The graph
/// reports an edge only when the name resolves to a function definition, so a
/// type never produces one, and dropping type imports here would put a second
/// rule about what a name may be in a second place.
fn collect_bindings(
    statement: Node<'_>,
    source: &str,
    specifier: &str,
    line: u32,
    bindings: &mut BTreeMap<String, ImportBinding>,
) {
    let mut cursor = statement.walk();
    for clause in statement.named_children(&mut cursor) {
        if clause.kind() != "import_clause" {
            continue;
        }
        let mut parts = clause.walk();
        for part in clause.named_children(&mut parts) {
            match part.kind() {
                // The clause's bare identifier is the default import.
                "identifier" => bind(
                    bindings,
                    node_text(part, source),
                    &ImportedName::Default,
                    specifier,
                    line,
                ),
                "namespace_import" => {
                    let local = part.named_child(0).and_then(|name| node_text(name, source));
                    bind(bindings, local, &ImportedName::Namespace, specifier, line);
                }
                "named_imports" => {
                    let mut named = part.walk();
                    for import in part.named_children(&mut named) {
                        let Some(name) = import
                            .child_by_field_name("name")
                            .and_then(|name| node_text(name, source))
                        else {
                            continue;
                        };
                        let alias = import
                            .child_by_field_name("alias")
                            .and_then(|alias| node_text(alias, source));
                        bind(
                            bindings,
                            Some(alias.unwrap_or(name)),
                            &ImportedName::Named(clean_property_name(name)),
                            specifier,
                            line,
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

/// Bind one local name, keeping the first statement that bound it.
///
/// A local name is bound once per module in well-formed TypeScript. Keeping
/// the first is what a truncated prefix or a malformed file degrades to, and
/// it is stable: the walk visits statements in source order.
fn bind(
    bindings: &mut BTreeMap<String, ImportBinding>,
    local: Option<&str>,
    imported: &ImportedName,
    specifier: &str,
    line: u32,
) {
    let Some(local) = local
        .map(clean_property_name)
        .filter(|name| !name.is_empty())
    else {
        return;
    };
    bindings.entry(local).or_insert_with(|| ImportBinding {
        imported: imported.clone(),
        specifier: specifier.to_owned(),
        line,
    });
}

/// The 1-based line a node starts on, for the evidence a caller renders.
fn start_line(node: Node<'_>) -> u32 {
    u32::try_from(node.start_position().row.saturating_add(1)).unwrap_or(u32::MAX)
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

/// Every export record one module declares, and the modules it forwards from.
#[derive(Debug, Default)]
struct ExportRecords {
    symbols: BTreeMap<String, ExportedSymbol>,
    modules: BTreeSet<String>,
}

/// Describe each exported name by what stands behind it.
///
/// This runs beside [`collect_exports`] rather than replacing it: the export
/// surface is a set of names and is compared as one, while resolution needs
/// the name behind each export and the definition it leads to. Keeping them
/// apart means a change here cannot move an export-surface delta.
fn collect_exported_symbols(
    root: Node<'_>,
    source: &str,
    functions: &[FunctionDefinition],
) -> ExportRecords {
    let mut records = ExportRecords::default();
    collect_export_records(root, source, &mut records);
    attach_function_ranges(&mut records.symbols, functions);
    records
}

fn collect_export_records(node: Node<'_>, source: &str, records: &mut ExportRecords) {
    if node.kind() == "export_statement" {
        add_export_records(node, source, records);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_export_records(child, source, records);
    }
}

/// Record what one export statement publishes, and from where.
///
/// A whole-module re-export publishes no name this analysis can see, so it
/// contributes only the module it forwards from: the names behind `export *`
/// live in a file that was never read, and inventing one of them would invent
/// an edge.
fn add_export_records(statement: Node<'_>, source: &str, records: &mut ExportRecords) {
    let from = statement
        .child_by_field_name("source")
        .and_then(|module| node_text(module, source))
        .map(clean_property_name)
        .filter(|specifier| !specifier.is_empty());
    if let Some(specifier) = &from {
        records.modules.insert(specifier.clone());
    }

    let line = start_line(statement);
    let text = node_text(statement, source).unwrap_or_default();
    let default = text.starts_with("export default");

    if let Some(declaration) = statement.child_by_field_name("declaration") {
        let mut declared = BTreeSet::new();
        add_declared_names(declaration, source, &mut declared);
        if default {
            // `export default function f() {}` publishes `default`; `f` is the
            // name the definition is found under in this file.
            let local = declared
                .into_iter()
                .next()
                .unwrap_or_else(|| "default".to_owned());
            record(records, "default", local, from, line);
            return;
        }
        for name in declared {
            record(records, &name.clone(), name, from.clone(), line);
        }
        return;
    }

    let mut cursor = statement.walk();
    let mut clause_named = false;
    for child in statement.named_children(&mut cursor) {
        match child.kind() {
            "export_clause" => {
                let mut clause = child.walk();
                for specifier in child.named_children(&mut clause) {
                    let Some(local) = specifier
                        .child_by_field_name("name")
                        .and_then(|name| node_text(name, source))
                    else {
                        continue;
                    };
                    let exported = specifier
                        .child_by_field_name("alias")
                        .and_then(|alias| node_text(alias, source))
                        .unwrap_or(local);
                    record(
                        records,
                        &clean_property_name(exported),
                        clean_property_name(local),
                        from.clone(),
                        line,
                    );
                    clause_named = true;
                }
            }
            "namespace_export" => {
                // `export * as ns from "x"` publishes the module itself under
                // one name. The name behind it is `*`, which no module exports,
                // so a lookup through it stops rather than guessing.
                if let Some(name) = child
                    .named_child(0)
                    .and_then(|name| node_text(name, source))
                {
                    record(
                        records,
                        &clean_property_name(name),
                        "*".to_owned(),
                        from.clone(),
                        line,
                    );
                    clause_named = true;
                }
            }
            _ => {}
        }
    }

    if clause_named || !default {
        return;
    }
    // `export default <expression>`: a bare identifier names something this
    // file declares, and anything else is an expression with no name to give.
    let local = statement
        .child_by_field_name("value")
        .filter(|value| value.kind() == "identifier")
        .and_then(|value| node_text(value, source))
        .map_or_else(|| "default".to_owned(), clean_property_name);
    record(records, "default", local, from, line);
}

fn record(
    records: &mut ExportRecords,
    exported: &str,
    local: String,
    from: Option<String>,
    line: u32,
) {
    if exported.is_empty() {
        return;
    }
    records
        .symbols
        .entry(exported.to_owned())
        .or_insert(ExportedSymbol {
            local,
            from,
            function: None,
            line,
        });
}

/// Point each locally declared export at the function it names, when it names
/// one.
///
/// The name compared is the leaf of the qualified name, which is what an
/// importer writes. A leaf two definitions share stays unresolved: two
/// same-named functions in one file are an ambiguity the analysis already
/// reports, and picking one of them would be worse than saying nothing.
fn attach_function_ranges(
    symbols: &mut BTreeMap<String, ExportedSymbol>,
    functions: &[FunctionDefinition],
) {
    let mut by_name: BTreeMap<&str, Option<&FunctionDefinition>> = BTreeMap::new();
    for function in functions {
        let leaf = function
            .qualified_name
            .rsplit('.')
            .next()
            .unwrap_or(&function.qualified_name);
        by_name
            .entry(leaf)
            .and_modify(|slot| *slot = None)
            .or_insert(Some(function));
    }
    for symbol in symbols.values_mut() {
        if symbol.from.is_some() {
            continue;
        }
        if let Some(Some(function)) = by_name.get(symbol.local.as_str()) {
            symbol.function = Some(function.range.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::{
        CallSite, DiagnosticSeverity, FunctionKind, ImportedName, Language, LanguageDiagnosticCode,
        analyze_source, detect_language,
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
    fn collects_calls_to_locally_declared_functions() {
        let source = br"
function helper(): number { return 1; }

function caller(): number {
  return helper();
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let caller = analysis
            .functions
            .iter()
            .find(|function| function.qualified_name == "caller")
            .expect("caller function exists");

        assert_eq!(
            caller.calls,
            vec![CallSite {
                name: "helper".to_owned(),
                line: 5,
            }]
        );
    }

    #[test]
    fn attributes_calls_inside_nested_functions_to_the_nested_function() {
        // The mirror of the complexity boundary. Attributing the arrow
        // function's call upward would make `outer` appear to call something
        // it does not, and the arrow function is a node of its own.
        let source = br"
function outer() {
  const inner = () => helper();
  other();
  return inner;
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let outer = analysis
            .functions
            .iter()
            .find(|function| function.qualified_name == "outer")
            .expect("outer function exists");
        let inner = analysis
            .functions
            .iter()
            .find(|function| function.qualified_name == "inner")
            .expect("inner function exists");

        assert_eq!(
            inner.calls,
            vec![CallSite {
                name: "helper".to_owned(),
                line: 3,
            }]
        );
        assert_eq!(
            outer.calls,
            vec![CallSite {
                name: "other".to_owned(),
                line: 4,
            }]
        );
    }

    #[test]
    fn ignores_callees_that_are_not_plain_identifiers() {
        // A member callee, a computed one, and a call on a call each name
        // something this stage cannot resolve, so they record nothing. The
        // plain call inside the first one's arguments is still reached by the
        // walk and is still a call to a name this file could declare.
        let source = br"
function indirect(holder: Holder, key: string) {
  holder.method(direct());
  holder[key]();
  holder.create()();
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let indirect = &analysis.functions[0];

        assert_eq!(
            indirect.calls,
            vec![CallSite {
                name: "direct".to_owned(),
                line: 3,
            }]
        );
    }

    #[test]
    fn collects_constructor_calls() {
        let source = br"
function build(factory: Factory): Widget {
  const made = new factory.Widget(1);
  return new Widget(made);
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let build = &analysis.functions[0];

        // A `new_expression` names its callee in a different field than a
        // `call_expression` does, and a member constructor is no more a plain
        // identifier than a member call is.
        assert_eq!(
            build.calls,
            vec![CallSite {
                name: "Widget".to_owned(),
                line: 4,
            }]
        );
    }

    #[test]
    fn collects_recursive_calls() {
        let source = br"
function countdown(value: number): number {
  if (value <= 0) {
    return 0;
  }
  return countdown(value - 1);
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let countdown = &analysis.functions[0];

        assert_eq!(
            countdown.calls,
            vec![CallSite {
                name: "countdown".to_owned(),
                line: 6,
            }]
        );
    }

    #[test]
    fn keeps_calls_in_source_order_with_duplicates_across_decisions() {
        // The walk reaches a call through three different arms -- plain
        // children, decisions, and short-circuit operands -- and a call inside
        // a decision is collected by that decision's own recursion. Order and
        // duplicates are what the graph emits edges from, so both are pinned
        // here.
        let source = br"
function mixed(flag: boolean) {
  second();
  if (flag) {
    first();
    second();
  }
  flag ? first() : second();
}
";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let mixed = &analysis.functions[0];

        assert_eq!(
            mixed.calls,
            vec![
                CallSite {
                    name: "second".to_owned(),
                    line: 3,
                },
                CallSite {
                    name: "first".to_owned(),
                    line: 5,
                },
                CallSite {
                    name: "second".to_owned(),
                    line: 6,
                },
                CallSite {
                    name: "first".to_owned(),
                    line: 8,
                },
                CallSite {
                    name: "second".to_owned(),
                    line: 8,
                },
            ]
        );
    }

    #[test]
    fn keeps_metrics_and_body_hashes_unchanged_by_call_collection() {
        // Calls come out of the same walk as the metrics, so they change
        // neither the metrics nor the body hash: a body that only calls is as
        // simple as an empty one, and its hash is still its source text with
        // whitespace collapsed.
        let source = br"
function onlyCalls() {
  first();
  second();
  new Third();
}
";
        let body = "function onlyCalls() {\n  first();\n  second();\n  new Third();\n}";

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let only_calls = &analysis.functions[0];

        assert_eq!(only_calls.calls.len(), 3);
        assert_eq!(only_calls.metrics.physical_loc, 5);
        assert_eq!(only_calls.metrics.source_loc, 5);
        assert_eq!(only_calls.metrics.cyclomatic_complexity, 1);
        assert_eq!(only_calls.metrics.cognitive_complexity, 0);
        assert_eq!(only_calls.body_hash, super::super::body_hash(body));
        assert_eq!(
            only_calls.body_hash,
            super::super::body_hash("function onlyCalls() { first(); second(); new Third(); }")
        );
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
    fn collects_every_form_of_export() {
        let source = br#"
export function named() { return 1; }
export const value = 1;
export class Widget {}
export interface Shape {}
const internal = 2;
export { internal as renamed };
export default function () { return 3; }
export * from "./other";
function hidden() { return 4; }
"#;

        let analysis = analyze_source(Path::new("sample.ts"), source).expect("analysis succeeds");
        let exports = analysis.exports;

        for expected in [
            "named", "value", "Widget", "Shape", "renamed", "default", "*",
        ] {
            assert!(
                exports.contains(expected),
                "missing {expected} in {exports:?}"
            );
        }
        // An unexported function is not part of the surface, and neither is the
        // local name behind a renamed export.
        assert!(!exports.contains("hidden"));
        assert!(!exports.contains("internal"));
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

    #[test]
    fn reports_each_specifier_with_the_line_its_statement_sits_on() {
        let source = b"import { a } from './first';\n\n// a comment\nexport * from './second';\nimport { c } from './first';\n";
        let mut analyzer =
            super::TypeScriptAnalyzer::new(Language::TypeScript).expect("grammar loads");

        let facts = analyzer.scan_imports(source).expect("scan succeeds");

        // Sorted by specifier, and a module imported twice keeps the earliest
        // line, because that is the site a reader checks first.
        assert_eq!(
            facts.specifiers.into_iter().collect::<Vec<_>>(),
            vec![("./first".to_owned(), 1), ("./second".to_owned(), 4)]
        );
    }

    #[test]
    fn records_the_name_behind_every_import_binding() {
        let source = br#"import defaultThing, { a as b, c } from "./x";
import * as ns from "./ns";
import "./side-effect";
import { type U } from "./mixed";
export { forwarded } from "./x";
"#;
        let mut analyzer =
            super::TypeScriptAnalyzer::new(Language::TypeScript).expect("grammar loads");

        let bindings = analyzer
            .scan_imports(source)
            .expect("scan succeeds")
            .bindings;
        let described = bindings
            .iter()
            .map(|(local, binding)| {
                (
                    local.as_str(),
                    binding.imported.clone(),
                    binding.specifier.as_str(),
                    binding.line,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            described,
            vec![
                ("U", ImportedName::Named("U".to_owned()), "./mixed", 4),
                ("b", ImportedName::Named("a".to_owned()), "./x", 1),
                ("c", ImportedName::Named("c".to_owned()), "./x", 1),
                ("defaultThing", ImportedName::Default, "./x", 1),
                ("ns", ImportedName::Namespace, "./ns", 2),
            ]
        );
    }

    #[test]
    fn records_what_stands_behind_each_exported_name() {
        let source = br#"export function named() {}
export const arrow = () => {};
export { local as pub };
export { a as bee } from "./x";
export * from "./star";
export * as nsx from "./starns";
export default function main() {}
function local() {}
"#;

        let analysis = analyze_source(Path::new("surface.ts"), source).expect("analysis succeeds");
        let described = analysis
            .exported_symbols
            .iter()
            .map(|(exported, symbol)| {
                (
                    exported.as_str(),
                    symbol.local.as_str(),
                    symbol.from.as_deref(),
                    symbol.function.is_some(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            described,
            vec![
                ("arrow", "arrow", None, true),
                ("bee", "a", Some("./x"), false),
                ("default", "main", None, true),
                ("named", "named", None, true),
                // `export * as nsx` publishes a module, and `*` is a name no
                // module exports, so a lookup through it stops.
                ("nsx", "*", Some("./starns"), false),
                ("pub", "local", None, true),
            ]
        );
        assert_eq!(
            analysis.re_exported_modules.iter().collect::<Vec<_>>(),
            vec!["./star", "./starns", "./x"]
        );
    }

    #[test]
    fn leaves_the_export_surface_unchanged_by_the_symbol_record() {
        let source = br#"export function named() {}
export * from "./star";
export default 42;
"#;

        let analysis = analyze_source(Path::new("surface.ts"), source).expect("analysis succeeds");

        assert_eq!(
            analysis.exports.iter().collect::<Vec<_>>(),
            vec!["*", "default", "named"]
        );
        // A default export with no name behind it still publishes `default`,
        // and points at nothing this file declares.
        let default = &analysis.exported_symbols["default"];
        assert_eq!(default.local, "default");
        assert_eq!(default.function, None);
    }

    #[test]
    fn keeps_the_bindings_a_truncated_prefix_did_see() {
        use super::super::{ImportScanner, MAX_IMPORT_SCAN_BYTES};

        let mut source = b"import { early } from \"./early\";\n".to_vec();
        source.resize(MAX_IMPORT_SCAN_BYTES, b'\n');
        source.extend_from_slice(b"import { late } from \"./late\";\n");

        let scan = ImportScanner::new()
            .expect("grammar loads")
            .scan(Path::new("long.ts"), &source)
            .expect("scan succeeds");

        assert!(scan.truncated);
        assert_eq!(scan.bindings.keys().collect::<Vec<_>>(), vec!["early"]);
    }
}
