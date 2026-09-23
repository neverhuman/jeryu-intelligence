//! Codegraph oracle services and compatibility facade.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use jeryu_rustjet::WorkspaceGraph;
use serde::{Deserialize, Serialize};

use governance::GovernanceMetadata;

use crate::graph::CodeGraph;
use crate::graph::ImpactReport;
use crate::storage::{
    CodeGraphStore, FileRow, GovernanceRow, GraphSnapshot, IndexRunRow, SymbolRefRow, SymbolRow,
};
use crate::{Result, error::CodeGraphError};

mod governance;
mod types;

pub use types::{
    CodeContextFile, CodeGraphImpactPack, CodeGraphMcpQuery, CodeGraphProvenance, CodeGraphQuery,
    CodeGraphRepoIdentity, ExcludedFile, GeneratedZoneHit, GraphStats, IndexReceipt,
    ProofLaneImpact, SymbolImpact, default_ref_name,
};

/// Query service for a materialized repository root and SQLite store.
#[derive(Debug, Clone)]
pub struct CodeGraphService {
    root: PathBuf,
    store: CodeGraphStore,
}

impl CodeGraphService {
    /// Build a service from a materialized repository root and store.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, store: CodeGraphStore) -> Self {
        Self {
            root: root.into(),
            store,
        }
    }

    /// Build/refresh the graph and return an auditable impact pack.
    pub fn query(
        &self,
        repo: CodeGraphRepoIdentity,
        commit: impl Into<String>,
        mut query: CodeGraphQuery,
    ) -> Result<CodeGraphImpactPack> {
        query.changed_paths = normalize_changed_paths(&query.changed_paths);
        let commit = commit.into();
        let workspace = WorkspaceGraph::load(&self.root)
            .map_err(|e| CodeGraphError::Workspace(e.to_string()))?;
        let graph = CodeGraph::index_workspace(&workspace)?;
        let impact = graph.impact_of(&workspace, &query.changed_paths);
        let governance = GovernanceMetadata::load(&self.root)?;
        let analyzer_scope = vec!["rust_cargo_exact".to_string()];
        let indexed_at = epoch_millis().to_string();
        let run_id = format!("codegraph-{}-{}", sanitize_id(&repo.id), indexed_at);

        let mut snapshot = graph.snapshot().clone();
        attach_governance_rows(&mut snapshot, &governance, &repo, &commit);

        let graph_stats = GraphStats {
            symbol_count: snapshot.symbols.len(),
            crate_dep_edges: snapshot.crate_deps.len(),
            indexed_file_count: snapshot.files.len(),
            governance_file_count: governance.loaded_files.len(),
            analyzers: analyzer_scope.clone(),
        };
        snapshot.index_runs.push(IndexRunRow {
            run_id: run_id.clone(),
            repo_id: repo.id.clone(),
            ref_name: query.ref_name.clone(),
            commit_sha: commit.clone(),
            root: self.root.display().to_string(),
            indexed_at: indexed_at.clone(),
            analyzer_scope_json: serde_json::to_string(&analyzer_scope)
                .map_err(|e| CodeGraphError::Storage(e.to_string()))?,
            graph_stats_json: serde_json::to_string(&graph_stats)
                .map_err(|e| CodeGraphError::Storage(e.to_string()))?,
        });
        self.store.persist(&snapshot)?;

        Ok(build_pack(
            repo,
            commit,
            query,
            PackBuildInput {
                workspace: &workspace,
                snapshot: &snapshot,
                impact: &impact,
                governance: &governance,
                graph_stats,
                index_receipt: IndexReceipt {
                    run_id,
                    store_path: self.store.path().display().to_string(),
                    ref_name: match snapshot.index_runs.last() {
                        Some(row) => row.ref_name.clone(),
                        None => String::new(),
                    },
                    commit: match snapshot.index_runs.last() {
                        Some(row) => row.commit_sha.clone(),
                        None => String::new(),
                    },
                    indexed_at,
                    analyzer_scope,
                },
            },
        ))
    }
}

struct PackBuildInput<'a> {
    workspace: &'a WorkspaceGraph,
    snapshot: &'a GraphSnapshot,
    impact: &'a ImpactReport,
    governance: &'a GovernanceMetadata,
    graph_stats: GraphStats,
    index_receipt: IndexReceipt,
}

fn build_pack(
    repo: CodeGraphRepoIdentity,
    commit: String,
    query: CodeGraphQuery,
    input: PackBuildInput<'_>,
) -> CodeGraphImpactPack {
    let PackBuildInput {
        workspace,
        snapshot,
        impact,
        governance,
        graph_stats,
        index_receipt,
    } = input;
    let mut must = BTreeMap::new();
    let mut should = BTreeMap::new();
    let file_rows: BTreeMap<&str, &FileRow> = snapshot
        .files
        .iter()
        .map(|row| (row.path.as_str(), row))
        .collect();

    for path in &query.changed_paths {
        insert_context(
            &mut must,
            path,
            0,
            "input_changed_path",
            "changed path supplied by caller",
            governance,
            file_rows.get(path.as_str()).copied(),
        );
    }

    for crate_name in &impact.changed_crates {
        if let Some(package) = workspace.package(crate_name) {
            let path = manifest_relative_path(package.relative_root.as_str());
            insert_context(
                &mut must,
                &path,
                20,
                "changed_crate_manifest",
                "Cargo manifest for a changed crate",
                governance,
                file_rows.get(path.as_str()).copied(),
            );
        }
    }

    for symbol in &snapshot.symbols {
        if impact.changed_crates.contains(&symbol.crate_name) {
            insert_context(
                &mut must,
                &symbol.file,
                30,
                "changed_crate_public_symbol_file",
                "public symbol file in a changed crate",
                governance,
                file_rows.get(symbol.file.as_str()).copied(),
            );
        } else if impact.affected_crates.contains(&symbol.crate_name) {
            insert_context(
                &mut should,
                &symbol.file,
                120,
                "affected_crate_public_symbol_file",
                "public symbol file in a reverse-dependent crate",
                governance,
                file_rows.get(symbol.file.as_str()).copied(),
            );
        }
    }

    for crate_name in impact.affected_crates.difference(&impact.changed_crates) {
        if let Some(package) = workspace.package(crate_name) {
            let path = manifest_relative_path(package.relative_root.as_str());
            insert_context(
                &mut should,
                &path,
                110,
                "affected_crate_manifest",
                "Cargo manifest for a reverse-dependent crate",
                governance,
                file_rows.get(path.as_str()).copied(),
            );
        }
    }

    for loaded in &governance.loaded_files {
        if !must.contains_key(loaded.path.as_str()) {
            insert_context(
                &mut should,
                &loaded.path,
                200,
                "governance_metadata",
                "loaded Jankurai governance metadata",
                governance,
                file_rows.get(loaded.path.as_str()).copied(),
            );
        }
    }

    let must_paths: BTreeSet<_> = must.keys().cloned().collect();
    should.retain(|path, _| !must_paths.contains(path));

    let mut proof_lanes = selected_proof_lanes(&query.changed_paths, governance);
    let mut suggested_commands: BTreeSet<String> = BTreeSet::new();
    for path in &query.changed_paths {
        if let Some(rule) = governance.test_for_path(path) {
            suggested_commands.insert(rule.command.clone());
        }
    }
    for lane in &proof_lanes {
        for command in &lane.required_commands {
            suggested_commands.insert(command.clone());
        }
    }
    for crate_name in &impact.changed_crates {
        suggested_commands.insert(format!("cargo test -p {crate_name} --jobs 40"));
    }
    proof_lanes.sort_by(|a, b| a.lane.cmp(&b.lane));

    let excluded_files = lexical_exclusions(&query, &must, &should, governance);

    let mut residual_risk = vec![
        "typescript/vite/react/security analyzers are outside the v1 authoritative analyzer scope; no authoritative results are emitted for those domains".to_string(),
    ];
    let unmapped: Vec<_> = query
        .changed_paths
        .iter()
        .filter(|path| workspace.package_for_path(path).is_none())
        .cloned()
        .collect();
    if !unmapped.is_empty() {
        residual_risk.push(format!(
            "changed paths not owned by a Rust workspace crate: {}",
            unmapped.join(", ")
        ));
    }
    if !excluded_files.is_empty() {
        residual_risk.push(
            "heuristic-only lexical matches were excluded from must_read context".to_string(),
        );
    }

    CodeGraphImpactPack {
        schema_version: "codegraph.query/v1".to_string(),
        repo,
        ref_name: query.ref_name.clone(),
        commit,
        changed_paths: query.changed_paths.clone(),
        intent: query.intent.clone(),
        question: query.question.clone(),
        max_tokens: query.max_tokens.unwrap_or(12_000),
        changed_crates: impact.changed_crates.iter().cloned().collect(),
        affected_crates: impact.affected_crates.iter().cloned().collect(),
        affected_symbols: affected_symbols(&snapshot.symbols, &impact.affected_crates),
        must_read_files: sorted_context(must),
        should_read_files: sorted_context(should),
        proof_lanes,
        suggested_commands: suggested_commands.into_iter().collect(),
        excluded_files,
        graph_stats,
        residual_risk,
        provenance: vec![
            CodeGraphProvenance {
                source: "git_ref".to_string(),
                detail: "repo/ref resolved before materialized indexing".to_string(),
                path: None,
            },
            CodeGraphProvenance {
                source: "rust_cargo_exact".to_string(),
                detail: "WorkspaceGraph package and reverse-dependency reachability".to_string(),
                path: Some("Cargo.toml".to_string()),
            },
            CodeGraphProvenance {
                source: "governance_ingestion".to_string(),
                detail: "Jankurai governance files loaded when present".to_string(),
                path: None,
            },
            CodeGraphProvenance {
                source: "sqlite_index_receipt".to_string(),
                detail: "index refresh persisted to SQLite".to_string(),
                path: Some(index_receipt.store_path.clone()),
            },
        ],
        index_receipt,
    }
}

fn affected_symbols(
    symbols: &[SymbolRow],
    affected_crates: &BTreeSet<String>,
) -> Vec<SymbolImpact> {
    symbols
        .iter()
        .filter(|row| affected_crates.contains(&row.crate_name))
        .map(|row| SymbolImpact {
            crate_name: row.crate_name.clone(),
            symbol: row.symbol.clone(),
            kind: row.kind.clone(),
            file: row.file.clone(),
            provenance: vec![CodeGraphProvenance {
                source: "rust_public_symbol_index".to_string(),
                detail: format!("symbol owned by affected crate {}", row.crate_name),
                path: Some(row.file.clone()),
            }],
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn insert_context(
    files: &mut BTreeMap<String, CodeContextFile>,
    path: &str,
    rank: u32,
    reason: &str,
    detail: &str,
    governance: &GovernanceMetadata,
    row: Option<&FileRow>,
) {
    let owner = match row {
        Some(file) => file.owner.clone(),
        None => governance.owner_for_path(path),
    };
    let mut proof_lanes = match row {
        Some(file) => file.proof_lanes.clone(),
        None => Vec::new(),
    };
    if let Some(rule) = governance.test_for_path(path)
        && !proof_lanes.contains(&rule.lane)
    {
        proof_lanes.push(rule.lane.clone());
    }
    proof_lanes.sort();
    proof_lanes.dedup();
    let generated_zone = governance.generated_zone_for_path(path);
    let editable = match row {
        Some(file) => file.editable,
        None => generated_zone.as_ref().is_none_or(|zone| zone.manual_edits),
    };

    let entry = files
        .entry(path.to_string())
        .or_insert_with(|| CodeContextFile {
            path: path.to_string(),
            reasons: Vec::new(),
            rank,
            owner,
            proof_lanes,
            generated_zone,
            editable,
            provenance: Vec::new(),
        });
    if !entry.reasons.iter().any(|value| value == reason) {
        entry.reasons.push(reason.to_string());
    }
    entry.rank = entry.rank.min(rank);
    entry.provenance.push(CodeGraphProvenance {
        source: reason.to_string(),
        detail: detail.to_string(),
        path: Some(path.to_string()),
    });
}

fn sorted_context(files: BTreeMap<String, CodeContextFile>) -> Vec<CodeContextFile> {
    let mut files: Vec<_> = files.into_values().collect();
    files.sort_by(|a, b| (a.rank, a.path.as_str()).cmp(&(b.rank, b.path.as_str())));
    files
}

fn selected_proof_lanes(
    changed_paths: &[String],
    governance: &GovernanceMetadata,
) -> Vec<ProofLaneImpact> {
    let mut lanes: BTreeMap<String, ProofLaneImpact> = BTreeMap::new();
    for path in changed_paths {
        let Some(test_rule) = governance.test_for_path(path) else {
            continue;
        };
        let lane_rule = governance.proof_lanes.get(&test_rule.lane);
        let required_commands = match lane_rule {
            Some(lane) if !lane.required.is_empty() => lane.required.clone(),
            _ => vec![test_rule.command.clone()],
        };
        lanes
            .entry(test_rule.lane.clone())
            .or_insert(ProofLaneImpact {
                lane: test_rule.lane.clone(),
                required_commands,
                blocks_merge: lane_rule.is_some_and(|lane| lane.blocks_merge),
                reason: format!("changed path {path} maps to test lane {}", test_rule.lane),
                provenance: vec![CodeGraphProvenance {
                    source: "test_map".to_string(),
                    detail: test_rule.purpose.clone(),
                    path: Some(path.to_string()),
                }],
            });
    }
    lanes.into_values().collect()
}

fn lexical_exclusions(
    query: &CodeGraphQuery,
    must: &BTreeMap<String, CodeContextFile>,
    should: &BTreeMap<String, CodeContextFile>,
    governance: &GovernanceMetadata,
) -> Vec<ExcludedFile> {
    let tokens = lexical_tokens(query);
    if tokens.is_empty() {
        return Vec::new();
    }
    let included: BTreeSet<&str> = must
        .keys()
        .chain(should.keys())
        .map(String::as_str)
        .collect();
    let mut excluded = Vec::new();
    for path in &governance.repo_files {
        if included.contains(path.as_str()) {
            continue;
        }
        let lower = path.to_ascii_lowercase();
        if tokens.iter().any(|token| lower.contains(token)) {
            excluded.push(ExcludedFile {
                path: path.clone(),
                reason: "heuristic_only_lexical_match".to_string(),
                provenance: vec![CodeGraphProvenance {
                    source: "lexical_fallback".to_string(),
                    detail: "matched intent/question token by file path only".to_string(),
                    path: Some(path.clone()),
                }],
            });
        }
        if excluded.len() >= 20 {
            break;
        }
    }
    excluded
}

fn lexical_tokens(query: &CodeGraphQuery) -> BTreeSet<String> {
    let text = [query.intent.as_deref(), query.question.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let stop = [
        "change", "changed", "question", "optional", "short", "task", "intent", "code", "what",
        "where", "which", "should", "read", "file", "files",
    ];
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() >= 4 && !stop.contains(&token.as_str()))
        .collect()
}

fn attach_governance_rows(
    snapshot: &mut GraphSnapshot,
    governance: &GovernanceMetadata,
    repo: &CodeGraphRepoIdentity,
    commit: &str,
) {
    for file in &mut snapshot.files {
        file.repo_id = repo.id.clone();
        file.commit_sha = commit.to_string();
        file.owner = governance.owner_for_path(&file.path);
        file.test_lane = governance
            .test_for_path(&file.path)
            .map(|rule| rule.lane.clone());
        file.proof_lanes = file.test_lane.iter().cloned().collect();
        file.generated_zone = governance
            .generated_zone_for_path(&file.path)
            .map(|zone| zone.path);
        file.editable = governance
            .generated_zone_for_path(&file.path)
            .is_none_or(|zone| zone.manual_edits);
        file.provenance_json = serde_json::to_string(&vec![CodeGraphProvenance {
            source: "rust_cargo_index".to_string(),
            detail: "file discovered by Rust/Cargo workspace index".to_string(),
            path: Some(file.path.clone()),
        }])
        .unwrap_or_else(|_| "[]".to_string());
    }

    for loaded in &governance.loaded_files {
        snapshot.files.push(FileRow {
            repo_id: repo.id.clone(),
            commit_sha: commit.to_string(),
            path: loaded.path.clone(),
            crate_name: None,
            language: "governance".to_string(),
            owner: governance.owner_for_path(&loaded.path),
            test_lane: governance
                .test_for_path(&loaded.path)
                .map(|rule| rule.lane.clone()),
            proof_lanes: match governance.test_for_path(&loaded.path) {
                Some(rule) => vec![rule.lane.clone()],
                None => Vec::new(),
            },
            generated_zone: governance
                .generated_zone_for_path(&loaded.path)
                .map(|zone| zone.path),
            editable: governance
                .generated_zone_for_path(&loaded.path)
                .is_none_or(|zone| zone.manual_edits),
            provenance_json: serde_json::to_string(&vec![CodeGraphProvenance {
                source: "governance_ingestion".to_string(),
                detail: format!("loaded {}", loaded.kind),
                path: Some(loaded.path.clone()),
            }])
            .unwrap_or_else(|_| "[]".to_string()),
        });
        snapshot.governance.push(GovernanceRow {
            repo_id: repo.id.clone(),
            commit_sha: commit.to_string(),
            path: loaded.path.clone(),
            kind: loaded.kind.clone(),
            digest: loaded.digest.clone(),
            loaded: true,
        });
    }
    snapshot.files.sort_by(|a, b| a.path.cmp(&b.path));
    snapshot.files.dedup_by(|a, b| a.path == b.path);
}

fn manifest_relative_path(relative_root: &str) -> String {
    if relative_root == "." || relative_root.is_empty() {
        "Cargo.toml".to_string()
    } else {
        format!("{relative_root}/Cargo.toml")
    }
}

pub(super) fn normalize_changed_paths(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = paths
        .iter()
        .map(|path| path.trim().trim_start_matches("./").replace('\\', "/"))
        .filter(|path| !path.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

pub(super) fn epoch_millis() -> u128 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis(),
        Err(_) => 0,
    }
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// Query accepted by the compatibility REST/MCP oracle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodegraphQuery {
    /// Repo-relative changed paths to analyze for impact.
    #[serde(default)]
    pub changed_paths: Vec<String>,
    /// Optional symbol to resolve and collect references for.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Optional crate to inspect for reverse dependencies.
    #[serde(default)]
    pub crate_name: Option<String>,
    /// Limit for symbol search results.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for CodegraphQuery {
    /// Matches what deserializing `{}` yields, so a hand-built query and a
    /// wire query ask for the same number of results.
    fn default() -> Self {
        Self {
            changed_paths: Vec::new(),
            symbol: None,
            crate_name: None,
            limit: default_limit(),
        }
    }
}

/// Oracle response consumed by older codegraph clients and newer agent repair flows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodegraphImpactPack {
    pub schema_version: String,
    pub provenance: CodegraphProvenance,
    pub impact: CodegraphImpact,
    pub symbols: Vec<SymbolRow>,
    pub definition: Option<SymbolRow>,
    pub references: Vec<SymbolRefRow>,
    pub reverse_deps: Vec<String>,
    pub required_reads: Vec<String>,
    pub proof_lanes: Vec<String>,
    pub suggested_commands: Vec<String>,
    pub misses: Vec<CodegraphMiss>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodegraphProvenance {
    pub storage_schema: String,
    pub source: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodegraphImpact {
    pub changed_crates: BTreeSet<String>,
    pub affected_crates: BTreeSet<String>,
    pub affected_symbols: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodegraphMiss {
    pub code: String,
    pub purpose: String,
    pub reason: String,
    pub common_fixes: Vec<String>,
    pub docs_url: String,
    pub repair_hint: String,
}

/// Build an oracle pack from a persisted codegraph store.
pub fn query_store(store: &CodeGraphStore, query: &CodegraphQuery) -> Result<CodegraphImpactPack> {
    let snapshot = store.load_snapshot()?;
    let schema_version = store.schema_version()?;
    Ok(query_snapshot(snapshot, schema_version, query))
}

/// Build an oracle pack from an already-loaded snapshot. This is the shared
/// deterministic path used by tests and the MCP memory backend.
#[must_use]
pub fn query_snapshot(
    snapshot: GraphSnapshot,
    schema_version: String,
    query: &CodegraphQuery,
) -> CodegraphImpactPack {
    let graph = CodeGraph::from_snapshot(snapshot);
    let symbols = match query.symbol.as_deref() {
        Some(symbol) => graph.search_symbols(symbol, query.limit),
        None => Vec::new(),
    };
    let definition = query
        .symbol
        .as_deref()
        .and_then(|symbol| graph.definition(symbol));
    let references = match query.symbol.as_deref() {
        Some(symbol) => graph.references(symbol),
        None => Vec::new(),
    };
    let reverse_deps = match query.crate_name.as_deref() {
        Some(name) => graph.reverse_deps(name),
        None => Vec::new(),
    };

    let mut changed_crates = BTreeSet::new();
    for path in &query.changed_paths {
        if let Some(crate_name) = crate_from_path(path, graph.snapshot()) {
            changed_crates.insert(crate_name);
        }
    }
    let mut affected_crates = changed_crates.clone();
    for crate_name in &changed_crates {
        for dependent in graph.reverse_deps(crate_name) {
            affected_crates.insert(dependent);
        }
    }
    let affected_symbols = graph
        .snapshot()
        .symbols
        .iter()
        .filter(|row| affected_crates.contains(&row.crate_name))
        .map(|row| row.symbol.clone())
        .collect();

    let mut required_reads = Vec::new();
    required_reads.extend(query.changed_paths.iter().cloned());
    if let Some(definition) = &definition {
        required_reads.push(definition.file.clone());
    }
    required_reads.extend(references.iter().map(|row| row.ref_file.clone()));
    required_reads.sort();
    required_reads.dedup();

    let mut misses = Vec::new();
    if query.symbol.is_some() && definition.is_none() {
        misses.push(miss(
            "codegraph_symbol_miss",
            "resolve codegraph symbol",
            "the requested symbol was not present in the current codegraph snapshot",
            "rerun `jeryu-codegraph index` and retry the query",
        ));
    }
    if query.crate_name.is_some() && reverse_deps.is_empty() {
        misses.push(miss(
            "codegraph_reverse_deps_empty",
            "resolve reverse dependency impact",
            "the requested crate has no recorded direct reverse dependencies",
            "rerun `jeryu-codegraph index` before treating this as final",
        ));
    }

    CodegraphImpactPack {
        schema_version: "codegraph.query/v1".to_string(),
        provenance: CodegraphProvenance {
            storage_schema: schema_version,
            source: "jeryu-codegraph/current-storage".to_string(),
        },
        impact: CodegraphImpact {
            changed_crates,
            affected_crates,
            affected_symbols,
        },
        symbols,
        definition,
        references,
        reverse_deps,
        required_reads,
        proof_lanes: vec![
            "rtk cargo test -p jeryu-codegraph -p jeryu-mcp --jobs 40 code".to_string(),
            "rtk bash ops/ci/codegraph-oracle.sh".to_string(),
        ],
        suggested_commands: vec![
            "rtk cargo run -p jeryu-codegraph -- index".to_string(),
            "rtk bash ops/ci/codegraph-oracle.sh".to_string(),
        ],
        misses,
    }
}

fn crate_from_path(path: &str, snapshot: &GraphSnapshot) -> Option<String> {
    snapshot
        .symbols
        .iter()
        .filter(|row| path.starts_with(row.file.trim_end_matches("src/lib.rs")))
        .max_by_key(|row| row.file.len())
        .map(|row| row.crate_name.clone())
        .or_else(|| {
            snapshot
                .symbols
                .iter()
                .find(|row| row.file == path)
                .map(|row| row.crate_name.clone())
        })
}

fn miss(code: &str, purpose: &str, reason: &str, repair_hint: &str) -> CodegraphMiss {
    CodegraphMiss {
        code: code.to_string(),
        purpose: purpose.to_string(),
        reason: reason.to_string(),
        common_fixes: vec![
            "refresh the codegraph SQLite snapshot".to_string(),
            "rerun the codegraph oracle proof lane".to_string(),
        ],
        docs_url: "docs/errors.md#not-found".to_string(),
        repair_hint: repair_hint.to_string(),
    }
}

fn default_limit() -> usize {
    20
}

impl From<CodeGraphError> for CodegraphMiss {
    fn from(error: CodeGraphError) -> Self {
        miss(
            "codegraph_storage_error",
            "load codegraph query evidence",
            &error.to_string(),
            "rerun `jeryu-codegraph index`, then rerun the codegraph oracle proof lane",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codegraph-oracle-{tag}-{nanos}"));
        std::fs::create_dir_all(&root).expect("temp root");
        root
    }

    fn write(root: &std::path::Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write");
    }

    /// A repo root carrying the governance files the oracle reads.
    fn governed_root(tag: &str) -> PathBuf {
        let root = tmp_root(tag);
        write(&root, "AGENTS.md", "# rules\n");
        write(
            &root,
            "agent/owner-map.json",
            r#"{"owners": {"crates/core/": "core-team", "crates/": "platform"}}"#,
        );
        write(
            &root,
            "agent/test-map.json",
            r#"{"tests": {"crates/core/": {"command": "cargo test -p core_lib",
                 "purpose": "prove the core crate", "lane": "check"}}}"#,
        );
        write(
            &root,
            "agent/generated-zones.toml",
            "[[zones]]\npath = \"crates/core/src/generated/\"\ngenerator = \"just score\"\nmanual_edits = false\n",
        );
        write(
            &root,
            "agent/proof-lanes.toml",
            "[lanes.check]\nrequired = [\"just check\"]\nblocks_merge = true\n",
        );
        write(&root, "crates/core/src/lib.rs", "pub fn core_api() {}\n");
        write(
            &root,
            "crates/core/src/generated/table.rs",
            "// generated\n",
        );
        write(
            &root,
            "crates/api/src/retry_policy.rs",
            "pub fn retry() {}\n",
        );
        root
    }

    fn governance(tag: &str) -> (PathBuf, GovernanceMetadata) {
        let root = governed_root(tag);
        let metadata = GovernanceMetadata::load(&root).expect("governance");
        (root, metadata)
    }

    fn symbol(crate_name: &str, file: &str, symbol: &str) -> SymbolRow {
        SymbolRow {
            crate_name: crate_name.to_string(),
            file: file.to_string(),
            symbol: symbol.to_string(),
            kind: "public".to_string(),
            is_public: true,
            line: 1,
        }
    }

    fn snapshot() -> GraphSnapshot {
        GraphSnapshot {
            symbols: vec![
                symbol("core_lib", "crates/core/src/lib.rs", "core_api"),
                symbol("api_lib", "crates/api/src/lib.rs", "api_entry"),
            ],
            crate_deps: vec![crate::storage::CrateDepRow {
                crate_name: "api_lib".to_string(),
                depends_on: "core_lib".to_string(),
            }],
            symbol_refs: vec![SymbolRefRow {
                crate_name: "core_lib".to_string(),
                file: "crates/core/src/lib.rs".to_string(),
                symbol: "core_api".to_string(),
                ref_file: "crates/api/src/lib.rs".to_string(),
                ref_line: 4,
                ref_kind: "call".to_string(),
            }],
            ..GraphSnapshot::default()
        }
    }

    #[test]
    fn changed_paths_are_normalized_sorted_and_deduplicated() {
        let paths = normalize_changed_paths(&[
            "  crates/api/src/lib.rs  ".to_string(),
            "./crates/api/src/lib.rs".to_string(),
            "crates\\core\\src\\lib.rs".to_string(),
            "   ".to_string(),
            String::new(),
        ]);
        assert_eq!(
            paths,
            vec![
                "crates/api/src/lib.rs".to_string(),
                "crates/core/src/lib.rs".to_string(),
            ]
        );
        assert!(normalize_changed_paths(&[]).is_empty());
    }

    #[test]
    fn run_ids_keep_only_characters_that_are_safe_in_an_id() {
        assert_eq!(sanitize_id("jeryu/intelligence"), "jeryu-intelligence");
        assert_eq!(sanitize_id("repo-1"), "repo-1");
        assert_eq!(sanitize_id("a b.c_d"), "a-b-c-d");
        assert!(epoch_millis() > 0);
    }

    #[test]
    fn the_workspace_manifest_path_follows_the_relative_root() {
        assert_eq!(manifest_relative_path("."), "Cargo.toml");
        assert_eq!(manifest_relative_path(""), "Cargo.toml");
        assert_eq!(
            manifest_relative_path("crates/core"),
            "crates/core/Cargo.toml"
        );
    }

    #[test]
    fn lexical_tokens_drop_short_words_and_task_vocabulary() {
        let query = CodeGraphQuery {
            intent: Some("Change the RETRY policy".to_string()),
            question: Some("which files own backoff?".to_string()),
            ..CodeGraphQuery::default()
        };
        let tokens = lexical_tokens(&query);
        assert!(tokens.contains("retry"), "{tokens:?}");
        assert!(tokens.contains("policy"));
        assert!(tokens.contains("backoff"));
        // "change", "which" and "files" are task vocabulary; "the" is too short.
        for noise in ["change", "which", "files", "the"] {
            assert!(!tokens.contains(noise), "{noise} is not a code token");
        }
        assert!(lexical_tokens(&CodeGraphQuery::default()).is_empty());
    }

    #[test]
    fn lexical_matches_are_reported_as_excluded_not_as_context() {
        let (root, governance) = governance("lexical");
        let query = CodeGraphQuery {
            intent: Some("tune retry_policy".to_string()),
            ..CodeGraphQuery::default()
        };
        let empty = BTreeMap::new();
        let excluded = lexical_exclusions(&query, &empty, &empty, &governance);
        let paths: Vec<&str> = excluded.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, vec!["crates/api/src/retry_policy.rs"]);
        assert_eq!(excluded[0].reason, "heuristic_only_lexical_match");
        assert_eq!(excluded[0].provenance[0].source, "lexical_fallback");

        // A file already carried as context is never also reported as excluded.
        let mut must = BTreeMap::new();
        insert_context(
            &mut must,
            "crates/api/src/retry_policy.rs",
            0,
            "changed_path",
            "changed by the task",
            &governance,
            None,
        );
        assert!(lexical_exclusions(&query, &must, &empty, &governance).is_empty());

        // No lexical tokens, no exclusions to report.
        assert!(
            lexical_exclusions(&CodeGraphQuery::default(), &empty, &empty, &governance).is_empty()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn context_entries_merge_reasons_and_keep_the_strongest_rank() {
        let (root, governance) = governance("context");
        let mut files = BTreeMap::new();
        insert_context(
            &mut files,
            "crates/core/src/lib.rs",
            2,
            "affected_crate",
            "crate is downstream of the change",
            &governance,
            None,
        );
        insert_context(
            &mut files,
            "crates/core/src/lib.rs",
            0,
            "changed_path",
            "changed by the task",
            &governance,
            None,
        );
        insert_context(
            &mut files,
            "crates/core/src/lib.rs",
            1,
            "changed_path",
            "seen again",
            &governance,
            None,
        );
        let entry = &files["crates/core/src/lib.rs"];
        assert_eq!(entry.rank, 0, "the strongest rank wins");
        assert_eq!(
            entry.reasons,
            vec!["affected_crate".to_string(), "changed_path".to_string()],
            "a repeated reason is recorded once"
        );
        assert_eq!(entry.provenance.len(), 3, "every inclusion leaves evidence");
        // Governance fills the owner and lane when no indexed row exists. The
        // longest matching owner rule wins over the broader one.
        assert_eq!(entry.owner.as_deref(), Some("core-team"));
        assert_eq!(entry.proof_lanes, vec!["check".to_string()]);
        assert!(entry.editable);
        assert!(entry.generated_zone.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_generated_file_is_carried_as_context_but_not_as_editable() {
        let (root, governance) = governance("generated");
        let mut files = BTreeMap::new();
        insert_context(
            &mut files,
            "crates/core/src/generated/table.rs",
            1,
            "changed_path",
            "changed by the task",
            &governance,
            None,
        );
        let entry = &files["crates/core/src/generated/table.rs"];
        assert!(!entry.editable, "a generated zone is not hand-edited");
        assert_eq!(
            entry
                .generated_zone
                .as_ref()
                .map(|zone| zone.generator.as_str()),
            Some("just score")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_indexed_row_wins_over_the_governance_fallback() {
        let (root, governance) = governance("indexed-row");
        let row = FileRow {
            repo_id: "repo".to_string(),
            commit_sha: "c1".to_string(),
            path: "crates/core/src/lib.rs".to_string(),
            crate_name: Some("core_lib".to_string()),
            language: "rust".to_string(),
            owner: Some("indexed-owner".to_string()),
            test_lane: None,
            proof_lanes: vec!["fast".to_string()],
            generated_zone: None,
            editable: false,
            provenance_json: "[]".to_string(),
        };
        let mut files = BTreeMap::new();
        insert_context(
            &mut files,
            "crates/core/src/lib.rs",
            0,
            "changed_path",
            "changed by the task",
            &governance,
            Some(&row),
        );
        let entry = &files["crates/core/src/lib.rs"];
        assert_eq!(entry.owner.as_deref(), Some("indexed-owner"));
        assert!(!entry.editable);
        // The row's lanes and the governed lane are unioned, sorted, deduped.
        assert_eq!(
            entry.proof_lanes,
            vec!["check".to_string(), "fast".to_string()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn context_is_sorted_by_rank_then_path() {
        let (root, governance) = governance("sorted");
        let mut files = BTreeMap::new();
        for (path, rank) in [
            ("crates/api/src/retry_policy.rs", 1u32),
            ("crates/core/src/lib.rs", 0),
            ("AGENTS.md", 1),
        ] {
            insert_context(
                &mut files,
                path,
                rank,
                "reason",
                "detail",
                &governance,
                None,
            );
        }
        let paths: Vec<String> = sorted_context(files)
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(
            paths,
            vec![
                "crates/core/src/lib.rs".to_string(),
                "AGENTS.md".to_string(),
                "crates/api/src/retry_policy.rs".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn proof_lanes_come_from_the_lane_rule_and_fall_back_to_the_test_command() {
        let (root, governance) = governance("lanes");
        let lanes = selected_proof_lanes(
            &[
                "crates/core/src/lib.rs".to_string(),
                "crates/core/src/generated/table.rs".to_string(),
                "crates/api/src/retry_policy.rs".to_string(),
            ],
            &governance,
        );
        assert_eq!(lanes.len(), 1, "an unmapped path selects no lane");
        assert_eq!(lanes[0].lane, "check");
        assert_eq!(lanes[0].required_commands, vec!["just check".to_string()]);
        assert!(lanes[0].blocks_merge);
        assert_eq!(lanes[0].provenance[0].source, "test_map");
        assert!(lanes[0].reason.contains("crates/core/src/lib.rs"));

        // Without a matching lane rule, the test-map command is the proof.
        let mut orphaned = GovernanceMetadata::load(&root).expect("governance");
        orphaned.proof_lanes.clear();
        let lanes = selected_proof_lanes(&["crates/core/src/lib.rs".to_string()], &orphaned);
        assert_eq!(
            lanes[0].required_commands,
            vec!["cargo test -p core_lib".to_string()]
        );
        assert!(!lanes[0].blocks_merge);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_symbols_owned_by_an_affected_crate_are_reported() {
        let snapshot = snapshot();
        let affected: BTreeSet<String> = ["core_lib".to_string()].into_iter().collect();
        let impacts = affected_symbols(&snapshot.symbols, &affected);
        assert_eq!(impacts.len(), 1);
        assert_eq!(impacts[0].symbol, "core_api");
        assert_eq!(impacts[0].provenance[0].source, "rust_public_symbol_index");
        assert!(impacts[0].provenance[0].detail.contains("core_lib"));
        assert!(affected_symbols(&snapshot.symbols, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_changed_path_resolves_to_its_owning_crate() {
        let snapshot = snapshot();
        assert_eq!(
            crate_from_path("crates/core/src/lib.rs", &snapshot),
            Some("core_lib".to_string())
        );
        assert_eq!(
            crate_from_path("crates/core/src/handler.rs", &snapshot),
            Some("core_lib".to_string()),
            "a sibling file belongs to the same crate"
        );
        assert_eq!(crate_from_path("docs/readme.md", &snapshot), None);
    }

    #[test]
    fn a_snapshot_query_answers_symbol_crate_and_path_questions() {
        let pack = query_snapshot(
            snapshot(),
            "5".to_string(),
            &CodegraphQuery {
                changed_paths: vec!["crates/core/src/lib.rs".to_string()],
                symbol: Some("core_api".to_string()),
                crate_name: Some("core_lib".to_string()),
                limit: default_limit(),
            },
        );
        assert_eq!(pack.schema_version, "codegraph.query/v1");
        assert_eq!(pack.provenance.storage_schema, "5");
        assert_eq!(
            pack.definition.as_ref().map(|row| row.symbol.as_str()),
            Some("core_api")
        );
        assert_eq!(pack.reverse_deps, vec!["api_lib".to_string()]);
        assert_eq!(
            pack.impact.changed_crates,
            ["core_lib".to_string()].into_iter().collect()
        );
        assert_eq!(
            pack.impact.affected_crates,
            ["api_lib".to_string(), "core_lib".to_string()]
                .into_iter()
                .collect(),
            "dependents of a changed crate are affected too"
        );
        assert!(pack.impact.affected_symbols.contains("api_entry"));
        // Changed paths, the definition site and every reference, once each.
        assert_eq!(
            pack.required_reads,
            vec![
                "crates/api/src/lib.rs".to_string(),
                "crates/core/src/lib.rs".to_string(),
            ]
        );
        assert!(pack.misses.is_empty());
        assert!(!pack.proof_lanes.is_empty());
        assert!(!pack.suggested_commands.is_empty());
    }

    #[test]
    fn an_empty_query_asks_nothing_and_misses_nothing() {
        let pack = query_snapshot(snapshot(), "5".to_string(), &CodegraphQuery::default());
        assert!(pack.symbols.is_empty());
        assert!(pack.definition.is_none());
        assert!(pack.references.is_empty());
        assert!(pack.reverse_deps.is_empty());
        assert!(pack.required_reads.is_empty());
        assert!(pack.misses.is_empty(), "nothing was asked, nothing missed");
    }

    #[test]
    fn unresolved_questions_come_back_as_repairable_misses() {
        let pack = query_snapshot(
            snapshot(),
            "5".to_string(),
            &CodegraphQuery {
                symbol: Some("absent_symbol".to_string()),
                crate_name: Some("api_lib".to_string()),
                limit: 5,
                ..CodegraphQuery::default()
            },
        );
        let codes: Vec<&str> = pack.misses.iter().map(|miss| miss.code.as_str()).collect();
        assert_eq!(
            codes,
            vec!["codegraph_symbol_miss", "codegraph_reverse_deps_empty"]
        );
        for miss in &pack.misses {
            assert!(!miss.repair_hint.is_empty());
            assert!(!miss.common_fixes.is_empty());
            assert_eq!(miss.docs_url, "docs/errors.md#not-found");
        }
    }

    #[test]
    fn a_storage_failure_is_reported_as_a_miss_a_caller_can_act_on() {
        let miss = CodegraphMiss::from(CodeGraphError::Storage("disk is gone".to_string()));
        assert_eq!(miss.code, "codegraph_storage_error");
        assert!(miss.reason.contains("disk is gone"));
        assert!(miss.repair_hint.contains("jeryu-codegraph index"));
    }

    #[test]
    fn a_query_without_a_limit_deserializes_to_the_default() {
        let query: CodegraphQuery = serde_json::from_str("{}").expect("parse");
        assert_eq!(query.limit, 20);
        assert_eq!(
            query,
            CodegraphQuery::default(),
            "a hand-built query asks for as much as a wire query"
        );
    }

    #[test]
    fn governance_rows_are_attached_to_every_indexed_and_loaded_file() {
        let (root, metadata) = governance("attach");
        let mut snapshot = GraphSnapshot {
            files: vec![FileRow {
                repo_id: String::new(),
                commit_sha: String::new(),
                path: "crates/core/src/generated/table.rs".to_string(),
                crate_name: Some("core_lib".to_string()),
                language: "rust".to_string(),
                owner: None,
                test_lane: None,
                proof_lanes: Vec::new(),
                generated_zone: None,
                editable: true,
                provenance_json: String::new(),
            }],
            ..GraphSnapshot::default()
        };
        attach_governance_rows(
            &mut snapshot,
            &metadata,
            &CodeGraphRepoIdentity::from_repo_string("repo"),
            "c1",
        );

        let indexed = snapshot
            .files
            .iter()
            .find(|file| file.path == "crates/core/src/generated/table.rs")
            .expect("indexed file");
        assert_eq!(indexed.repo_id, "repo");
        assert_eq!(indexed.commit_sha, "c1");
        assert_eq!(indexed.owner.as_deref(), Some("core-team"));
        assert_eq!(indexed.test_lane.as_deref(), Some("check"));
        assert_eq!(indexed.proof_lanes, vec!["check".to_string()]);
        assert!(!indexed.editable, "the file sits in a generated zone");
        assert!(indexed.provenance_json.contains("rust_cargo_index"));

        // Every governance file the loader read is carried as a row of its own.
        let kinds: BTreeSet<&str> = snapshot
            .governance
            .iter()
            .map(|row| row.kind.as_str())
            .collect();
        assert_eq!(
            kinds,
            [
                "agents",
                "generated_zones",
                "owner_map",
                "proof_lanes",
                "test_map"
            ]
            .into_iter()
            .collect()
        );
        assert!(snapshot.governance.iter().all(|row| row.loaded));
        assert!(
            snapshot
                .files
                .iter()
                .any(|file| file.path == "agent/owner-map.json" && file.language == "governance")
        );
        // Paths are sorted and never duplicated.
        let paths: Vec<&str> = snapshot.files.iter().map(|f| f.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(paths, sorted);
        let _ = std::fs::remove_dir_all(&root);
    }
}
