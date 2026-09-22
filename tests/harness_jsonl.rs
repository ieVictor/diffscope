use std::{
    fs,
    io::{Cursor, Write},
    process::{Command, Stdio},
};

use diffscope::harness::jsonl;
use serde_json::{Value, json};

/// Version the adapter reports for the tool that produced an analysis.
const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn cli_and_jsonl_adapter_return_equivalent_results() {
    let repo = TestRepo::with_base();
    repo.write("example.ts", "function changed() { return 1; }\n");
    repo.commit("add source");

    let cli_output = Command::new(env!("CARGO_BIN_EXE_diffscope"))
        .args(["--repository", repo.path_str(), "--format", "json"])
        .args(["HEAD~1", "HEAD"])
        .output()
        .expect("run CLI");
    assert!(cli_output.status.success());
    let cli_result: Value = serde_json::from_slice(&cli_output.stdout).expect("parse CLI result");

    // A request without a method asks for the complete analysis, which is what
    // the CLI emitted above.
    let responses = serve_process(&[request(&repo, "contract-test")]);
    let response = &responses[0];

    assert_eq!(response["protocol_version"], json!(2));
    assert_eq!(response["id"], "contract-test");
    assert!(response.get("error").is_none());
    assert_eq!(result(response)["data"], cli_result);
    assert_eq!(result(response).get("page"), None);
    assert_eq!(result(response)["query"], json!({}));

    let metadata = &result(response)["analysis"];
    assert_eq!(metadata["schema_version"], json!(2));
    assert_eq!(metadata["tool_version"], TOOL_VERSION);
    assert_eq!(metadata["base"]["id"], repo.commit_id("HEAD~1"));
    assert_eq!(metadata["target"]["id"], repo.commit_id("HEAD"));
    assert_eq!(metadata["base"]["display_name"], "HEAD~1");
    assert_eq!(metadata["target"]["display_name"], "HEAD");
}

#[test]
fn successful_answers_share_one_v2_envelope() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(&repo, "summary", "get_change_summary", &json!({})),
        query(&repo, "functions", "list_changed_functions", &json!({})),
        query(&repo, "graph", "get_impact_graph", &json!({})),
    ]);

    let summary = result(&responses[0]);
    assert_eq!(object_keys(summary), ["analysis", "data", "query"]);
    assert!(summary.get("page").is_none());

    let graph = result(&responses[2]);
    assert_eq!(object_keys(graph), ["analysis", "data", "query"]);
    assert!(graph.get("page").is_none());
    let rendered = &graph["data"];
    assert_eq!(rendered["root"], Value::Null);
    // A rendering nobody asked for costs nothing and is not carried.
    assert!(rendered.get("dependency_diff").is_none(), "{rendered}");
    assert!(rendered.get("mermaid").is_none(), "{rendered}");

    let list = result(&responses[1]);
    assert_eq!(object_keys(list), ["analysis", "data", "page", "query"]);
    assert_eq!(
        object_keys(&list["page"]),
        ["has_more", "next_cursor", "returned", "total"]
    );

    for response in &responses {
        assert_eq!(response["protocol_version"], json!(2));
        assert!(response.get("error").is_none());
        assert!(response["id"].is_string());
        let metadata = &response["result"]["analysis"];
        assert_eq!(
            object_keys(metadata),
            ["base", "id", "schema_version", "target", "tool_version"]
        );
        assert_eq!(metadata["schema_version"], json!(2));
        assert_eq!(metadata["tool_version"], TOOL_VERSION);
        assert_eq!(object_keys(&metadata["base"]), ["display_name", "id"]);
        assert_eq!(object_keys(&metadata["target"]), ["display_name", "id"]);
    }
}

#[test]
fn malformed_and_unsupported_requests_return_errors_without_stopping_stream() {
    let repo = sample_repo();
    let mut input = String::from("not JSON\n");
    for (version, id) in [(9, "nine"), (1, "one")] {
        input.push_str(
            &json!({
                "protocol_version": version,
                "id": id,
                "repository": repo.path_str(),
                "base": "HEAD~1",
                "target": "HEAD"
            })
            .to_string(),
        );
        input.push('\n');
    }
    input.push_str(&query(&repo, "after", "get_change_summary", &json!({})).to_string());
    input.push('\n');

    let mut output = Vec::new();
    jsonl::serve(Cursor::new(input), &mut output).expect("adapter serves requests");
    let responses = responses(&output);

    assert_eq!(responses.len(), 4);
    assert_eq!(error_code(&responses[0]), "malformed_request");
    assert_eq!(error_code(&responses[1]), "unsupported_protocol_version");
    // The cutover is a cutover: version 1 is not served beside version 2.
    assert_eq!(error_code(&responses[2]), "unsupported_protocol_version");
    for response in &responses[..3] {
        assert_eq!(response["protocol_version"], json!(2));
        assert!(response.get("result").is_none());
    }
    assert_eq!(responses[1]["id"], "nine");
    assert_eq!(responses[2]["id"], "one");

    // Errors never terminate the stream; the next valid request is answered.
    assert_eq!(responses[3]["protocol_version"], json!(2));
    assert!(responses[3]["result"].is_object());
}

#[test]
fn queries_return_only_what_was_asked_for() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(&repo, "summary", "get_change_summary", &json!({})),
        query(&repo, "files", "list_changed_files", &json!({})),
        query(
            &repo,
            "source-functions",
            "list_changed_functions",
            &json!({ "classification": "source" }),
        ),
        query(
            &repo,
            "test-functions",
            "list_changed_functions",
            &json!({ "classification": "test" }),
        ),
    ]);

    // Every answer is a projection of the same analysis.
    for response in &responses {
        assert_eq!(analysis(response), analysis(&responses[0]));
    }

    let summary = data(&responses[0]);
    assert!(summary["files"]["changed"].as_u64().expect("changed") >= 1);
    for candidate in summary["review_candidates"]
        .as_array()
        .expect("review candidates")
    {
        assert!(
            matches!(candidate["risk"].as_str(), Some("low" | "medium" | "high")),
            "{candidate}"
        );
        assert!(
            matches!(
                candidate["review_priority"].as_str(),
                Some("low" | "medium" | "high")
            ),
            "{candidate}"
        );
        // The overview ranks candidates; it never carries their full detail.
        assert!(candidate.get("hunks").is_none());
    }

    let files = data(&responses[1]);
    assert!(
        files["files"]
            .as_array()
            .is_some_and(|files| !files.is_empty()),
        "{files}"
    );

    let source = function_rows(&responses[2]);
    assert!(!source.is_empty());
    for row in source {
        assert_eq!(row["classification"], "source");
        assert_ne!(row["status"], "unchanged");
    }

    let tests = function_rows(&responses[3]);
    assert!(
        tests.iter().any(|row| row["classification"] == "test"),
        "the test-classified change must be reachable through its classification"
    );
    for row in tests {
        assert_eq!(row["classification"], "test");
    }
}

#[test]
fn unchanged_functions_are_withheld_until_they_are_requested() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(
            &repo,
            "default",
            "list_changed_functions",
            &json!({ "file": "src/app.ts", "limit": 200 }),
        ),
        query(
            &repo,
            "including",
            "list_changed_functions",
            &json!({ "file": "src/app.ts", "limit": 200, "include_unchanged": true }),
        ),
    ]);

    let changed = function_rows(&responses[0]);
    assert!(changed.iter().all(|row| row["status"] != "unchanged"));
    assert!(
        changed
            .iter()
            .any(|row| row["qualified_name"] == "run" && row["symbol"].is_string())
    );
    assert!(!changed.iter().any(|row| row["qualified_name"] == "helper"));

    let all = function_rows(&responses[1]);
    assert!(
        all.iter()
            .any(|row| row["qualified_name"] == "helper" && row["status"] == "unchanged"),
        "an unchanged function must be retrievable when it is asked for"
    );
    assert!(page_of(&responses[0])["total"].as_u64().expect("total") < all.len() as u64);
}

#[test]
fn detail_queries_resolve_function_ids() {
    let repo = sample_repo();

    let listed = serve_all(&[query(
        &repo,
        "functions",
        "list_changed_functions",
        &json!({ "file": "src/app.ts", "limit": 200 }),
    )]);
    let rows = function_rows(&listed[0]);
    assert_eq!(rows.len(), 1);
    let row = rows[0].clone();
    let id = row["function_id"].as_str().expect("function_id");

    let responses = serve_all(&[
        query(
            &repo,
            "detail",
            "get_function_change",
            &json!({ "function_id": id }),
        ),
        query(
            &repo,
            "old-lookup",
            "get_function_change",
            &json!({ "file": "src/app.ts", "symbol": "run" }),
        ),
        query(
            &repo,
            "absent",
            "get_function_change",
            &json!({ "function_id": "src/app.ts#fn:absent@target:1:1" }),
        ),
    ]);

    let detail = data(&responses[0]);
    // The drill-down returns the very record the list exposed.
    assert_eq!(&detail["function"], &row);
    assert_eq!(query_of(&responses[0]), &json!({ "function_id": id }));
    assert!(detail["hunks"].as_array().is_some(), "{detail}");
    assert!(detail["diagnostics"].is_array(), "{detail}");

    // The file-plus-symbol lookup is gone, not shimmed.
    assert_eq!(error_code(&responses[1]), "malformed_request");
    assert_eq!(error_code(&responses[2]), "unknown_function");
}

#[test]
fn duplicate_names_are_addressable_by_function_id() {
    let repo = sample_repo();

    let listed = serve_all(&[query(
        &repo,
        "functions",
        "list_changed_functions",
        &json!({ "file": "src/dupes.ts", "limit": 200 }),
    )]);
    let rows = function_rows(&listed[0]);
    assert_eq!(rows.len(), 2, "{listed:?}");
    // Both declarations carry the same human symbol; only the identifiers,
    // which also record where each definition sits, tell them apart.
    assert_eq!(rows[0]["symbol"], rows[1]["symbol"]);
    assert_eq!(rows[0]["qualified_name"], rows[1]["qualified_name"]);
    assert_eq!(rows[0]["status"], "added");
    assert_eq!(rows[1]["status"], "added");
    assert_ne!(rows[0]["function_id"], rows[1]["function_id"]);

    let first = rows[0]["function_id"].as_str().expect("function_id");
    let second = rows[1]["function_id"].as_str().expect("function_id");
    let responses = serve_all(&[
        query(
            &repo,
            "first",
            "get_function_change",
            &json!({ "function_id": first }),
        ),
        query(
            &repo,
            "second",
            "get_function_change",
            &json!({ "function_id": second }),
        ),
    ]);

    assert_eq!(&data(&responses[0])["function"], &rows[0]);
    assert_eq!(&data(&responses[1])["function"], &rows[1]);
    assert_ne!(data(&responses[0]), data(&responses[1]));
    // Each identifier resolves to its own definition site.
    assert_ne!(
        data(&responses[0])["function"]["range"],
        data(&responses[1])["function"]["range"]
    );
}

#[test]
fn rejects_unknown_methods_and_parameters_without_stopping_the_stream() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(&repo, "method", "no_such_method", &json!({})),
        query(
            &repo,
            "value",
            "list_changed_functions",
            &json!({ "minimum_risk": "extreme" }),
        ),
        // Paging moved to cursors; the offset parameter is not accepted at all.
        query(
            &repo,
            "offset",
            "list_changed_functions",
            &json!({ "offset": 1 }),
        ),
        query(&repo, "after", "get_change_summary", &json!({})),
    ]);

    assert_eq!(error_code(&responses[0]), "unknown_method");
    assert_eq!(error_code(&responses[1]), "invalid_params");
    assert_eq!(error_code(&responses[2]), "malformed_request");
    // The stream survives all of them, and the next request is still answered.
    assert!(responses[3]["result"].is_object());
}

/// Every way an impact-graph request can be rejected, each naming the field it
/// is about and what that field accepts — and never taking the stream down.
#[test]
fn impact_graph_rejections_name_the_field_and_what_it_accepts() {
    let repo = sample_repo();

    // A real identity, so the exclusivity rejection is not reached through an
    // unknown one.
    let listed = serve_all(&[query(&repo, "listed", "list_changed_functions", &json!({}))]);
    let ids = function_ids(&listed[0]);
    let real = ids
        .iter()
        .find(|id| id.starts_with("src/util.ts#fn:score"))
        .expect("the modified function is listed");

    let responses = serve_all(&[
        query(
            &repo,
            "relation",
            "get_impact_graph",
            &json!({ "relations": ["extends"] }),
        ),
        query(
            &repo,
            "function-root",
            "get_impact_graph",
            &json!({ "function_id": "src/util.ts#fn:score" }),
        ),
        query(
            &repo,
            "cursor",
            "get_impact_graph",
            &json!({ "cursor": "not-a-cursor" }),
        ),
        query(
            &repo,
            "both-roots",
            "get_impact_graph",
            &json!({ "file": "src/util.ts", "function_id": real }),
        ),
        query(
            &repo,
            "unchanged-file",
            "get_impact_graph",
            &json!({ "file": "src/absent.ts" }),
        ),
        query(&repo, "after", "get_impact_graph", &json!({})),
    ]);

    for response in &responses[..5] {
        assert_eq!(error_code(response), "invalid_params", "{response}");
    }

    let relation = error_message(&responses[0]);
    assert!(relation.contains("`relations`"), "{relation}");
    assert!(
        relation.contains("imports, tested_by, calls, contains"),
        "{relation}"
    );
    assert!(relation.contains("extends"), "{relation}");

    // An identity the analysis does not contain is answered with the identities
    // it does: the caller can correct itself in one step.
    let function_root = error_message(&responses[1]);
    assert!(function_root.contains("`function_id`"), "{function_root}");
    assert!(function_root.contains(real.as_str()), "{function_root}");

    // A cursor belongs to the paged methods; this answer is bounded by its
    // budgets instead.
    let cursor = error_message(&responses[2]);
    assert!(cursor.contains("`cursor`"), "{cursor}");
    assert!(cursor.contains("list_changed_files"), "{cursor}");

    let both_roots = error_message(&responses[3]);
    assert!(both_roots.contains("`file`"), "{both_roots}");
    assert!(both_roots.contains("`function_id`"), "{both_roots}");
    assert!(both_roots.contains("mutually exclusive"), "{both_roots}");

    // A path the comparison did not change is answered with the paths it did:
    // the caller can correct itself in one step.
    let unknown = error_message(&responses[4]);
    assert!(unknown.contains("`file`"), "{unknown}");
    assert!(unknown.contains("src/absent.ts"), "{unknown}");
    assert!(unknown.contains("src/util.ts"), "{unknown}");

    // The stream survives every rejection, and the next request is answered.
    assert!(responses[5]["result"].is_object());
}

#[test]
fn analyses_share_one_metadata_block_and_identify_their_inputs() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(&repo, "summary", "get_change_summary", &json!({})),
        query(&repo, "files", "list_changed_files", &json!({})),
        query(
            &repo,
            "functions",
            "list_changed_functions",
            &json!({ "include_unchanged": true }),
        ),
        query(&repo, "diagnostics", "get_analysis_diagnostics", &json!({})),
        query(&repo, "analyze", "analyze", &json!({})),
    ]);

    let metadata = analysis(&responses[0]).clone();
    for response in &responses {
        assert_eq!(analysis(response), &metadata);
    }

    let base_commit = repo.commit_id("HEAD~1");
    let target_commit = repo.commit_id("HEAD");
    assert_eq!(metadata["base"]["id"], base_commit);
    assert_eq!(metadata["target"]["id"], target_commit);

    let id = metadata["id"].as_str().expect("analysis id");
    assert!(!id.is_empty());
    assert!(
        !id.contains(repo.path_str()),
        "analysis id leaks the repository path: {id}"
    );
    assert!(
        !id.contains("HEAD"),
        "analysis id leaks a revision name: {id}"
    );
    assert_ne!(id, base_commit.as_str());

    // A second session and a separate process derive the same identifier.
    let again = serve_all(&[query(&repo, "again", "get_change_summary", &json!({}))]);
    assert_eq!(analysis(&again[0])["id"], metadata["id"]);
    let process = serve_process(&[query(&repo, "process", "get_change_summary", &json!({}))]);
    assert_eq!(analysis(&process[0])["id"], metadata["id"]);

    // The same commits reached through different names are one analysis...
    let mut by_commit = query(&repo, "by-commit", "get_change_summary", &json!({}));
    by_commit["base"] = json!(base_commit);
    by_commit["target"] = json!(target_commit);
    let resolved = serve_all(&[by_commit]);
    assert_eq!(analysis(&resolved[0])["id"], metadata["id"]);

    // ...and a different comparison is a different analysis.
    let mut other = query(&repo, "empty", "get_change_summary", &json!({}));
    other["base"] = json!("HEAD");
    other["target"] = json!("HEAD");
    let empty = serve_all(&[other]);
    assert_ne!(analysis(&empty[0])["id"], metadata["id"]);
}

#[test]
fn canonical_query_echoes_applied_parameters_and_defaults() {
    let repo = sample_repo();

    let responses = serve_all(&[
        query(&repo, "analyze", "analyze", &json!({})),
        query(&repo, "summary", "get_change_summary", &json!({})),
        query(&repo, "files", "list_changed_files", &json!({})),
        query(&repo, "functions", "list_changed_functions", &json!({})),
        query(
            &repo,
            "filtered",
            "list_changed_functions",
            &json!({ "classification": "source", "include_unchanged": true, "limit": 1 }),
        ),
        query(&repo, "diagnostics", "get_analysis_diagnostics", &json!({})),
        query(
            &repo,
            "file-diagnostics",
            "get_analysis_diagnostics",
            &json!({ "file": "src/app.ts" }),
        ),
        query(&repo, "graph", "get_impact_graph", &json!({})),
        query(
            &repo,
            "graph-bounded",
            "get_impact_graph",
            &json!({
                "file": "src/util.ts",
                "direction": "upstream",
                "relations": ["tested_by"],
                "depth": 9,
                "view": "target",
                "max_nodes": 1,
                "max_edges": 999,
                "render": ["mermaid"]
            }),
        ),
    ]);

    assert_eq!(query_of(&responses[0]), &json!({}));
    assert_eq!(query_of(&responses[1]), &json!({}));
    assert_eq!(
        query_of(&responses[2]),
        &json!({ "classification": null, "minimum_risk": null, "limit": 50 })
    );
    assert_eq!(
        query_of(&responses[3]),
        &json!({
            "file": null,
            "status": null,
            "classification": null,
            "minimum_risk": null,
            "min_complexity_delta": null,
            "include_unchanged": false,
            "limit": 50
        })
    );
    assert_eq!(
        query_of(&responses[4]),
        &json!({
            "file": null,
            "status": null,
            "classification": "source",
            "minimum_risk": null,
            "min_complexity_delta": null,
            "include_unchanged": true,
            "limit": 1
        })
    );
    assert_eq!(query_of(&responses[5]), &json!({ "file": null }));
    assert_eq!(query_of(&responses[6]), &json!({ "file": "src/app.ts" }));
    // Every parameter the method understands, with the defaults that were
    // applied: the budgets it used, and the relations this version resolves.
    assert_eq!(
        query_of(&responses[7]),
        &json!({
            "file": null,
            "function_id": null,
            "direction": "both",
            "relations": ["imports", "tested_by", "calls", "contains", "re_exports"],
            "depth": 1,
            "view": "delta",
            "max_nodes": 30,
            "max_edges": 60,
            "render": []
        })
    );
    // Out-of-range values are clamped rather than rejected, and the echo
    // reports the clamped ones.
    assert_eq!(
        query_of(&responses[8]),
        &json!({
            "file": "src/util.ts",
            "function_id": null,
            "direction": "upstream",
            "relations": ["tested_by"],
            "depth": 3,
            "view": "target",
            "max_nodes": 3,
            "max_edges": 200,
            "render": ["mermaid"]
        })
    );
}

#[test]
fn cursor_continuation_walks_the_ranking_exactly_once() {
    let repo = sample_repo();

    let complete = serve_all(&[query(
        &repo,
        "all",
        "list_changed_functions",
        &json!({ "limit": 200, "include_unchanged": true }),
    )]);
    let complete = function_ids(&complete[0]);
    assert!(
        complete.len() > 2,
        "the fixture must rank more than one page of functions"
    );

    let bound = json!({
        "file": null,
        "status": null,
        "classification": null,
        "minimum_risk": null,
        "min_complexity_delta": null,
        "include_unchanged": true,
        "limit": 2
    });
    let mut cursor: Option<String> = None;
    let mut collected: Vec<String> = Vec::new();
    loop {
        let params = match &cursor {
            None => json!({ "limit": 2, "include_unchanged": true }),
            Some(cursor) => json!({ "cursor": cursor, "limit": 2, "include_unchanged": true }),
        };
        let batch = serve_all(&[query(&repo, "page", "list_changed_functions", &params)]);
        let response = &batch[0];
        let page = page_of(response);
        let ids = function_ids(response);

        assert_eq!(page["returned"], json!(ids.len()));
        for id in &ids {
            assert!(!collected.contains(id), "cursor paging repeated {id}");
            collected.push(id.clone());
        }
        // A continuation echoes the query the cursor bound, not the shorthand
        // the caller repeated.
        assert_eq!(query_of(response), &bound);

        match page["next_cursor"].as_str() {
            None => {
                assert_eq!(page["has_more"], json!(false));
                break;
            }
            Some(next) => {
                assert_eq!(page["has_more"], json!(true));
                cursor = Some(next.to_owned());
            }
        }
        assert!(collected.len() < complete.len());
    }

    assert_eq!(collected, complete);
}

#[test]
fn cursors_bind_one_method_analysis_and_query() {
    let repo = sample_repo();

    let first = serve_all(&[query(
        &repo,
        "first",
        "list_changed_functions",
        &json!({ "limit": 1, "include_unchanged": true }),
    )]);
    let cursor = page_of(&first[0])["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_owned();

    let mut other_analysis = query(
        &repo,
        "other-analysis",
        "list_changed_functions",
        &json!({ "cursor": cursor }),
    );
    other_analysis["base"] = json!("HEAD");
    other_analysis["target"] = json!("HEAD");

    let responses = serve_all(&[
        // The cursor alone carries the query it was cut from.
        query(
            &repo,
            "alone",
            "list_changed_functions",
            &json!({ "cursor": cursor }),
        ),
        query(
            &repo,
            "repeated",
            "list_changed_functions",
            &json!({ "cursor": cursor, "limit": 1, "include_unchanged": true }),
        ),
        query(
            &repo,
            "other-filter",
            "list_changed_functions",
            &json!({ "cursor": cursor, "limit": 1, "include_unchanged": true, "status": "added" }),
        ),
        query(
            &repo,
            "other-method",
            "list_changed_files",
            &json!({ "cursor": cursor, "limit": 1 }),
        ),
        query(
            &repo,
            "other-limit",
            "list_changed_functions",
            &json!({ "cursor": cursor, "limit": 2, "include_unchanged": true }),
        ),
        query(
            &repo,
            "garbage",
            "list_changed_functions",
            &json!({ "cursor": "not-a-cursor" }),
        ),
        other_analysis,
        query(&repo, "after", "get_change_summary", &json!({})),
    ]);

    assert_eq!(data(&responses[0]), data(&responses[1]));
    assert_eq!(error_code(&responses[2]), "invalid_params");
    assert_eq!(error_code(&responses[3]), "invalid_params");
    assert_eq!(error_code(&responses[4]), "invalid_params");
    assert_eq!(error_code(&responses[5]), "invalid_params");
    assert_eq!(error_code(&responses[6]), "invalid_params");
    // A cursor from another session is still valid here, but a mismatched
    // request is not; either way the stream keeps going.
    assert!(responses[7]["result"].is_object());
}

#[test]
fn diagnostic_counts_describe_the_returned_diagnostics() {
    let repo = diagnostics_repo();

    let responses = serve_all(&[
        query(&repo, "all", "get_analysis_diagnostics", &json!({})),
        query(
            &repo,
            "one-file",
            "get_analysis_diagnostics",
            &json!({ "file": "src/broken.ts" }),
        ),
    ]);

    for response in &responses {
        let counts = &data(response)["counts"];
        let mut keys = counts
            .as_object()
            .expect("counts object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(keys, ["errors", "info", "total", "warnings"]);

        let diagnostics = data(response)["diagnostics"]
            .as_array()
            .expect("diagnostics array");
        let (mut info, mut warnings, mut errors) = (0_u64, 0_u64, 0_u64);
        for diagnostic in diagnostics {
            match diagnostic["severity"].as_str().expect("severity") {
                "info" => info += 1,
                "warning" => warnings += 1,
                "error" => errors += 1,
                other => panic!("unexpected severity `{other}`"),
            }
        }
        assert_eq!(counts["info"], json!(info));
        assert_eq!(counts["warnings"], json!(warnings));
        assert_eq!(counts["errors"], json!(errors));
        assert_eq!(counts["total"], json!(diagnostics.len()));
        assert_eq!(counts["total"], json!(info + warnings + errors));
    }

    let all = &data(&responses[0])["counts"];
    assert!(
        all["info"].as_u64().expect("info") >= 1,
        "the unsupported file must be counted: {all}"
    );
    assert!(
        all["warnings"].as_u64().expect("warnings") >= 1,
        "the malformed source must be counted: {all}"
    );
    assert!(
        all["errors"].as_u64().expect("errors") >= 1,
        "the undecodable source must be counted: {all}"
    );

    // A filtered answer counts only what it returned.
    let one_file = &data(&responses[1])["counts"];
    assert_eq!(one_file["info"], json!(0));
    assert_eq!(one_file["errors"], json!(0));
    assert!(one_file["warnings"].as_u64().expect("warnings") >= 1);
}

#[test]
fn risk_and_review_priority_score_different_signals() {
    let repo = sample_repo();

    let responses = serve_all(&[query(
        &repo,
        "source",
        "list_changed_functions",
        &json!({ "classification": "source", "limit": 200 }),
    )]);
    let rows = function_rows(&responses[0]);
    assert!(!rows.is_empty());

    for row in rows {
        let risk = score_object(&row["risk"], "diffscope-risk-v1", 8);
        let priority = score_object(&row["review_priority"], "diffscope-review-priority-v1", 14);

        for reason in risk["reasons"].as_array().expect("risk reasons") {
            let code = reason["code"].as_str().expect("reason code");
            assert!(
                matches!(
                    code,
                    "cognitive_complexity_increased"
                        | "cyclomatic_complexity_increased"
                        | "cognitive_complexity_after"
                        | "high_churn"
                        | "added_function_cognitive_complexity"
                        | "added_function_cyclomatic_complexity"
                        | "match_confidence"
                ),
                "intrinsic risk must not score module impact or source: {reason}"
            );
        }

        // Review priority starts from intrinsic risk and adds to it.
        assert!(
            priority["score"].as_u64().expect("score") >= risk["score"].as_u64().expect("score")
        );
        let priority_codes = reason_codes(priority);
        for code in reason_codes(risk) {
            assert!(
                priority_codes.contains(&code),
                "review priority must carry every intrinsic reason, missing {code}"
            );
        }
    }

    // The changed function whose containing module five others import picks up
    // both the blast radius and the source classification; the intrinsic score
    // stays free of them.
    let scored = rows
        .iter()
        .find(|row| row["qualified_name"] == "score")
        .expect("the changed score row");
    let risk = score_object(&scored["risk"], "diffscope-risk-v1", 8);
    let priority = score_object(
        &scored["review_priority"],
        "diffscope-review-priority-v1",
        14,
    );

    let importers = priority["reasons"]
        .as_array()
        .expect("review reasons")
        .iter()
        .find(|reason| reason["code"] == "containing_module_direct_importers")
        .expect("the blast-radius reason");
    assert!(
        importers["message"]
            .as_str()
            .expect("message")
            .contains("containing module"),
        "{importers}"
    );
    assert!(importers["value"].as_i64().expect("value") >= 5);
    assert!(
        priority["reasons"]
            .as_array()
            .expect("review reasons")
            .iter()
            .any(|reason| reason["code"] == "production_source"),
        "{priority}"
    );
    assert!(!reason_codes(risk).contains(&"containing_module_direct_importers"));
    assert_eq!(
        priority["score"].as_u64().expect("score"),
        risk["score"].as_u64().expect("score") + 2
    );
}

#[test]
fn added_functions_report_absolute_complexity() {
    let repo = sample_repo();

    let responses = serve_all(&[query(
        &repo,
        "added",
        "list_changed_functions",
        &json!({ "file": "src/added.ts", "limit": 200 }),
    )]);
    let rows = function_rows(&responses[0]);
    assert_eq!(rows.len(), 1, "{responses:?}");
    let row = &rows[0];
    assert_eq!(row["status"], "added");

    for (key, model, maximum) in [
        ("risk", "diffscope-risk-v1", 8),
        ("review_priority", "diffscope-review-priority-v1", 14),
    ] {
        let assessment = score_object(&row[key], model, maximum);
        let reasons = assessment["reasons"].as_array().expect("reasons");
        for reason in reasons {
            let code = reason["code"].as_str().expect("code").to_lowercase();
            let message = reason["message"].as_str().expect("message").to_lowercase();
            assert!(
                !code.contains("increase"),
                "an added function has no before to increase from: {reason}"
            );
            assert!(
                !message.contains("increase"),
                "an added function has no before to increase from: {reason}"
            );
        }
        let complexity = reasons
            .iter()
            .find(|reason| reason["code"] == "added_function_cognitive_complexity")
            .unwrap_or_else(|| panic!("expected absolute complexity in {assessment}"));
        assert!(
            complexity["value"].as_i64().expect("value") >= 10,
            "{complexity}"
        );
        assert!(
            complexity["message"]
                .as_str()
                .expect("message")
                .starts_with("new function has cognitive complexity"),
            "{complexity}"
        );
    }
}

#[test]
fn extraction_is_distinguished_from_other_changes() {
    let repo = extraction_repo();

    let responses = serve_all(&[query(
        &repo,
        "files",
        "list_changed_files",
        &json!({ "limit": 200 }),
    )]);
    let rows = data(&responses[0])["files"]
        .as_array()
        .expect("files array");

    for row in rows {
        let shape = row["change_shape"].as_str().expect("change_shape");
        assert!(
            matches!(shape, "complexity_extraction" | "other"),
            "unknown change shape `{shape}`"
        );
    }

    let extraction = rows
        .iter()
        .find(|row| row["path"] == "src/refactor.ts")
        .expect("the refactored file");
    assert_eq!(
        extraction["change_shape"], "complexity_extraction",
        "{extraction}"
    );

    let plain = rows
        .iter()
        .find(|row| row["path"] == "src/plain.ts")
        .expect("the plainly edited file");
    assert_eq!(plain["change_shape"], "other", "{plain}");
}

#[test]
fn a_function_root_answers_with_calls_and_containment() {
    let repo = extraction_repo();

    let listed = serve_all(&[query(
        &repo,
        "listed",
        "list_changed_functions",
        &json!({ "file": "src/refactor.ts" }),
    )]);
    let rows = function_rows(&listed[0]);
    let process = rows
        .iter()
        .find(|row| row["qualified_name"] == json!("process"))
        .expect("the changed function is listed");
    let helper = rows
        .iter()
        .find(|row| row["qualified_name"] == json!("totalFor"))
        .expect("the extracted helper is listed");
    assert_eq!(process["status"], json!("modified"), "{process}");
    assert_eq!(helper["status"], json!("added"), "{helper}");
    let process_id = process["function_id"].as_str().expect("function_id");
    let helper_id = helper["function_id"].as_str().expect("function_id");

    let responses = serve_all(&[query(
        &repo,
        "rooted",
        "get_impact_graph",
        &json!({ "function_id": process_id, "render": ["diff"] }),
    )]);
    let answer = data(&responses[0]);

    // The echo reports the function root the request named, and the defaults
    // this version resolves.
    let applied = query_of(&responses[0]);
    assert_eq!(applied["file"], Value::Null);
    assert_eq!(applied["function_id"], json!(process_id));
    assert_eq!(
        applied["relations"],
        json!(["imports", "tested_by", "calls", "contains", "re_exports"])
    );

    assert_eq!(answer["root"]["kind"], json!("function"));
    assert_eq!(
        answer["root"]["id"],
        json!(format!("function:{process_id}"))
    );
    assert_eq!(answer["root"]["path"], json!("src/refactor.ts"));

    // The function it calls, and the module that declares both, are one hop
    // from the root.
    let node_ids = answer["graph"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|node| node["id"].as_str())
        .collect::<Vec<_>>();
    for id in [
        format!("function:{process_id}"),
        format!("function:{helper_id}"),
        "module:src/refactor.ts".to_owned(),
    ] {
        assert!(node_ids.contains(&id.as_str()), "{answer}");
    }

    let edges = answer["graph"]["edges"].as_array().expect("edges");
    let call = edges
        .iter()
        .find(|edge| edge["relation"] == json!("calls"))
        .unwrap_or_else(|| panic!("the extracted call is an edge: {answer}"));
    assert_eq!(call["from"], json!(format!("function:{process_id}")));
    assert_eq!(call["to"], json!(format!("function:{helper_id}")));
    assert_eq!(call["status"], json!("added"));
    assert_eq!(call["resolution"], json!("direct_local_symbol"));
    assert_eq!(call["confidence"], json!(1.0));
    assert_eq!(call["evidence"]["file"], json!("src/refactor.ts"));
    assert_eq!(
        call["evidence"]["line"],
        json!(19),
        "the call site is the target definition's call expression: {call}"
    );

    let contains = edges
        .iter()
        .find(|edge| {
            edge["relation"] == json!("contains")
                && edge["to"] == json!(format!("function:{process_id}"))
        })
        .unwrap_or_else(|| panic!("the containing module is an edge: {answer}"));
    assert_eq!(contains["from"], json!("module:src/refactor.ts"));
    assert_eq!(contains["resolution"], json!("declaration"));
    assert_eq!(contains["confidence"], json!(1.0));

    // Both function endpoints are named in the dependency diff, so two
    // functions of one file are two lines rather than one path.
    let diff = answer["dependency_diff"]
        .as_str()
        .expect("a dependency diff");
    assert!(
        diff.contains("+ src/refactor.ts::process -[calls]-> src/refactor.ts::totalFor"),
        "a calls edge names both functions: {diff}"
    );
}

#[test]
fn repeated_queries_are_byte_identical() {
    // Determinism is the property every other guarantee rests on: parallel file
    // analysis, throttling, hash-based pairing of ambiguous identities, and the
    // import graph all introduce ordering that must not reach the output.
    let repo = sample_repo();
    let requests = [
        query(&repo, "summary", "get_change_summary", &json!({})),
        query(&repo, "files", "list_changed_files", &json!({ "limit": 1 })),
        query(
            &repo,
            "functions",
            "list_changed_functions",
            &json!({ "limit": 1 }),
        ),
        query(&repo, "diagnostics", "get_analysis_diagnostics", &json!({})),
        query(&repo, "analysis", "analyze", &json!({})),
        // Both renderings of one graph: the Mermaid document and the dependency
        // diff are output, so they are held to the same byte-identity rule.
        query(
            &repo,
            "graph",
            "get_impact_graph",
            &json!({ "file": "src/util.ts", "render": ["diff", "mermaid"] }),
        ),
    ];
    let input = requests
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    let first = serve_bytes(&input);
    for _ in 0..3 {
        assert_eq!(serve_bytes(&input), first);
    }

    // The graph answer carries both renderings it was asked for, so the
    // comparison above covered rendered output and not only structured data.
    let answered = responses(&first);
    let rendered = data(&answered[5]);
    assert!(
        rendered["mermaid"]
            .as_str()
            .is_some_and(|text| text.starts_with("flowchart LR")),
        "{rendered}"
    );
    assert!(rendered["dependency_diff"].is_string(), "{rendered}");

    // A page cut from a cursor reproduces byte-for-byte too, in a fresh session.
    let cursor = page_of(&responses(&first)[1])["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_owned();
    let continuation = query(
        &repo,
        "files-next",
        "list_changed_files",
        &json!({ "cursor": cursor, "limit": 1 }),
    )
    .to_string();
    let page = serve_bytes(&continuation);
    assert_eq!(serve_bytes(&continuation), page);
}

/// A comparison that redirected one import to another module: the same caller
/// now reaches a different file, which is one removed edge and one added one.
#[test]
fn a_redirected_import_reports_the_removal_and_the_addition_together() {
    let repo = redirect_repo();

    let responses = serve_all(&[query(
        &repo,
        "delta",
        "get_impact_graph",
        &json!({ "file": "src/app.ts", "render": ["diff"] }),
    )]);
    let answer = data(&responses[0]);

    let imports = |status: &str| -> Vec<&Value> {
        answer["graph"]["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .filter(|edge| edge["relation"] == "imports" && edge["status"] == status)
            .collect()
    };
    let removed = imports("removed");
    let added = imports("added");
    assert_eq!(removed.len(), 1, "{answer}");
    assert_eq!(added.len(), 1, "{answer}");
    assert_eq!(removed[0]["from"], "module:src/app.ts");
    assert_eq!(removed[0]["to"], "module:src/legacy.ts");
    assert_eq!(removed[0]["resolution"], "resolved_specifier");
    assert_eq!(removed[0]["confidence"], json!(1.0));
    assert_eq!(added[0]["from"], "module:src/app.ts");
    assert_eq!(added[0]["to"], "module:src/modern.ts");

    // The diff is ordered by the edge ordering, so the removal and the
    // addition that replaced it sit on adjacent lines.
    let diff = answer["dependency_diff"].as_str().expect("dependency_diff");
    let lines = diff.lines().collect::<Vec<_>>();
    let removed_at = lines
        .iter()
        .position(|line| line.starts_with("- ") && line.contains("legacy"))
        .unwrap_or_else(|| panic!("no removed line in {diff:?}"));
    let added_at = lines
        .iter()
        .position(|line| line.starts_with("+ ") && line.contains("modern"))
        .unwrap_or_else(|| panic!("no added line in {diff:?}"));
    assert_eq!(
        added_at,
        removed_at + 1,
        "the removal and its replacement must sit together: {diff:?}"
    );
}

#[test]
fn a_reused_analysis_answers_identically_to_a_fresh_one() {
    let repo = sample_repo();
    let request = query(&repo, "q", "get_change_summary", &json!({}));

    // Two adapter sessions each analyze once; one session answers twice and
    // serves the second from its cache. All three answers must agree.
    let fresh = serve_all(std::slice::from_ref(&request));
    let reused = serve_all(&[request.clone(), request]);

    assert_eq!(result(&fresh[0]), result(&reused[0]));
    assert_eq!(result(&reused[0]), result(&reused[1]));
}

/// Serve a batch of requests through one adapter session, as a long-lived
/// harness does, and return one parsed response per request.
fn serve_all(requests: &[Value]) -> Vec<Value> {
    let input = requests
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    responses(&serve_bytes(&input))
}

/// Serve a batch of requests through a fresh adapter process.
fn serve_process(requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_diffscope"))
        .arg("--jsonl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start JSONL adapter");
    {
        let stdin = child.stdin.as_mut().expect("adapter stdin");
        for request in requests {
            writeln!(stdin, "{request}").expect("write request");
        }
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for adapter");
    assert!(output.status.success(), "adapter exited with {output:?}");
    responses(&output.stdout)
}

fn serve_bytes(input: &str) -> Vec<u8> {
    let mut output = Vec::new();
    jsonl::serve(Cursor::new(input), &mut output).expect("adapter serves requests");
    output
}

fn responses(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8(bytes.to_vec())
        .expect("adapter output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("response is JSON"))
        .collect()
}

fn request(repo: &TestRepo, id: &str) -> Value {
    json!({
        "protocol_version": 2,
        "id": id,
        "repository": repo.path_str(),
        "base": "HEAD~1",
        "target": "HEAD"
    })
}

fn query(repo: &TestRepo, id: &str, method: &str, params: &Value) -> Value {
    let mut request = request(repo, id);
    request["method"] = Value::from(method);
    request["params"] = params.clone();
    request
}

fn result(response: &Value) -> &Value {
    response
        .get("result")
        .unwrap_or_else(|| panic!("response carries no result: {response}"))
}

fn data(response: &Value) -> &Value {
    result(response)
        .get("data")
        .unwrap_or_else(|| panic!("response carries no data: {response}"))
}

fn query_of(response: &Value) -> &Value {
    result(response)
        .get("query")
        .unwrap_or_else(|| panic!("response carries no query echo: {response}"))
}

fn page_of(response: &Value) -> &Value {
    result(response)
        .get("page")
        .unwrap_or_else(|| panic!("response carries no page: {response}"))
}

fn analysis(response: &Value) -> &Value {
    result(response)
        .get("analysis")
        .unwrap_or_else(|| panic!("response carries no analysis metadata: {response}"))
}

fn error_code(response: &Value) -> &str {
    response["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("response carries no error code: {response}"))
}

fn error_message(response: &Value) -> &str {
    response["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("response carries no error message: {response}"))
}

fn function_rows(response: &Value) -> &[Value] {
    data(response)["functions"]
        .as_array()
        .unwrap_or_else(|| panic!("response carries no functions: {response}"))
}

fn function_ids(response: &Value) -> Vec<String> {
    function_rows(response)
        .iter()
        .map(|row| row["function_id"].as_str().expect("function_id").to_owned())
        .collect()
}

fn reason_codes(assessment: &Value) -> Vec<&str> {
    assessment["reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .map(|reason| reason["code"].as_str().expect("reason code"))
        .collect()
}

/// Assert one score object matches the published model, and return it.
fn score_object<'a>(assessment: &'a Value, model: &str, maximum: u64) -> &'a Value {
    assert_eq!(assessment["model"], model);
    assert_eq!(assessment["maximum_score"], json!(maximum));
    let score = assessment["score"].as_u64().expect("score");
    assert!(score <= maximum, "{assessment}");
    let expected_level = if score >= 5 {
        "high"
    } else if score >= 2 {
        "medium"
    } else {
        "low"
    };
    assert_eq!(assessment["level"], expected_level, "{assessment}");
    for reason in assessment["reasons"].as_array().expect("reasons") {
        assert!(
            reason["code"].as_str().is_some_and(|code| !code.is_empty()),
            "{reason}"
        );
        assert!(
            reason["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "{reason}"
        );
        if let Some(value) = reason.get("value") {
            assert!(
                value.as_i64().is_some(),
                "reason value must be an integer: {reason}"
            );
        }
    }
    assessment
}

fn object_keys(value: &Value) -> Vec<&str> {
    let mut keys = value
        .as_object()
        .unwrap_or_else(|| panic!("expected an object: {value}"))
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

/// A comparison with one of every shape the queries must distinguish: an
/// unchanged function beside a changed one, an exported function whose
/// containing module five others import, a test-classified change, two added
/// functions that share a name, and one more added function that is complex.
fn sample_repo() -> TestRepo {
    const UTIL_BASE: &str = "export function score(value: number) {\n  return value;\n}\n";
    const UTIL_TARGET: &str = "export function score(value: number) {\n  if (value > 0) {\n    return value * 2;\n  }\n  return -value;\n}\n";
    const APP_BASE: &str = "import { score } from './util';\n\nexport function helper(value: number) {\n  return value + 1;\n}\n\nexport function run(flag: boolean) {\n  return score(flag ? 1 : 0);\n}\n";
    const APP_TARGET: &str = "import { score } from './util';\n\nexport function helper(value: number) {\n  return value + 1;\n}\n\nexport function run(flag: boolean) {\n  if (flag) {\n    for (const item of [1, 2]) {\n      score(item);\n    }\n    return 1;\n  }\n  return score(0);\n}\n";
    const SPEC_BASE: &str =
        "import { run } from '../app';\n\nexport function testsRun() {\n  return run(true);\n}\n";
    const SPEC_TARGET: &str =
        "import { run } from '../app';\n\nexport function testsRun() {\n  return run(false);\n}\n";
    const DUPES_BASE: &str = "";
    const DUPES_TARGET: &str = "export function dup(value: number) {\n  return value + 1;\n}\n\nexport function dup(value: number) {\n  return value + 2;\n}\n";
    const ADDED: &str = "export function newlyAdded(values: number[]) {\n  let total = 0;\n  for (const value of values) {\n    if (value > 0) {\n      for (const inner of values) {\n        if (inner > value) {\n          if (inner % 2 === 0) {\n            total += inner;\n          }\n        }\n      }\n    }\n  }\n  return total;\n}\n";

    let repo = TestRepo::with_base();
    repo.write("src/util.ts", UTIL_BASE);
    repo.write("src/app.ts", APP_BASE);
    repo.write("src/__tests__/app.spec.ts", SPEC_BASE);
    repo.write("src/dupes.ts", DUPES_BASE);
    for name in ["a", "b", "c", "d", "e"] {
        repo.write(
            &format!("src/importers/{name}.ts"),
            &format!(
                "import {{ score }} from '../util';\n\nexport function use{}() {{\n  return score(1);\n}}\n",
                name.to_uppercase()
            ),
        );
    }
    repo.commit("add sources");

    repo.write("src/util.ts", UTIL_TARGET);
    repo.write("src/app.ts", APP_TARGET);
    repo.write("src/__tests__/app.spec.ts", SPEC_TARGET);
    repo.write("src/dupes.ts", DUPES_TARGET);
    repo.write("src/added.ts", ADDED);
    repo.commit("change sources");
    repo
}

/// A comparison that redirects one import from a legacy module to its
/// replacement, leaving both targets unchanged in the tree.
fn redirect_repo() -> TestRepo {
    let repo = TestRepo::with_base();
    repo.write(
        "src/app.ts",
        "import { parse } from './legacy';\n\nexport function run() {\n  return parse();\n}\n",
    );
    repo.write(
        "src/legacy.ts",
        "export function parse() {\n  return 1;\n}\n",
    );
    repo.write(
        "src/modern.ts",
        "export function parse() {\n  return 2;\n}\n",
    );
    repo.commit("add sources");

    repo.write(
        "src/app.ts",
        "import { parse } from './modern';\n\nexport function run() {\n  return parse();\n}\n",
    );
    repo.commit("redirect the import");
    repo
}

/// A comparison whose diagnostics span every severity the counts report.
fn diagnostics_repo() -> TestRepo {
    let repo = TestRepo::with_base();
    repo.write("README.md", "# Notes\n");
    repo.write("src/broken.ts", "export function broken( {\n");
    repo.write_bytes(
        "src/undecodable.ts",
        b"export function undecodable() {\n  return \xff\xfe;\n}\n",
    );
    repo.commit("add files");
    repo
}

/// A comparison where one file extracts a helper out of a complex function and
/// another only edits a function in place.
fn extraction_repo() -> TestRepo {
    const REFACTOR_BASE: &str = "export function process(values: number[]) {\n  let total = 0;\n  for (const value of values) {\n    if (value > 0) {\n      for (const inner of values) {\n        if (inner % 2 === 0) {\n          total += inner * value;\n        } else if (inner > value) {\n          total += inner;\n        }\n      }\n    } else {\n      total -= value;\n    }\n  }\n  return total;\n}\n";
    const REFACTOR_TARGET: &str = "function totalFor(values: number[], value: number) {\n  let total = 0;\n  for (const inner of values) {\n    if (inner % 2 === 0) {\n      if (inner > value) {\n        total += inner * value;\n      }\n    } else if (inner < value) {\n      total += inner;\n    }\n  }\n  return total;\n}\n\nexport function process(values: number[]) {\n  let total = 0;\n  for (const value of values) {\n    if (value > 0) {\n      total += totalFor(values, value);\n    }\n  }\n  return total;\n}\n";
    const PLAIN_BASE: &str = "export function plain(value: number) {\n  return value;\n}\n";
    const PLAIN_TARGET: &str = "export function plain(value: number) {\n  return value + 1;\n}\n";

    let repo = TestRepo::with_base();
    repo.write("src/refactor.ts", REFACTOR_BASE);
    repo.write("src/plain.ts", PLAIN_BASE);
    repo.commit("add sources");
    repo.write("src/refactor.ts", REFACTOR_TARGET);
    repo.write("src/plain.ts", PLAIN_TARGET);
    repo.commit("change sources");
    repo
}

struct TestRepo {
    path: std::path::PathBuf,
}

impl TestRepo {
    fn with_base() -> Self {
        let path = std::env::temp_dir().join(format!(
            "diffscope-harness-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos()
        ));
        fs::create_dir(&path).expect("create repository");
        let repo = Self { path };
        repo.git(&["init"]);
        repo.git(&["config", "user.email", "diffscope@example.invalid"]);
        repo.git(&["config", "user.name", "DiffScope"]);
        repo.git(&["commit", "--allow-empty", "-m", "base"]);
        repo
    }

    fn path_str(&self) -> &str {
        self.path.to_str().expect("temporary path is UTF-8")
    }

    fn write(&self, relative: &str, contents: &str) {
        self.write_bytes(relative, contents.as_bytes());
    }

    fn write_bytes(&self, relative: &str, contents: &[u8]) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(path, contents).expect("write source file");
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", message]);
    }

    fn commit_id(&self, revision: &str) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["rev-parse", revision])
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git stdout is UTF-8")
            .trim()
            .to_owned()
    }

    fn git(&self, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(arguments)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.path);
    }
}
