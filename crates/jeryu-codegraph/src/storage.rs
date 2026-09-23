//! Self-contained SQLite storage for the code graph.
//!
//! Mirrors the `SqliteStore` pattern in `jeryu-core::engine::storage`
//! (open -> apply schema via `execute_batch` -> atomic snapshot persist via a
//! transaction) but is fully self-contained: this crate opens its own
//! `codegraph.sqlite` and applies its own embedded schema. It never touches the
//! shared `db/migrations/` set and never edits `jeryu-core`.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params, types::Type};
use serde::{Deserialize, Serialize};

use crate::error::{CodeGraphError, Result};
use crate::tool_build::{ToolBuildCluster, ToolBuildIgnore, ToolBuildScanReport};

/// Embedded code-graph schema. Applied via `execute_batch` on open.
pub const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS codegraph_symbols (
    crate     TEXT NOT NULL,
    file      TEXT NOT NULL,
    symbol    TEXT NOT NULL,
    kind      TEXT NOT NULL,
    is_public INTEGER NOT NULL,
    line      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS codegraph_crate_deps (
    crate      TEXT NOT NULL,
    depends_on TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS codegraph_symbol_refs (
    crate     TEXT NOT NULL,
    file      TEXT NOT NULL,
    symbol    TEXT NOT NULL,
    ref_file  TEXT NOT NULL,
    ref_line  INTEGER NOT NULL,
    ref_kind  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS codegraph_files (
    repo_id          TEXT NOT NULL,
    commit_sha       TEXT NOT NULL,
    path             TEXT NOT NULL,
    crate            TEXT,
    language         TEXT NOT NULL,
    owner            TEXT,
    test_lane        TEXT,
    proof_lanes_json TEXT NOT NULL,
    generated_zone   TEXT,
    editable         INTEGER NOT NULL,
    provenance_json  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS codegraph_governance (
    repo_id    TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    path       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    digest     TEXT NOT NULL,
    loaded     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS codegraph_index_runs (
    run_id              TEXT PRIMARY KEY,
    repo_id             TEXT NOT NULL,
    ref_name            TEXT NOT NULL,
    commit_sha          TEXT NOT NULL,
    root                TEXT NOT NULL,
    indexed_at          TEXT NOT NULL,
    analyzer_scope_json TEXT NOT NULL,
    graph_stats_json    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS codegraph_slice_locks (
    id           TEXT PRIMARY KEY,
    crate        TEXT NOT NULL,
    prefixes_json TEXT NOT NULL,
    locked_by    TEXT NOT NULL,
    reason       TEXT NOT NULL,
    locked_at    TEXT NOT NULL,
    expires_at   TEXT
);

CREATE TABLE IF NOT EXISTS codegraph_tool_build_clusters (
    cluster_id         TEXT NOT NULL,
    repo_id            TEXT NOT NULL,
    commit_sha         TEXT NOT NULL,
    fingerprint        TEXT NOT NULL,
    score              INTEGER NOT NULL,
    occurrence_count   INTEGER NOT NULL,
    repo_count         INTEGER NOT NULL,
    file_count         INTEGER NOT NULL,
    total_lines        INTEGER NOT NULL,
    language           TEXT NOT NULL,
    insight            TEXT NOT NULL,
    normalized_preview TEXT NOT NULL,
    occurrences_json   TEXT NOT NULL,
    created_at         TEXT NOT NULL,
    category           TEXT NOT NULL DEFAULT 'tool-candidate',
    member_cluster_ids_json TEXT NOT NULL DEFAULT '[]',
    coverage_json      TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (repo_id, cluster_id)
);

CREATE INDEX IF NOT EXISTS idx_codegraph_tool_build_clusters_rank
ON codegraph_tool_build_clusters (repo_id, score DESC, occurrence_count DESC);

CREATE TABLE IF NOT EXISTS codegraph_tool_build_ignores (
    cluster_id TEXT PRIMARY KEY,
    reason     TEXT NOT NULL,
    ignored_by TEXT NOT NULL,
    ignored_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS codegraph_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

INSERT OR REPLACE INTO codegraph_meta (key, value) VALUES ('schema_version', '5');
"#;

/// Default database location under the user's local Jeryu data directory.
#[must_use]
pub fn default_db_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local")
        .join("share")
        .join("jeryu")
        .join("codegraph.sqlite")
}

/// A row in `codegraph_symbols`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRow {
    /// Owning crate (workspace package name).
    pub crate_name: String,
    /// Repo-relative source file path.
    pub file: String,
    /// Symbol name.
    pub symbol: String,
    /// Symbol kind (e.g. `public`).
    pub kind: String,
    /// Whether the symbol is part of the public API.
    pub is_public: bool,
    /// 1-based line number (0 when unknown).
    pub line: u32,
}

/// A row in `codegraph_crate_deps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrateDepRow {
    /// Dependent crate.
    pub crate_name: String,
    /// Crate it depends on.
    pub depends_on: String,
}

/// A row in `codegraph_symbol_refs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRefRow {
    /// Owning crate for the referenced symbol.
    pub crate_name: String,
    /// Definition file for the referenced symbol.
    pub file: String,
    /// Referenced symbol name.
    pub symbol: String,
    /// Repo-relative file containing the reference.
    pub ref_file: String,
    /// 1-based reference line number (0 when unknown).
    pub ref_line: u32,
    /// Reference kind, for example `call`, `type`, or `mention`.
    pub ref_kind: String,
}

/// A repo file recorded with governance and provenance metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRow {
    /// Stable repository id.
    pub repo_id: String,
    /// Commit sha the row was indexed from.
    pub commit_sha: String,
    /// Repo-relative file path.
    pub path: String,
    /// Owning Rust crate when known.
    pub crate_name: Option<String>,
    /// Analyzer language/domain label.
    pub language: String,
    /// Owner-map owner when known.
    pub owner: Option<String>,
    /// Test-map lane when known.
    pub test_lane: Option<String>,
    /// Proof lanes attached to the file.
    pub proof_lanes: Vec<String>,
    /// Matching generated-zone path when the file is generated.
    pub generated_zone: Option<String>,
    /// Whether an agent should treat the file as directly editable.
    pub editable: bool,
    /// JSON provenance records for this row.
    pub provenance_json: String,
}

/// A loaded governance metadata file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernanceRow {
    /// Stable repository id.
    pub repo_id: String,
    /// Commit sha the row was loaded from.
    pub commit_sha: String,
    /// Repo-relative governance path.
    pub path: String,
    /// Governance kind, such as `owner_map` or `proof_lanes`.
    pub kind: String,
    /// Lightweight content digest for provenance.
    pub digest: String,
    /// Whether the file was present and loaded.
    pub loaded: bool,
}

/// Receipt for one index refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRunRow {
    /// Stable run id for this refresh.
    pub run_id: String,
    /// Stable repository id.
    pub repo_id: String,
    /// Ref name requested by the caller.
    pub ref_name: String,
    /// Commit sha indexed.
    pub commit_sha: String,
    /// Materialized root that was indexed.
    pub root: String,
    /// Timestamp for the refresh.
    pub indexed_at: String,
    /// JSON array of enabled analyzers.
    pub analyzer_scope_json: String,
    /// JSON object with graph counts.
    pub graph_stats_json: String,
}

/// A persistable snapshot of the code graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSnapshot {
    /// All indexed symbol rows.
    pub symbols: Vec<SymbolRow>,
    /// All recorded crate dependency edges.
    pub crate_deps: Vec<CrateDepRow>,
    /// All recorded symbol reference rows.
    pub symbol_refs: Vec<SymbolRefRow>,
    /// Files with attached governance metadata.
    pub files: Vec<FileRow>,
    /// Governance metadata files loaded for this snapshot.
    pub governance: Vec<GovernanceRow>,
    /// Index refresh receipts.
    pub index_runs: Vec<IndexRunRow>,
}

/// Self-contained SQLite store for the code graph.
#[derive(Debug, Clone)]
pub struct CodeGraphStore {
    path: PathBuf,
}

impl CodeGraphStore {
    /// Opens (creating if needed) the store at `path` and applies the embedded
    /// schema. Mirrors `SqliteStore::open`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        }
        let store = Self { path };
        let conn = store.connect()?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        migrate_tool_build_clusters(&conn)?;
        add_cluster_coverage_column(&conn)?;
        Ok(store)
    }

    /// Opens the store at the default `~/.jeryu/codegraph.sqlite` path.
    pub fn open_default() -> Result<Self> {
        Self::open(default_db_path())
    }

    fn connect(&self) -> Result<Connection> {
        let conn =
            Connection::open(&self.path).map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        // Concurrent readers/writers are normal here: the live API serves the
        // dashboard while a scan persists, and CLI scans run alongside the
        // server. Wait for the lock instead of failing with "database is
        // locked".
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        Ok(conn)
    }

    /// Persists a full snapshot atomically, mirroring `SqliteStore::persist`
    /// (delete-all then re-insert inside a single transaction).
    pub fn persist(&self, snapshot: &GraphSnapshot) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn
            .transaction()
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        tx.execute_batch(
            "DELETE FROM codegraph_symbols; \
             DELETE FROM codegraph_crate_deps; \
             DELETE FROM codegraph_symbol_refs; \
             DELETE FROM codegraph_files; \
             DELETE FROM codegraph_governance; \
             DELETE FROM codegraph_index_runs;",
        )
        .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        for row in &snapshot.symbols {
            tx.execute(
                "INSERT INTO codegraph_symbols (crate, file, symbol, kind, is_public, line) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.crate_name,
                    row.file,
                    row.symbol,
                    row.kind,
                    i64::from(row.is_public),
                    i64::from(row.line),
                ],
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        }
        for dep in &snapshot.crate_deps {
            tx.execute(
                "INSERT INTO codegraph_crate_deps (crate, depends_on) VALUES (?1, ?2)",
                params![dep.crate_name, dep.depends_on],
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        }
        for reference in &snapshot.symbol_refs {
            tx.execute(
                "INSERT INTO codegraph_symbol_refs \
                 (crate, file, symbol, ref_file, ref_line, ref_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    reference.crate_name,
                    reference.file,
                    reference.symbol,
                    reference.ref_file,
                    i64::from(reference.ref_line),
                    reference.ref_kind,
                ],
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        }
        for file in &snapshot.files {
            tx.execute(
                "INSERT INTO codegraph_files \
                 (repo_id, commit_sha, path, crate, language, owner, test_lane, \
                  proof_lanes_json, generated_zone, editable, provenance_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    file.repo_id,
                    file.commit_sha,
                    file.path,
                    file.crate_name,
                    file.language,
                    file.owner,
                    file.test_lane,
                    serde_json::to_string(&file.proof_lanes)
                        .map_err(|e| CodeGraphError::Storage(e.to_string()))?,
                    file.generated_zone,
                    i64::from(file.editable),
                    file.provenance_json,
                ],
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        }
        for row in &snapshot.governance {
            tx.execute(
                "INSERT INTO codegraph_governance \
                 (repo_id, commit_sha, path, kind, digest, loaded) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.repo_id,
                    row.commit_sha,
                    row.path,
                    row.kind,
                    row.digest,
                    i64::from(row.loaded),
                ],
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        }
        for row in &snapshot.index_runs {
            tx.execute(
                "INSERT INTO codegraph_index_runs \
                 (run_id, repo_id, ref_name, commit_sha, root, indexed_at, \
                  analyzer_scope_json, graph_stats_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    row.run_id,
                    row.repo_id,
                    row.ref_name,
                    row.commit_sha,
                    row.root,
                    row.indexed_at,
                    row.analyzer_scope_json,
                    row.graph_stats_json,
                ],
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Loads the full snapshot back from storage.
    pub fn load_snapshot(&self) -> Result<GraphSnapshot> {
        let conn = self.connect()?;
        let mut snapshot = GraphSnapshot::default();

        let mut stmt = conn
            .prepare(
                "SELECT crate, file, symbol, kind, is_public, line \
                 FROM codegraph_symbols ORDER BY crate, file, symbol",
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SymbolRow {
                    crate_name: row.get(0)?,
                    file: row.get(1)?,
                    symbol: row.get(2)?,
                    kind: row.get(3)?,
                    is_public: row.get::<_, i64>(4)? != 0,
                    line: row.get::<_, i64>(5)? as u32,
                })
            })
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        for row in rows {
            snapshot
                .symbols
                .push(row.map_err(|e| CodeGraphError::Storage(e.to_string()))?);
        }

        let mut dep_stmt = conn
            .prepare(
                "SELECT crate, depends_on FROM codegraph_crate_deps \
                 ORDER BY crate, depends_on",
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        let dep_rows = dep_stmt
            .query_map([], |row| {
                Ok(CrateDepRow {
                    crate_name: row.get(0)?,
                    depends_on: row.get(1)?,
                })
            })
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        for row in dep_rows {
            snapshot
                .crate_deps
                .push(row.map_err(|e| CodeGraphError::Storage(e.to_string()))?);
        }

        let mut ref_stmt = conn
            .prepare(
                "SELECT crate, file, symbol, ref_file, ref_line, ref_kind \
                 FROM codegraph_symbol_refs ORDER BY crate, symbol, ref_file, ref_line",
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        let ref_rows = ref_stmt
            .query_map([], |row| {
                Ok(SymbolRefRow {
                    crate_name: row.get(0)?,
                    file: row.get(1)?,
                    symbol: row.get(2)?,
                    ref_file: row.get(3)?,
                    ref_line: row.get::<_, i64>(4)? as u32,
                    ref_kind: row.get(5)?,
                })
            })
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        for row in ref_rows {
            snapshot
                .symbol_refs
                .push(row.map_err(|e| CodeGraphError::Storage(e.to_string()))?);
        }

        let mut file_stmt = conn
            .prepare(
                "SELECT repo_id, commit_sha, path, crate, language, owner, test_lane, \
                 proof_lanes_json, generated_zone, editable, provenance_json \
                 FROM codegraph_files ORDER BY path",
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        let file_rows = file_stmt
            .query_map([], |row| {
                let proof_lanes_json: String = row.get(7)?;
                let proof_lanes: Vec<String> = serde_json::from_str(&proof_lanes_json)
                    .map_err(|error| sqlite_json_error(7, error))?;
                Ok(FileRow {
                    repo_id: row.get(0)?,
                    commit_sha: row.get(1)?,
                    path: row.get(2)?,
                    crate_name: row.get(3)?,
                    language: row.get(4)?,
                    owner: row.get(5)?,
                    test_lane: row.get(6)?,
                    proof_lanes,
                    generated_zone: row.get(8)?,
                    editable: row.get::<_, i64>(9)? != 0,
                    provenance_json: row.get(10)?,
                })
            })
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        for row in file_rows {
            snapshot
                .files
                .push(row.map_err(|e| CodeGraphError::Storage(e.to_string()))?);
        }

        let mut gov_stmt = conn
            .prepare(
                "SELECT repo_id, commit_sha, path, kind, digest, loaded \
                 FROM codegraph_governance ORDER BY path",
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        let gov_rows = gov_stmt
            .query_map([], |row| {
                Ok(GovernanceRow {
                    repo_id: row.get(0)?,
                    commit_sha: row.get(1)?,
                    path: row.get(2)?,
                    kind: row.get(3)?,
                    digest: row.get(4)?,
                    loaded: row.get::<_, i64>(5)? != 0,
                })
            })
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        for row in gov_rows {
            snapshot
                .governance
                .push(row.map_err(|e| CodeGraphError::Storage(e.to_string()))?);
        }

        let mut run_stmt = conn
            .prepare(
                "SELECT run_id, repo_id, ref_name, commit_sha, root, indexed_at, \
                 analyzer_scope_json, graph_stats_json \
                 FROM codegraph_index_runs ORDER BY indexed_at, run_id",
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        let run_rows = run_stmt
            .query_map([], |row| {
                Ok(IndexRunRow {
                    run_id: row.get(0)?,
                    repo_id: row.get(1)?,
                    ref_name: row.get(2)?,
                    commit_sha: row.get(3)?,
                    root: row.get(4)?,
                    indexed_at: row.get(5)?,
                    analyzer_scope_json: row.get(6)?,
                    graph_stats_json: row.get(7)?,
                })
            })
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        for row in run_rows {
            snapshot
                .index_runs
                .push(row.map_err(|e| CodeGraphError::Storage(e.to_string()))?);
        }

        Ok(snapshot)
    }

    /// Search persisted symbols by substring, ordered deterministically.
    pub fn search_symbols(&self, query: &str, limit: usize) -> Result<Vec<SymbolRow>> {
        let conn = self.connect()?;
        let pattern = format!("%{query}%");
        let mut stmt = conn
            .prepare(
                "SELECT crate, file, symbol, kind, is_public, line \
                 FROM codegraph_symbols \
                 WHERE symbol LIKE ?1 OR file LIKE ?1 OR crate LIKE ?1 \
                 ORDER BY crate, file, symbol LIMIT ?2",
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![pattern, limit.max(1) as i64], |row| {
                Ok(SymbolRow {
                    crate_name: row.get(0)?,
                    file: row.get(1)?,
                    symbol: row.get(2)?,
                    kind: row.get(3)?,
                    is_public: row.get::<_, i64>(4)? != 0,
                    line: row.get::<_, i64>(5)? as u32,
                })
            })
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Return the first persisted definition row for `symbol`.
    pub fn definition(&self, symbol: &str) -> Result<Option<SymbolRow>> {
        Ok(self
            .search_symbols(symbol, 100)?
            .into_iter()
            .find(|row| row.symbol == symbol))
    }

    /// Return all persisted references for `symbol`.
    pub fn references(&self, symbol: &str) -> Result<Vec<SymbolRefRow>> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT crate, file, symbol, ref_file, ref_line, ref_kind \
                 FROM codegraph_symbol_refs WHERE symbol = ?1 \
                 ORDER BY crate, symbol, ref_file, ref_line",
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![symbol], |row| {
                Ok(SymbolRefRow {
                    crate_name: row.get(0)?,
                    file: row.get(1)?,
                    symbol: row.get(2)?,
                    ref_file: row.get(3)?,
                    ref_line: row.get::<_, i64>(4)? as u32,
                    ref_kind: row.get(5)?,
                })
            })
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Return crates that directly depend on `crate_name`.
    pub fn reverse_deps(&self, crate_name: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT crate FROM codegraph_crate_deps WHERE depends_on = ?1 ORDER BY crate")
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![crate_name], |row| row.get::<_, String>(0))
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        collect_rows(rows)
    }

    /// Return the embedded schema version recorded in `codegraph_meta`.
    pub fn schema_version(&self) -> Result<String> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT value FROM codegraph_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| CodeGraphError::Storage(e.to_string()))
    }

    /// Persist the ranked tool-building clusters from a fast scan.
    pub fn persist_tool_build_report(&self, report: &ToolBuildScanReport) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn
            .transaction()
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        tx.execute(
            "DELETE FROM codegraph_tool_build_clusters WHERE repo_id = ?1 AND commit_sha = ?2",
            params![report.repo_id, report.commit_sha],
        )
        .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        for cluster in &report.clusters {
            tx.execute(
                "INSERT OR REPLACE INTO codegraph_tool_build_clusters \
                 (cluster_id, repo_id, commit_sha, fingerprint, score, occurrence_count, \
                  repo_count, file_count, total_lines, language, insight, normalized_preview, \
                  occurrences_json, created_at, category, member_cluster_ids_json, \
                  coverage_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                  ?17)",
                params![
                    cluster.cluster_id,
                    cluster.repo_id,
                    cluster.commit_sha,
                    cluster.fingerprint,
                    i64::try_from(cluster.score).unwrap_or(i64::MAX),
                    i64::try_from(cluster.occurrence_count).unwrap_or(i64::MAX),
                    i64::try_from(cluster.repo_count).unwrap_or(i64::MAX),
                    i64::try_from(cluster.file_count).unwrap_or(i64::MAX),
                    i64::try_from(cluster.total_lines).unwrap_or(i64::MAX),
                    cluster.language,
                    cluster.insight,
                    cluster.normalized_preview,
                    serde_json::to_string(&cluster.occurrences)
                        .map_err(|e| CodeGraphError::Storage(e.to_string()))?,
                    report.scanned_at,
                    cluster.category.as_str(),
                    serde_json::to_string(&cluster.member_cluster_ids)
                        .map_err(|e| CodeGraphError::Storage(e.to_string()))?,
                    serde_json::to_string(&cluster.coverage)
                        .map_err(|e| CodeGraphError::Storage(e.to_string()))?,
                ],
            )
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Return ranked tool-building clusters. Ignored clusters are excluded by default.
    pub fn tool_build_clusters(
        &self,
        repo_id: Option<&str>,
        limit: usize,
        include_ignored: bool,
    ) -> Result<Vec<ToolBuildCluster>> {
        let conn = self.connect()?;
        let limit = limit.max(1);
        let sql_all = "SELECT c.cluster_id, c.repo_id, c.commit_sha, c.fingerprint, c.score, \
                       c.occurrence_count, c.repo_count, c.file_count, c.total_lines, \
                       c.language, c.insight, c.normalized_preview, c.occurrences_json, \
                       c.category, c.member_cluster_ids_json, \
                       i.reason, i.ignored_by, i.ignored_at, c.coverage_json \
                       FROM codegraph_tool_build_clusters c \
                       LEFT JOIN codegraph_tool_build_ignores i ON i.cluster_id = c.cluster_id";
        let order = " ORDER BY c.score DESC, c.occurrence_count DESC, c.cluster_id LIMIT ?";
        let rows = match (repo_id, include_ignored) {
            (Some(repo_id), true) => {
                let mut stmt = conn
                    .prepare(&format!("{sql_all} WHERE c.repo_id = ?{order}"))
                    .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
                let rows = stmt
                    .query_map(
                        params![repo_id, i64::try_from(limit).unwrap_or(i64::MAX)],
                        tool_build_cluster_from_row,
                    )
                    .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
                collect_rows(rows)?
            }
            (Some(repo_id), false) => {
                let mut stmt = conn
                    .prepare(&format!(
                        "{sql_all} WHERE c.repo_id = ? AND i.cluster_id IS NULL{order}"
                    ))
                    .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
                let rows = stmt
                    .query_map(
                        params![repo_id, i64::try_from(limit).unwrap_or(i64::MAX)],
                        tool_build_cluster_from_row,
                    )
                    .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
                collect_rows(rows)?
            }
            (None, true) => {
                let mut stmt = conn
                    .prepare(&format!("{sql_all}{order}"))
                    .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
                let rows = stmt
                    .query_map(
                        params![i64::try_from(limit).unwrap_or(i64::MAX)],
                        tool_build_cluster_from_row,
                    )
                    .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
                collect_rows(rows)?
            }
            (None, false) => {
                let mut stmt = conn
                    .prepare(&format!("{sql_all} WHERE i.cluster_id IS NULL{order}"))
                    .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
                let rows = stmt
                    .query_map(
                        params![i64::try_from(limit).unwrap_or(i64::MAX)],
                        tool_build_cluster_from_row,
                    )
                    .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
                collect_rows(rows)?
            }
        };
        Ok(rows)
    }

    /// Return `(total_clusters, ignored_clusters)` for the tool-building index.
    pub fn tool_build_cluster_counts(&self, repo_id: Option<&str>) -> Result<(usize, usize)> {
        let conn = self.connect()?;
        let (total, ignored): (i64, i64) = if let Some(repo_id) = repo_id {
            conn.query_row(
                "SELECT COUNT(*), COUNT(i.cluster_id) \
                 FROM codegraph_tool_build_clusters c \
                 LEFT JOIN codegraph_tool_build_ignores i ON i.cluster_id = c.cluster_id \
                 WHERE c.repo_id = ?1",
                params![repo_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        } else {
            conn.query_row(
                "SELECT COUNT(*), COUNT(i.cluster_id) \
                 FROM codegraph_tool_build_clusters c \
                 LEFT JOIN codegraph_tool_build_ignores i ON i.cluster_id = c.cluster_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        }
        .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        Ok((total as usize, ignored as usize))
    }

    /// Record durable feedback that a tool-building cluster should be ignored.
    pub fn ignore_tool_build_cluster(
        &self,
        cluster_id: &str,
        reason: &str,
        ignored_by: &str,
    ) -> Result<ToolBuildIgnore> {
        let ignored = ToolBuildIgnore {
            cluster_id: cluster_id.to_string(),
            reason: reason.to_string(),
            ignored_by: ignored_by.to_string(),
            ignored_at: epoch_millis(),
        };
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO codegraph_tool_build_ignores \
             (cluster_id, reason, ignored_by, ignored_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                ignored.cluster_id,
                ignored.reason,
                ignored.ignored_by,
                ignored.ignored_at,
            ],
        )
        .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
        Ok(ignored)
    }

    /// The `created_at` stamp (unix millis) of the persisted scan for
    /// `repo_id`, or `None` when no scan has been persisted.
    pub fn tool_build_scanned_at(&self, repo_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT MAX(created_at) FROM codegraph_tool_build_clusters WHERE repo_id = ?1",
            params![repo_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|e| CodeGraphError::Storage(e.to_string()))
    }

    /// Group the persisted clusters for `repo_id` into pattern families.
    /// Families are computed on read (a pure function of the cluster rows),
    /// so they can never go stale against the persisted clusters.
    pub fn tool_build_families(
        &self,
        repo_id: Option<&str>,
        limit: usize,
        include_ignored: bool,
    ) -> Result<Vec<crate::tool_build::ToolBuildClusterFamily>> {
        let clusters = self.tool_build_clusters(repo_id, limit, include_ignored)?;
        Ok(crate::tool_build::group_pattern_families(&clusters))
    }

    /// Inherit ignore feedback recorded against pre-merge window cluster ids
    /// onto the merged clusters that absorbed them, so an operator's earlier
    /// "not a lead" verdict survives overlap merging. Returns how many merged
    /// clusters gained an inherited ignore.
    pub fn propagate_ignores_to_merged(&self, clusters: &[ToolBuildCluster]) -> Result<usize> {
        let conn = self.connect()?;
        let mut inherited = 0usize;
        for cluster in clusters {
            if cluster.member_cluster_ids.is_empty() || cluster.ignored.is_some() {
                continue;
            }
            for member_id in &cluster.member_cluster_ids {
                if member_id == &cluster.cluster_id {
                    continue;
                }
                let existing: Option<(String, String)> = conn
                    .query_row(
                        "SELECT reason, ignored_by FROM codegraph_tool_build_ignores \
                         WHERE cluster_id = ?1",
                        params![member_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map(Some)
                    .or_else(|error| match error {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(CodeGraphError::Storage(other.to_string())),
                    })?;
                let Some((reason, ignored_by)) = existing else {
                    continue;
                };
                let changed = conn
                    .execute(
                        "INSERT OR IGNORE INTO codegraph_tool_build_ignores \
                         (cluster_id, reason, ignored_by, ignored_at) VALUES (?1, ?2, ?3, ?4)",
                        params![
                            cluster.cluster_id,
                            format!("inherited: {member_id}: {reason}"),
                            ignored_by,
                            epoch_millis(),
                        ],
                    )
                    .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
                if changed > 0 {
                    inherited += 1;
                }
                break;
            }
        }
        Ok(inherited)
    }

    /// Returns the on-disk path of this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One-time, shape-detected rebuild of `codegraph_tool_build_clusters` from
/// the v3 `PRIMARY KEY (cluster_id)` shape to the v4 composite
/// `PRIMARY KEY (repo_id, cluster_id)` shape (with category/member columns).
/// Without this, a `system/host` scan would steal rows from the
/// `family/jeryu-split` scan: the same window fingerprint yields the same
/// cluster_id in both, and `INSERT OR REPLACE` on a single-column PK
/// overwrites the other scan's row. No-op on already-migrated and fresh DBs.
fn migrate_tool_build_clusters(conn: &Connection) -> Result<()> {
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' \
             AND name = 'codegraph_tool_build_clusters'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| CodeGraphError::Storage(e.to_string()))?;
    if sql.contains("PRIMARY KEY (repo_id, cluster_id)") {
        return Ok(());
    }
    conn.execute_batch(
        r#"
BEGIN;
CREATE TABLE codegraph_tool_build_clusters_v4 (
    cluster_id         TEXT NOT NULL,
    repo_id            TEXT NOT NULL,
    commit_sha         TEXT NOT NULL,
    fingerprint        TEXT NOT NULL,
    score              INTEGER NOT NULL,
    occurrence_count   INTEGER NOT NULL,
    repo_count         INTEGER NOT NULL,
    file_count         INTEGER NOT NULL,
    total_lines        INTEGER NOT NULL,
    language           TEXT NOT NULL,
    insight            TEXT NOT NULL,
    normalized_preview TEXT NOT NULL,
    occurrences_json   TEXT NOT NULL,
    created_at         TEXT NOT NULL,
    category           TEXT NOT NULL DEFAULT 'tool-candidate',
    member_cluster_ids_json TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (repo_id, cluster_id)
);
INSERT INTO codegraph_tool_build_clusters_v4
    (cluster_id, repo_id, commit_sha, fingerprint, score, occurrence_count,
     repo_count, file_count, total_lines, language, insight, normalized_preview,
     occurrences_json, created_at)
  SELECT cluster_id, repo_id, commit_sha, fingerprint, score, occurrence_count,
     repo_count, file_count, total_lines, language, insight, normalized_preview,
     occurrences_json, created_at
  FROM codegraph_tool_build_clusters;
DROP TABLE codegraph_tool_build_clusters;
ALTER TABLE codegraph_tool_build_clusters_v4 RENAME TO codegraph_tool_build_clusters;
CREATE INDEX IF NOT EXISTS idx_codegraph_tool_build_clusters_rank
ON codegraph_tool_build_clusters (repo_id, score DESC, occurrence_count DESC);
COMMIT;
"#,
    )
    .map_err(|e| CodeGraphError::Storage(e.to_string()))
}

/// Additive v5 migration: give pre-coverage cluster tables the
/// `coverage_json` column. Existing rows keep `'[]'`, which family
/// aggregation reads as "no coverage recorded" and falls back to summed
/// member totals for. No-op on fresh DBs, which get the column from `SCHEMA`.
fn add_cluster_coverage_column(conn: &Connection) -> Result<()> {
    let has_column = conn
        .prepare("SELECT * FROM codegraph_tool_build_clusters LIMIT 0")
        .map_err(|e| CodeGraphError::Storage(e.to_string()))?
        .column_names()
        .contains(&"coverage_json");
    if has_column {
        return Ok(());
    }
    conn.execute(
        "ALTER TABLE codegraph_tool_build_clusters \
         ADD COLUMN coverage_json TEXT NOT NULL DEFAULT '[]'",
        [],
    )
    .map(|_| ())
    .map_err(|e| CodeGraphError::Storage(e.to_string()))
}

fn collect_rows<T, F>(rows: rusqlite::MappedRows<'_, F>) -> Result<Vec<T>>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| CodeGraphError::Storage(e.to_string()))?);
    }
    Ok(out)
}

fn tool_build_cluster_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolBuildCluster> {
    let occurrences_json: String = row.get(12)?;
    let occurrences =
        serde_json::from_str(&occurrences_json).map_err(|error| sqlite_json_error(12, error))?;
    let category_label: String = row.get(13)?;
    let member_ids_json: String = row.get(14)?;
    let member_cluster_ids: Vec<String> =
        serde_json::from_str(&member_ids_json).map_err(|error| sqlite_json_error(14, error))?;
    let reason: Option<String> = row.get(15)?;
    let ignored_by: Option<String> = row.get(16)?;
    let ignored_at: Option<String> = row.get(17)?;
    let coverage_json: String = row.get(18)?;
    let coverage =
        serde_json::from_str(&coverage_json).map_err(|error| sqlite_json_error(18, error))?;
    let cluster_id: String = row.get(0)?;
    let ignored = match (reason, ignored_by, ignored_at) {
        (Some(reason), Some(ignored_by), Some(ignored_at)) => Some(ToolBuildIgnore {
            cluster_id: cluster_id.clone(),
            reason,
            ignored_by,
            ignored_at,
        }),
        _ => None,
    };
    Ok(ToolBuildCluster {
        cluster_id,
        repo_id: row.get(1)?,
        commit_sha: row.get(2)?,
        fingerprint: row.get(3)?,
        score: row.get::<_, i64>(4)? as u64,
        occurrence_count: row.get::<_, i64>(5)? as usize,
        repo_count: row.get::<_, i64>(6)? as usize,
        file_count: row.get::<_, i64>(7)? as usize,
        total_lines: row.get::<_, i64>(8)? as usize,
        language: row.get(9)?,
        insight: row.get(10)?,
        normalized_preview: row.get(11)?,
        category: crate::tool_build::ToolBuildCategory::from_label(&category_label),
        member_cluster_ids,
        coverage,
        occurrences,
        ignored,
    })
}

fn sqlite_json_error(column: usize, error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
}

fn epoch_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
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
}
