//! Store-level tests: schema, persistence round trips and tool-build
//! cluster storage.

use super::*;
use crate::tool_build::{ToolBuildCategory, ToolBuildFileCoverage, ToolBuildOccurrence};

fn temp_store(tag: &str) -> CodeGraphStore {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir()
        .join(format!("codegraph-storage-{tag}-{nanos}"))
        .join("codegraph.sqlite");
    CodeGraphStore::open(path).expect("open store")
}

fn symbol(crate_name: &str, file: &str, symbol: &str) -> SymbolRow {
    SymbolRow {
        crate_name: crate_name.to_string(),
        file: file.to_string(),
        symbol: symbol.to_string(),
        kind: "public".to_string(),
        is_public: true,
        line: 7,
    }
}

fn snapshot() -> GraphSnapshot {
    GraphSnapshot {
        symbols: vec![
            symbol("core_lib", "crates/core/src/lib.rs", "core_api"),
            symbol("api_lib", "crates/api/src/lib.rs", "api_entry"),
        ],
        crate_deps: vec![CrateDepRow {
            crate_name: "api_lib".to_string(),
            depends_on: "core_lib".to_string(),
        }],
        symbol_refs: vec![
            SymbolRefRow {
                crate_name: "core_lib".to_string(),
                file: "crates/core/src/lib.rs".to_string(),
                symbol: "core_api".to_string(),
                ref_file: "crates/api/src/lib.rs".to_string(),
                ref_line: 12,
                ref_kind: "call".to_string(),
            },
            SymbolRefRow {
                crate_name: "core_lib".to_string(),
                file: "crates/core/src/lib.rs".to_string(),
                symbol: "core_api".to_string(),
                ref_file: "crates/api/src/handler.rs".to_string(),
                ref_line: 3,
                ref_kind: "call".to_string(),
            },
        ],
        files: vec![FileRow {
            repo_id: "repo".to_string(),
            commit_sha: "c1".to_string(),
            path: "crates/core/src/lib.rs".to_string(),
            crate_name: Some("core_lib".to_string()),
            language: "rust".to_string(),
            owner: Some("core-team".to_string()),
            test_lane: Some("check".to_string()),
            proof_lanes: vec!["check".to_string()],
            generated_zone: None,
            editable: true,
            provenance_json: "[]".to_string(),
        }],
        governance: vec![GovernanceRow {
            repo_id: "repo".to_string(),
            commit_sha: "c1".to_string(),
            path: "agent/owner-map.json".to_string(),
            kind: "owner_map".to_string(),
            digest: "digest".to_string(),
            loaded: true,
        }],
        index_runs: vec![IndexRunRow {
            run_id: "run-1".to_string(),
            repo_id: "repo".to_string(),
            ref_name: "main".to_string(),
            commit_sha: "c1".to_string(),
            root: "/tmp/repo".to_string(),
            indexed_at: "1".to_string(),
            analyzer_scope_json: "[]".to_string(),
            graph_stats_json: "{}".to_string(),
        }],
    }
}

fn cluster(cluster_id: &str, repo_id: &str, score: u64) -> ToolBuildCluster {
    ToolBuildCluster {
        cluster_id: cluster_id.to_string(),
        repo_id: repo_id.to_string(),
        commit_sha: "working-tree".to_string(),
        fingerprint: format!("{cluster_id}-fingerprint"),
        score,
        occurrence_count: 3,
        repo_count: 2,
        file_count: 3,
        total_lines: 24,
        language: "rust".to_string(),
        insight: "repeats".to_string(),
        normalized_preview: "call:issue_token member:checksum".to_string(),
        category: ToolBuildCategory::ToolCandidate,
        member_cluster_ids: Vec::new(),
        coverage: vec![ToolBuildFileCoverage {
            repo_id: repo_id.to_string(),
            path: "src/lib.rs".to_string(),
            spans: vec![(10, 17)],
        }],
        occurrences: vec![ToolBuildOccurrence {
            repo_id: repo_id.to_string(),
            commit_sha: "working-tree".to_string(),
            path: "src/lib.rs".to_string(),
            start_line: 10,
            end_line: 17,
            language: "rust".to_string(),
            normalized_token_count: 60,
            is_test: false,
        }],
        ignored: None,
    }
}

fn report(repo_id: &str, clusters: Vec<ToolBuildCluster>) -> ToolBuildScanReport {
    ToolBuildScanReport {
        repo_id: repo_id.to_string(),
        commit_sha: "working-tree".to_string(),
        root: "/tmp/repo".to_string(),
        scanned_at: "1700000000000".to_string(),
        scanned_files: 2,
        skipped_files: 0,
        clusters,
        families: Vec::new(),
    }
}

#[test]
fn opening_creates_the_parent_directory_and_applies_the_schema() {
    let store = temp_store("open");
    assert!(store.path().exists(), "the database file was created");
    assert_eq!(store.schema_version().expect("version"), "5");
    // Opening again is a no-op, not a second schema application.
    let reopened = CodeGraphStore::open(store.path()).expect("reopen");
    assert_eq!(reopened.schema_version().expect("version"), "5");
}

#[test]
fn the_default_path_lives_under_the_local_share_tree() {
    let path = default_db_path();
    assert!(path.ends_with("share/jeryu/codegraph.sqlite"), "{path:?}");
}

#[test]
fn persisting_replaces_the_previous_snapshot_wholesale() {
    let store = temp_store("persist-replace");
    store.persist(&snapshot()).expect("persist");
    let mut second = snapshot();
    second.symbols = vec![symbol("core_lib", "crates/core/src/lib.rs", "core_api")];
    second.crate_deps.clear();
    store.persist(&second).expect("persist again");

    let loaded = store.load_snapshot().expect("load");
    assert_eq!(loaded.symbols.len(), 1, "stale rows are gone");
    assert!(loaded.crate_deps.is_empty());
    assert_eq!(loaded.files.len(), 1);
    assert_eq!(loaded.governance.len(), 1);
    assert_eq!(loaded.index_runs.len(), 1);
    assert_eq!(loaded.files[0].proof_lanes, vec!["check".to_string()]);
}

#[test]
fn symbol_search_matches_symbol_file_or_crate_and_honors_the_limit() {
    let store = temp_store("search");
    store.persist(&snapshot()).expect("persist");

    let by_symbol = store.search_symbols("core_api", 10).expect("search");
    assert_eq!(by_symbol.len(), 1);
    assert_eq!(by_symbol[0].symbol, "core_api");
    assert!(by_symbol[0].is_public);
    assert_eq!(by_symbol[0].line, 7);

    let by_file = store.search_symbols("crates/", 10).expect("search");
    let crates: Vec<&str> = by_file.iter().map(|row| row.crate_name.as_str()).collect();
    assert_eq!(crates, vec!["api_lib", "core_lib"], "ordered by crate");

    let by_crate = store.search_symbols("_lib", 10).expect("search");
    assert_eq!(by_crate.len(), 2);

    // A zero limit still returns one row rather than nothing at all.
    assert_eq!(store.search_symbols("crates/", 0).expect("search").len(), 1);
    assert!(
        store
            .search_symbols("nothing", 10)
            .expect("search")
            .is_empty()
    );
}

#[test]
fn definition_resolves_an_exact_symbol_and_misses_cleanly() {
    let store = temp_store("definition");
    store.persist(&snapshot()).expect("persist");
    let found = store.definition("core_api").expect("definition");
    assert_eq!(
        found.map(|row| row.file),
        Some("crates/core/src/lib.rs".to_string())
    );
    // A substring match is not a definition.
    assert!(store.definition("core").expect("definition").is_none());
}

#[test]
fn references_and_reverse_deps_come_back_ordered() {
    let store = temp_store("references");
    store.persist(&snapshot()).expect("persist");
    let refs = store.references("core_api").expect("references");
    let files: Vec<&str> = refs.iter().map(|row| row.ref_file.as_str()).collect();
    assert_eq!(
        files,
        vec!["crates/api/src/handler.rs", "crates/api/src/lib.rs"]
    );
    assert!(
        store
            .references("api_entry")
            .expect("references")
            .is_empty()
    );

    assert_eq!(
        store.reverse_deps("core_lib").expect("deps"),
        vec!["api_lib"]
    );
    assert!(store.reverse_deps("api_lib").expect("deps").is_empty());
}

#[test]
fn a_tool_build_report_round_trips_with_coverage_and_category() {
    let store = temp_store("tool-build-round-trip");
    let mut row = cluster("toolbuild-aaaa", "system/host", 900);
    row.category = ToolBuildCategory::ManagedScaffold;
    row.member_cluster_ids = vec!["toolbuild-bbbb".to_string()];
    store
        .persist_tool_build_report(&report("system/host", vec![row.clone()]))
        .expect("persist");

    let loaded = store.tool_build_clusters(None, 10, false).expect("load");
    assert_eq!(loaded, vec![row]);
    assert_eq!(
        store.tool_build_scanned_at("system/host").expect("stamp"),
        Some("1700000000000".to_string())
    );
    assert_eq!(store.tool_build_scanned_at("absent").expect("stamp"), None);
}

#[test]
fn a_rescan_replaces_only_its_own_repo_and_commit() {
    let store = temp_store("tool-build-scope");
    store
        .persist_tool_build_report(&report(
            "family/jeryu",
            vec![cluster("toolbuild-shared", "family/jeryu", 500)],
        ))
        .expect("persist family");
    store
        .persist_tool_build_report(&report(
            "system/host",
            vec![cluster("toolbuild-shared", "system/host", 900)],
        ))
        .expect("persist system");

    // Same cluster id under two scan labels: both rows survive.
    assert_eq!(store.tool_build_cluster_counts(None).expect("counts").0, 2);

    // Rescanning one label drops only that label's rows.
    store
        .persist_tool_build_report(&report("system/host", Vec::new()))
        .expect("rescan");
    let remaining = store.tool_build_clusters(None, 10, false).expect("load");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].repo_id, "family/jeryu");
}

#[test]
fn clusters_are_ranked_by_score_then_occurrences_then_id() {
    let store = temp_store("tool-build-rank");
    let mut tie_low = cluster("toolbuild-zzz", "system/host", 500);
    tie_low.occurrence_count = 9;
    let mut tie_high = cluster("toolbuild-aaa", "system/host", 500);
    tie_high.occurrence_count = 9;
    store
        .persist_tool_build_report(&report(
            "system/host",
            vec![
                tie_low,
                cluster("toolbuild-top", "system/host", 900),
                tie_high,
            ],
        ))
        .expect("persist");

    let ids: Vec<String> = store
        .tool_build_clusters(Some("system/host"), 10, false)
        .expect("load")
        .into_iter()
        .map(|cluster| cluster.cluster_id)
        .collect();
    assert_eq!(ids, vec!["toolbuild-top", "toolbuild-aaa", "toolbuild-zzz"]);

    // The limit trims the tail, and it never collapses to nothing.
    assert_eq!(
        store
            .tool_build_clusters(Some("system/host"), 1, false)
            .expect("load")
            .len(),
        1
    );
    assert_eq!(
        store
            .tool_build_clusters(Some("system/host"), 0, false)
            .expect("load")
            .len(),
        1
    );
    // Filtering by repo id excludes other scan labels.
    assert!(
        store
            .tool_build_clusters(Some("family/jeryu"), 10, false)
            .expect("load")
            .is_empty()
    );
}

#[test]
fn ignored_clusters_are_hidden_unless_asked_for() {
    let store = temp_store("tool-build-ignore");
    store
        .persist_tool_build_report(&report(
            "system/host",
            vec![
                cluster("toolbuild-keep", "system/host", 900),
                cluster("toolbuild-drop", "system/host", 800),
            ],
        ))
        .expect("persist");

    let ignored = store
        .ignore_tool_build_cluster("toolbuild-drop", "scaffold", "operator")
        .expect("ignore");
    assert_eq!(ignored.cluster_id, "toolbuild-drop");
    assert_eq!(ignored.reason, "scaffold");
    assert!(!ignored.ignored_at.is_empty());

    let visible = store.tool_build_clusters(None, 10, false).expect("load");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].cluster_id, "toolbuild-keep");
    assert!(visible[0].ignored.is_none());

    let all = store.tool_build_clusters(None, 10, true).expect("load");
    assert_eq!(all.len(), 2);
    let dropped = all
        .iter()
        .find(|cluster| cluster.cluster_id == "toolbuild-drop")
        .expect("ignored row");
    assert_eq!(
        dropped.ignored.as_ref().map(|row| row.reason.as_str()),
        Some("scaffold")
    );
    assert_eq!(
        store.tool_build_cluster_counts(None).expect("counts"),
        (2, 1)
    );
    assert_eq!(
        store
            .tool_build_cluster_counts(Some("system/host"))
            .expect("counts"),
        (2, 1)
    );
    assert_eq!(
        store
            .tool_build_cluster_counts(Some("other"))
            .expect("counts"),
        (0, 0)
    );

    // Re-ignoring the same cluster updates the row instead of duplicating it.
    store
        .ignore_tool_build_cluster("toolbuild-drop", "already governed", "operator")
        .expect("re-ignore");
    assert_eq!(
        store.tool_build_cluster_counts(None).expect("counts"),
        (2, 1)
    );
}

#[test]
fn an_ignore_on_a_window_id_is_inherited_by_the_merged_cluster() {
    let store = temp_store("tool-build-inherit");
    store
        .ignore_tool_build_cluster("toolbuild-window", "fixture noise", "operator")
        .expect("ignore");

    let mut merged = cluster("toolbuild-merged", "system/host", 900);
    merged.member_cluster_ids = vec![
        "toolbuild-merged".to_string(),
        "toolbuild-window".to_string(),
    ];
    let unmerged = cluster("toolbuild-plain", "system/host", 800);
    let mut already = cluster("toolbuild-already", "system/host", 700);
    already.member_cluster_ids = vec!["toolbuild-window".to_string()];
    already.ignored = Some(ToolBuildIgnore {
        cluster_id: "toolbuild-already".to_string(),
        reason: "direct".to_string(),
        ignored_by: "operator".to_string(),
        ignored_at: "1".to_string(),
    });

    let clusters = vec![merged, unmerged, already];
    assert_eq!(
        store
            .propagate_ignores_to_merged(&clusters)
            .expect("inherit"),
        1,
        "only the merged cluster with an unignored member inherits"
    );
    // Inheriting twice records nothing new.
    assert_eq!(
        store
            .propagate_ignores_to_merged(&clusters)
            .expect("inherit"),
        0
    );

    store
        .persist_tool_build_report(&report("system/host", clusters))
        .expect("persist");
    let visible: Vec<String> = store
        .tool_build_clusters(None, 10, false)
        .expect("load")
        .into_iter()
        .map(|cluster| cluster.cluster_id)
        .collect();
    // The merged cluster is now hidden. `toolbuild-already` arrived
    // carrying its own ignore, so propagation skipped it and wrote no row.
    assert_eq!(visible, vec!["toolbuild-plain", "toolbuild-already"]);

    let inherited = store
        .tool_build_clusters(None, 10, true)
        .expect("load")
        .into_iter()
        .find(|cluster| cluster.cluster_id == "toolbuild-merged")
        .and_then(|cluster| cluster.ignored)
        .expect("inherited ignore");
    assert!(
        inherited
            .reason
            .starts_with("inherited: toolbuild-window: "),
        "{}",
        inherited.reason
    );
    assert_eq!(inherited.ignored_by, "operator");
}

#[test]
fn families_are_grouped_from_the_persisted_rows_on_read() {
    let store = temp_store("tool-build-families");
    let mut first = cluster("toolbuild-one", "system/host", 900);
    first.normalized_preview =
        "call:issue_token member:checksum call:audit_log call:notify_subscribers".to_string();
    let mut second = cluster("toolbuild-two", "system/host", 800);
    second.normalized_preview = first.normalized_preview.clone();
    store
        .persist_tool_build_report(&report("system/host", vec![first, second]))
        .expect("persist");

    let families = store
        .tool_build_families(None, 10, false)
        .expect("families");
    assert_eq!(families.len(), 1);
    assert_eq!(families[0].cluster_ids.len(), 2);
    assert_eq!(families[0].score_total, 1700);
}

#[test]
fn the_coverage_column_is_added_once_to_a_pre_coverage_table() {
    let store = temp_store("coverage-column");
    let conn = store.connect().expect("connect");
    conn.execute_batch(
        "DROP TABLE codegraph_tool_build_clusters; \
         CREATE TABLE codegraph_tool_build_clusters (cluster_id TEXT NOT NULL);",
    )
    .expect("shape a pre-coverage table");

    add_cluster_coverage_column(&conn).expect("add column");
    add_cluster_coverage_column(&conn).expect("second call is a no-op");
    let columns = conn
        .prepare("SELECT * FROM codegraph_tool_build_clusters LIMIT 0")
        .expect("prepare")
        .column_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    assert_eq!(columns, vec!["cluster_id", "coverage_json"]);
}

#[test]
fn a_row_with_unreadable_json_fails_the_query_instead_of_panicking() {
    let store = temp_store("bad-json");
    store
        .persist_tool_build_report(&report(
            "system/host",
            vec![cluster("toolbuild-bad", "system/host", 900)],
        ))
        .expect("persist");
    store
        .connect()
        .expect("connect")
        .execute(
            "UPDATE codegraph_tool_build_clusters SET occurrences_json = 'not json'",
            [],
        )
        .expect("corrupt the row");

    let error = store
        .tool_build_clusters(None, 10, false)
        .expect_err("unreadable json is an error");
    assert!(matches!(error, CodeGraphError::Storage(_)), "{error:?}");
}
