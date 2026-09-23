//! The shared scan core: parallel per-repo workers fingerprint normalized
//! windows into shards, shards fold into one cross-repo index, survivors are
//! (optionally) overlap-merged, then previews/fingerprints are reconstructed
//! from disk for just the surviving clusters.
//!
//! Memory note: the index stores only compact occurrences (no preview strings)
//! keyed by the first 16 bytes of the window's BLAKE3, so multi-million-window
//! system scans stay in the low hundreds of MB. Previews and full fingerprints
//! are recomputed for the few hundred survivors and verified against the index
//! key, which also guards against files changing mid-scan.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::normalize::{NormalizedLine, normalized_lines};
use super::progress::{ToolBuildScanPhase, ToolBuildScanProgress};
use super::walk::{self, PathClass};
use super::{
    ToolBuildCategory, ToolBuildCluster, ToolBuildFileCoverage, ToolBuildOccurrence,
    ToolBuildScanOptions, ToolBuildScanReport, enrich, epoch_millis, families, merge,
};
use crate::error::{CodeGraphError, Result};

/// Compact occurrence stored in the fingerprint index during scanning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScanOccurrence {
    pub repo_idx: u32,
    /// Index into the (shard-local, later rebased global) file table.
    pub file_idx: u32,
    /// Window start index in the file's normalized-line space. Overlap
    /// merging chains windows in THIS space, where +1 means "the next
    /// normalized line", regardless of interleaved blanks/comments.
    pub norm_start: u32,
    /// 1-based raw start line.
    pub start_line: u32,
    /// 1-based raw end line.
    pub end_line: u32,
    /// Normalized token count of the window.
    pub token_count: u32,
}

/// One scanned file in the global file table.
#[derive(Debug, Clone)]
pub(crate) struct FileEntry {
    pub abs: PathBuf,
    pub rel: String,
    pub language: String,
    pub class: PathClass,
}

/// Occurrence list that avoids a heap allocation for the (dominant) case of a
/// fingerprint seen exactly once.
#[derive(Debug, Clone)]
enum OccList {
    One(ScanOccurrence),
    Many(Vec<ScanOccurrence>),
}

impl OccList {
    fn push(&mut self, occ: ScanOccurrence) {
        match self {
            Self::One(first) => *self = Self::Many(vec![*first, occ]),
            Self::Many(list) => list.push(occ),
        }
    }

    fn extend_from(&mut self, other: OccList) {
        match other {
            OccList::One(occ) => self.push(occ),
            OccList::Many(list) => {
                for occ in list {
                    self.push(occ);
                }
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::One(_) => 1,
            Self::Many(list) => list.len(),
        }
    }

    fn into_vec(self) -> Vec<ScanOccurrence> {
        match self {
            Self::One(occ) => vec![occ],
            Self::Many(list) => list,
        }
    }
}

/// One worker's output for one repo.
struct Shard {
    repo_idx: usize,
    files: Vec<FileEntry>,
    index: HashMap<u128, OccList>,
    scanned: usize,
    skipped: usize,
    error: Option<CodeGraphError>,
}

/// A surviving cluster before preview reconstruction. `norm_len` grows past
/// `window_lines` when overlap merging chains windows together.
#[derive(Debug, Clone)]
pub(crate) struct ProtoCluster {
    /// Index key of the (chain-head) window.
    pub key: u128,
    /// Occurrences; sorted by (repo, file, norm_start) on the v2 path.
    pub occs: Vec<ScanOccurrence>,
    /// Normalized-line length of the (merged) window.
    pub norm_len: usize,
    /// Index keys of chained member windows (empty unless merged).
    pub member_keys: Vec<u128>,
}

/// Shared scan entry point behind every public scan function.
pub(crate) fn scan_roots(
    roots: &[(String, PathBuf)],
    label_repo_id: &str,
    commit_sha: &str,
    options: &ToolBuildScanOptions,
    on_progress: &(dyn Fn(ToolBuildScanProgress) + Send + Sync),
) -> Result<ToolBuildScanReport> {
    let window_lines = options.base.window_lines.max(2);
    let repo_total = roots.len();
    let files_scanned = AtomicUsize::new(0);
    let files_skipped = AtomicUsize::new(0);
    let repos_done = AtomicUsize::new(0);
    let next_repo = AtomicUsize::new(0);
    let shards: Mutex<Vec<Shard>> = Mutex::new(Vec::with_capacity(repo_total));

    let progress =
        |phase: ToolBuildScanPhase, repo_index: usize, current_repo: &str, clusters: usize| {
            on_progress(ToolBuildScanProgress {
                phase,
                repo_index,
                repo_total,
                repos_done: repos_done.load(Ordering::Relaxed),
                current_repo: current_repo.to_string(),
                files_scanned: files_scanned.load(Ordering::Relaxed),
                files_skipped: files_skipped.load(Ordering::Relaxed),
                clusters_so_far: clusters,
            });
        };

    progress(ToolBuildScanPhase::Discover, 0, "", 0);

    let worker_count = if options.threads == 0 {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(4)
    } else {
        options.threads
    }
    .min(repo_total.max(1))
    .max(1);

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let repo_idx = next_repo.fetch_add(1, Ordering::Relaxed);
                    if repo_idx >= repo_total {
                        break;
                    }
                    let (repo_id, root) = &roots[repo_idx];
                    progress(ToolBuildScanPhase::Scan, repo_idx, repo_id, 0);
                    let shard = scan_one_repo(
                        repo_idx,
                        root,
                        options,
                        window_lines,
                        &files_scanned,
                        &files_skipped,
                        &|| progress(ToolBuildScanPhase::Scan, repo_idx, repo_id, 0),
                    );
                    repos_done.fetch_add(1, Ordering::Relaxed);
                    progress(ToolBuildScanPhase::Scan, repo_idx, repo_id, 0);
                    shards.lock().expect("shard mutex poisoned").push(shard);
                }
            });
        }
    });

    let mut shards = shards.into_inner().expect("shard mutex poisoned");
    shards.sort_by_key(|shard| shard.repo_idx);
    // v1 strict IO semantics: surface the first error in repo order.
    for shard in &mut shards {
        if let Some(error) = shard.error.take() {
            return Err(error);
        }
    }

    progress(ToolBuildScanPhase::Merge, repo_total, "", 0);

    // Fold shards into one cross-repo index. Shards are visited in repo order
    // and per-shard occurrence vectors preserve scan order, so each key's
    // occurrence list matches what a sequential scan would have produced.
    let mut file_table: Vec<FileEntry> = Vec::new();
    let mut index: HashMap<u128, OccList> = HashMap::new();
    let mut scanned_files = 0;
    let mut skipped_files = 0;
    for shard in shards {
        let offset = file_table.len() as u32;
        file_table.extend(shard.files);
        scanned_files += shard.scanned;
        skipped_files += shard.skipped;
        for (key, mut occs) in shard.index {
            rebase_file_indexes(&mut occs, offset);
            match index.entry(key) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().extend_from(occs);
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(occs);
                }
            }
        }
    }

    // Survivors: enough occurrences, spanning enough repos.
    let min_occurrences = options.base.min_occurrences.max(2);
    let min_repo_count = options.base.min_repo_count.max(1);
    let mut survivors: Vec<ProtoCluster> = Vec::new();
    for (key, occs) in index {
        if occs.len() < min_occurrences {
            continue;
        }
        let occs = occs.into_vec();
        let distinct_repos = occs
            .iter()
            .map(|occ| occ.repo_idx)
            .collect::<BTreeSet<_>>()
            .len();
        if distinct_repos < min_repo_count {
            continue;
        }
        survivors.push(ProtoCluster {
            key,
            occs,
            norm_len: window_lines,
            member_keys: Vec::new(),
        });
    }
    // Deterministic processing order regardless of hash-map iteration.
    survivors.sort_by_key(|proto| proto.key);

    if options.merge_overlaps {
        for proto in &mut survivors {
            proto
                .occs
                .sort_by_key(|occ| (occ.repo_idx, occ.file_idx, occ.norm_start));
        }
        survivors = merge::merge_overlapping(survivors, window_lines);
    }

    // Reconstruct previews + full fingerprints for survivors only.
    let reconstructed = reconstruct_previews(&survivors, &file_table, window_lines);

    progress(
        ToolBuildScanPhase::Finalize,
        repo_total,
        "",
        survivors.len(),
    );

    let mut clusters: Vec<ToolBuildCluster> = Vec::new();
    for (proto, recon) in survivors.iter().zip(reconstructed) {
        let Some(recon) = recon else {
            // File changed/vanished mid-scan; the window no longer exists.
            continue;
        };
        clusters.push(build_cluster(
            proto,
            recon,
            roots,
            &file_table,
            label_repo_id,
            commit_sha,
            options,
            window_lines,
        ));
    }

    clusters.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.occurrence_count.cmp(&a.occurrence_count))
            .then_with(|| a.cluster_id.cmp(&b.cluster_id))
    });
    clusters.truncate(options.base.max_clusters.max(1));

    let families = if options.compat_v1 {
        Vec::new()
    } else {
        progress(ToolBuildScanPhase::Families, repo_total, "", clusters.len());
        families::group_pattern_families(&clusters)
    };

    progress(ToolBuildScanPhase::Finalize, repo_total, "", clusters.len());

    Ok(ToolBuildScanReport {
        repo_id: label_repo_id.to_string(),
        commit_sha: commit_sha.to_string(),
        root: String::new(), // callers overwrite with their root label
        scanned_at: epoch_millis(),
        scanned_files,
        skipped_files,
        clusters,
        families,
    })
}

fn rebase_file_indexes(occs: &mut OccList, offset: u32) {
    match occs {
        OccList::One(occ) => occ.file_idx += offset,
        OccList::Many(list) => {
            for occ in list {
                occ.file_idx += offset;
            }
        }
    }
}

/// Scan one repo into a shard. Tolerant on the v2 path; v1 IO errors are
/// captured for the coordinator to surface.
fn scan_one_repo(
    repo_idx: usize,
    root: &Path,
    options: &ToolBuildScanOptions,
    window_lines: usize,
    files_scanned: &AtomicUsize,
    files_skipped: &AtomicUsize,
    tick: &dyn Fn(),
) -> Shard {
    let mut shard = Shard {
        repo_idx,
        files: Vec::new(),
        index: HashMap::new(),
        scanned: 0,
        skipped: 0,
        error: None,
    };

    let discovered: Vec<FileEntry> = if options.compat_v1 {
        let mut paths = Vec::new();
        if let Err(error) = walk::collect_source_files_v1(root, root, &mut paths) {
            shard.error = Some(error);
            return shard;
        }
        paths.sort();
        paths
            .into_iter()
            .map(|abs| FileEntry {
                rel: walk::repo_relative(root, &abs),
                language: walk::language_for_path(&abs),
                class: PathClass::Source,
                abs,
            })
            .collect()
    } else {
        let (files, dropped) = walk::collect_repo_files(root, options);
        if dropped > 0 {
            shard.skipped += dropped;
            files_skipped.fetch_add(dropped, Ordering::Relaxed);
        }
        files
            .into_iter()
            .map(|file| FileEntry {
                abs: file.abs,
                rel: file.rel,
                language: file.language.to_string(),
                class: file.class,
            })
            .collect()
    };

    for file in discovered {
        match std::fs::metadata(&file.abs) {
            Ok(metadata) => {
                if metadata.len() > options.base.max_file_bytes {
                    shard.skipped += 1;
                    files_skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            Err(source) => {
                if options.compat_v1 {
                    shard.error = Some(CodeGraphError::Index {
                        path: file.abs.display().to_string(),
                        source,
                    });
                    return shard;
                }
                shard.skipped += 1;
                files_skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }
        let Ok(contents) = std::fs::read_to_string(&file.abs) else {
            shard.skipped += 1;
            files_skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        shard.scanned += 1;
        let total_scanned = files_scanned.fetch_add(1, Ordering::Relaxed) + 1;
        if total_scanned.is_multiple_of(64) {
            tick();
        }

        let file_idx = shard.files.len() as u32;
        let is_config = walk::is_config_language(&file.language);
        // Shell and config tokens collapse to mostly id/lit (no parens on
        // command invocations, key=value lines), so the anchor and diversity
        // floors would silently erase the managed-scaffold/config lanes they
        // exist to surface. Those languages rely on the token floor alone
        // (raised for config).
        let waive_structure = is_config || file.language == "shell";
        let normalized = normalized_lines(&contents);
        shard.files.push(file);
        if normalized.len() < window_lines {
            continue;
        }
        scan_windows(
            &normalized,
            repo_idx as u32,
            file_idx,
            is_config,
            waive_structure,
            window_lines,
            options,
            &mut shard.index,
        );
    }
    shard
}

/// Slide the window over one file's normalized lines, filter, hash, record.
#[allow(clippy::too_many_arguments)]
fn scan_windows(
    normalized: &[NormalizedLine],
    repo_idx: u32,
    file_idx: u32,
    is_config: bool,
    waive_structure: bool,
    window_lines: usize,
    options: &ToolBuildScanOptions,
    index: &mut HashMap<u128, OccList>,
) {
    let mut distinct_scratch: HashSet<&str> = HashSet::new();
    // Prefix sums make per-window token/anchor/import counts O(1).
    let count = normalized.len();
    let mut token_prefix = Vec::with_capacity(count + 1);
    let mut anchor_prefix = Vec::with_capacity(count + 1);
    let mut import_prefix = Vec::with_capacity(count + 1);
    token_prefix.push(0usize);
    anchor_prefix.push(0usize);
    import_prefix.push(0usize);
    for line in normalized {
        token_prefix.push(token_prefix.last().unwrap() + line.token_count);
        anchor_prefix.push(anchor_prefix.last().unwrap() + line.anchor_count);
        import_prefix.push(import_prefix.last().unwrap() + usize::from(line.is_import));
    }

    let min_tokens = if !options.compat_v1 && is_config {
        // Config repetition needs a meaningfully higher bar before it counts.
        options.base.min_normalized_tokens + options.base.min_normalized_tokens / 2
    } else {
        options.base.min_normalized_tokens
    };

    for start in 0..=(count - window_lines) {
        let end = start + window_lines;
        let token_count = token_prefix[end] - token_prefix[start];
        if token_count < min_tokens {
            continue;
        }
        if !options.compat_v1 {
            let import_lines = import_prefix[end] - import_prefix[start];
            if import_lines * 100 > options.max_import_fraction_x100 * window_lines {
                continue;
            }
            if !waive_structure {
                let anchors = anchor_prefix[end] - anchor_prefix[start];
                if anchors < options.min_anchor_tokens {
                    continue;
                }
                if options.min_distinct_tokens > 0 {
                    distinct_scratch.clear();
                    for line in &normalized[start..end] {
                        for token in line.joined.split_whitespace() {
                            distinct_scratch.insert(token);
                        }
                    }
                    if distinct_scratch.len() < options.min_distinct_tokens {
                        continue;
                    }
                }
            }
        }

        let key = window_key(&normalized[start..end]);
        let occ = ScanOccurrence {
            repo_idx,
            file_idx,
            norm_start: start as u32,
            start_line: normalized[start].line_number as u32,
            end_line: normalized[end - 1].line_number as u32,
            token_count: token_count as u32,
        };
        match index.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => entry.get_mut().push(occ),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(OccList::One(occ));
            }
        }
    }
}

/// First 16 bytes of the v1-identical window hash. The hash feeds each line's
/// pre-joined token string with `\n` separators — byte-identical input to
/// v1's `lines.join("\n")`, with zero per-window string allocation.
pub(crate) fn window_key(window: &[NormalizedLine]) -> u128 {
    let mut hasher = blake3::Hasher::new();
    for (i, line) in window.iter().enumerate() {
        if i > 0 {
            hasher.update(b"\n");
        }
        hasher.update(line.joined.as_bytes());
    }
    let bytes = hasher.finalize();
    u128::from_le_bytes(
        bytes.as_bytes()[..16]
            .try_into()
            .expect("blake3 is 32 bytes"),
    )
}

/// The reconstructed identity of one surviving cluster.
pub(crate) struct ReconstructedWindow {
    pub preview: String,
    pub fingerprint_hex: String,
    pub token_count: usize,
}

/// Re-read just the survivors' head files and rebuild preview text + full
/// fingerprints. Returns `None` for a cluster whose head window no longer
/// hashes to its index key (file changed mid-scan).
fn reconstruct_previews(
    survivors: &[ProtoCluster],
    file_table: &[FileEntry],
    window_lines: usize,
) -> Vec<Option<ReconstructedWindow>> {
    // Group head-window requests by file so each file is read once.
    let mut by_file: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (idx, proto) in survivors.iter().enumerate() {
        if let Some(head) = proto.occs.first() {
            by_file.entry(head.file_idx).or_default().push(idx);
        }
    }
    let mut out: Vec<Option<ReconstructedWindow>> = (0..survivors.len()).map(|_| None).collect();
    for (file_idx, proto_indexes) in by_file {
        let Some(file) = file_table.get(file_idx as usize) else {
            continue;
        };
        let Ok(contents) = std::fs::read_to_string(&file.abs) else {
            continue;
        };
        let normalized = normalized_lines(&contents);
        for proto_idx in proto_indexes {
            let proto = &survivors[proto_idx];
            let head = proto.occs[0];
            let start = head.norm_start as usize;
            let end = start + proto.norm_len;
            if end > normalized.len() {
                continue;
            }
            // Verify the head window still hashes to the index key.
            let head_end = start + window_lines;
            if head_end > normalized.len() || window_key(&normalized[start..head_end]) != proto.key
            {
                continue;
            }
            let lines: Vec<&str> = normalized[start..end]
                .iter()
                .map(|line| line.joined.as_str())
                .collect();
            let preview = lines.join("\n");
            let fingerprint_hex = blake3::hash(preview.as_bytes()).to_hex().to_string();
            let token_count = normalized[start..end]
                .iter()
                .map(|line| line.token_count)
                .sum();
            out[proto_idx] = Some(ReconstructedWindow {
                preview,
                fingerprint_hex,
                token_count,
            });
        }
    }
    out
}

/// Everything the v2 ranking reads about one cluster.
pub(crate) struct ClusterScoreInput<'a> {
    pub occurrence_count: usize,
    pub repo_count: usize,
    /// Normalized tokens, summed over occurrences.
    pub token_total: usize,
    /// Normalized preview of the window, mined for domain anchors.
    pub preview: &'a str,
}

/// Cross-repo spread is worth this many percent per repo past the first.
const REPO_SPREAD_BONUS_PCT: u64 = 25;
/// Spread stops paying above this many extra repos.
const MAX_REPO_SPREAD_STEPS: u64 = 8;
/// A window with no domain anchor at all keeps this percent of its score.
const NO_ANCHOR_PCT: u64 = 25;
/// Each distinct domain anchor adds this percent, up to full score.
const ANCHOR_STEP_PCT: u64 = 25;

/// The frozen v1 ranking: repetition mass, files, lines. `token_total` already
/// carries one window's tokens per occurrence, so this squares the occurrence
/// count. Persisted v1 report bytes pin it; only the v1 path may use it.
fn v1_score(
    occurrence_count: usize,
    token_total: usize,
    file_count: usize,
    total_lines: usize,
) -> u64 {
    (occurrence_count as u64)
        .saturating_mul(token_total as u64)
        .saturating_add((file_count as u64).saturating_mul(100))
        .saturating_add(total_lines as u64)
}

/// Rank a v2 cluster by the duplication a shared tool would actually remove.
///
/// The v1 shape squares the occurrence count, so a short window repeated
/// dozens of times inside one repo outranks a substantial helper duplicated
/// across repos — which is how standard-library plumbing came to fill the top
/// of the Intelligence and Shared tools pages. v2 instead scores one copy's
/// token weight times the copies past the first (what extraction deletes),
/// then weights that by how far the repetition spreads across repos and by how
/// many distinct domain anchors the window carries. A window anchored only on
/// `iter`/`collect`/`to_string` names the language, not a shared tool, so it
/// keeps a quarter of its score and sinks below code that names a domain.
pub(crate) fn cluster_score(input: &ClusterScoreInput<'_>) -> u64 {
    let occurrences = input.occurrence_count.max(1) as u64;
    let one_copy_tokens = (input.token_total as u64) / occurrences;
    // The v1 file/line tails are deliberately gone: at 100 points per file
    // they put a cluster's raw file count back on top of the ranking, which is
    // the bias this scoring exists to remove. File and line spread stay in the
    // report for the reader; ranking is about removable tokens.
    let base = one_copy_tokens.saturating_mul(occurrences - 1);

    let extra_repos = (input.repo_count.max(1) as u64 - 1).min(MAX_REPO_SPREAD_STEPS);
    let repo_pct = 100 + REPO_SPREAD_BONUS_PCT.saturating_mul(extra_repos);

    let anchors = distinct_domain_anchors(input.preview) as u64;
    let anchor_pct = (NO_ANCHOR_PCT + ANCHOR_STEP_PCT.saturating_mul(anchors)).min(100);

    base.saturating_mul(repo_pct)
        .saturating_mul(anchor_pct)
        .saturating_div(10_000)
}

/// How many distinct domain-meaningful anchor names the preview carries.
fn distinct_domain_anchors(preview: &str) -> usize {
    preview
        .split_whitespace()
        .filter_map(super::anchors::domain_anchor)
        .collect::<BTreeSet<_>>()
        .len()
}

#[allow(clippy::too_many_arguments)]
fn build_cluster(
    proto: &ProtoCluster,
    recon: ReconstructedWindow,
    roots: &[(String, PathBuf)],
    file_table: &[FileEntry],
    label_repo_id: &str,
    commit_sha: &str,
    options: &ToolBuildScanOptions,
    window_lines: usize,
) -> ToolBuildCluster {
    let merged = !proto.member_keys.is_empty();
    let occurrence_count = proto.occs.len();

    let mut repos: BTreeSet<&str> = BTreeSet::new();
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut languages: BTreeMap<&str, usize> = BTreeMap::new();
    let mut total_lines = 0usize;
    let mut token_total = 0usize;
    let mut test_occs = 0usize;
    let mut scaffold_occs = 0usize;
    let mut config_occs = 0usize;
    for occ in &proto.occs {
        let file = &file_table[occ.file_idx as usize];
        repos.insert(roots[occ.repo_idx as usize].0.as_str());
        if options.compat_v1 {
            files.insert(file.rel.clone());
            total_lines += window_lines;
            token_total += occ.token_count as usize;
        } else {
            files.insert(format!("{}:{}", roots[occ.repo_idx as usize].0, file.rel));
            total_lines += (occ.end_line - occ.start_line + 1) as usize;
            token_total += if merged {
                recon.token_count
            } else {
                occ.token_count as usize
            };
        }
        *languages.entry(file.language.as_str()).or_default() += 1;
        match file.class {
            PathClass::Test => test_occs += 1,
            PathClass::ManagedScaffold => scaffold_occs += 1,
            PathClass::Config => config_occs += 1,
            _ => {}
        }
    }

    let language = languages
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(language, _)| (*language).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let file_count = files.len();

    let mut score = if options.compat_v1 {
        v1_score(occurrence_count, token_total, file_count, total_lines)
    } else {
        cluster_score(&ClusterScoreInput {
            occurrence_count,
            repo_count: repos.len().max(1),
            token_total,
            preview: &recon.preview,
        })
    };

    let category = if options.compat_v1 {
        ToolBuildCategory::ToolCandidate
    } else if scaffold_occs * 100 >= options.scaffold_fraction_threshold_x100 * occurrence_count {
        ToolBuildCategory::ManagedScaffold
    } else if config_occs * 100 >= options.scaffold_fraction_threshold_x100 * occurrence_count {
        ToolBuildCategory::ConfigPattern
    } else if test_occs * 100 >= options.test_fraction_threshold_x100 * occurrence_count {
        score /= 4;
        ToolBuildCategory::TestPattern
    } else {
        ToolBuildCategory::ToolCandidate
    };

    let insight = enrich::cluster_insight(
        occurrence_count,
        file_count,
        total_lines,
        &language,
        &recon.preview,
    );

    // v1 keeps the first 12 occurrences (byte parity). v2 caps per repo so
    // every spanning repo stays visible in the compact list even when one
    // repo carries dozens of occurrences.
    let kept: Vec<&ScanOccurrence> = if options.compat_v1 {
        proto.occs.iter().take(12).collect()
    } else {
        // Every repo's first occurrence is always kept; second occurrences
        // fill the remainder up to 64, preserving scan order.
        let mut seen_repos: BTreeSet<u32> = BTreeSet::new();
        let mut kept: Vec<&ScanOccurrence> = proto
            .occs
            .iter()
            .filter(|occ| seen_repos.insert(occ.repo_idx))
            .collect();
        let mut per_repo: BTreeMap<u32, usize> = BTreeMap::new();
        for occ in &proto.occs {
            if kept.len() >= 64 {
                break;
            }
            let extras = per_repo.entry(occ.repo_idx).or_default();
            *extras += 1;
            if *extras == 2 {
                kept.push(occ);
            }
        }
        kept.sort_by_key(|occ| (occ.repo_idx, occ.file_idx, occ.norm_start));
        kept
    };
    let occurrences: Vec<ToolBuildOccurrence> = kept
        .into_iter()
        .map(|occ| {
            let file = &file_table[occ.file_idx as usize];
            ToolBuildOccurrence {
                repo_id: roots[occ.repo_idx as usize].0.clone(),
                commit_sha: commit_sha.to_string(),
                path: file.rel.clone(),
                start_line: occ.start_line as usize,
                end_line: occ.end_line as usize,
                language: file.language.clone(),
                normalized_token_count: if merged {
                    recon.token_count
                } else {
                    occ.token_count as usize
                },
                is_test: file.class == PathClass::Test,
            }
        })
        .collect();

    // Full, uncapped per-file coverage: what families aggregate over. The v1
    // path leaves it empty so v1 report bytes stay unchanged.
    let coverage = if options.compat_v1 {
        Vec::new()
    } else {
        file_coverage(&proto.occs, roots, file_table)
    };

    ToolBuildCluster {
        cluster_id: format!("toolbuild-{}", &recon.fingerprint_hex[..16]),
        repo_id: label_repo_id.to_string(),
        commit_sha: commit_sha.to_string(),
        fingerprint: recon.fingerprint_hex,
        score,
        occurrence_count,
        repo_count: repos.len().max(1),
        file_count,
        total_lines,
        language,
        insight,
        normalized_preview: recon.preview,
        category,
        coverage,
        member_cluster_ids: proto
            .member_keys
            .iter()
            .map(|key| format!("toolbuild-{}", key_hex_prefix(*key)))
            .collect(),
        occurrences,
        ignored: None,
    }
}

/// Group every occurrence by file and merge its spans into non-overlapping
/// inclusive ranges, so a file is one entry and a line is counted once.
fn file_coverage(
    occs: &[ScanOccurrence],
    roots: &[(String, PathBuf)],
    file_table: &[FileEntry],
) -> Vec<ToolBuildFileCoverage> {
    let mut by_file: BTreeMap<(&str, &str), Vec<(usize, usize)>> = BTreeMap::new();
    for occ in occs {
        let file = &file_table[occ.file_idx as usize];
        by_file
            .entry((roots[occ.repo_idx as usize].0.as_str(), file.rel.as_str()))
            .or_default()
            .push((occ.start_line as usize, occ.end_line as usize));
    }
    by_file
        .into_iter()
        .map(|((repo_id, path), spans)| ToolBuildFileCoverage {
            repo_id: repo_id.to_string(),
            path: path.to_string(),
            spans: merge_spans(spans),
        })
        .collect()
}

/// Sort and coalesce inclusive line spans that overlap or touch.
pub(crate) fn merge_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The first 16 hex chars of the full fingerprint, recovered from the 16-byte
/// index key (which holds the hash's leading bytes).
pub(crate) fn key_hex_prefix(key: u128) -> String {
    let bytes = key.to_le_bytes();
    let mut out = String::with_capacity(16);
    for byte in &bytes[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolBuildScanConfig;

    /// A window whose anchors are all standard-library plumbing.
    const PLUMBING: &str = "call:iter call:collect\nmember:to_string call:unwrap";
    /// A window that names a domain.
    const DOMAIN: &str = "call:issue_token member:checksum\ncall:audit_log call:notify_subscribers";

    fn score(occurrences: usize, repos: usize, tokens_per_copy: usize, preview: &str) -> u64 {
        cluster_score(&ClusterScoreInput {
            occurrence_count: occurrences,
            repo_count: repos,
            token_total: occurrences * tokens_per_copy,
            preview,
        })
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("codegraph-scan-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write");
    }

    /// Dense duplicated body: clears the token, anchor and diversity floors.
    const HANDLER: &str = r#"pub fn alpha_handler(req: Request, ctx: &Context) -> Response {
    let parsed = validate_input(req.body(), ctx.schema(), MAX_BYTES).expect("validated");
    let token = ctx.auth().issue_token(parsed.user_id(), Scope::ReadWrite, EXPIRY_SECS);
    let record = Record::new(parsed.id(), parsed.payload(), token.claims(), now_ms());
    audit_log(ctx.logger(), "create", record.id(), record.actor(), record.checksum());
    let stored = ctx.store().insert(record.clone(), WriteMode::Durable).map_err(wrap_err)?;
    notify_subscribers(ctx.bus(), Topic::Created, stored.id(), stored.version());
    metrics_incr(ctx.metrics(), "records_created_total", 1, &[("kind", "create")]);
    Response::created(stored.id(), stored.version(), etag_for(stored.checksum()))
}
"#;

    #[test]
    fn spans_coalesce_when_they_overlap_or_touch() {
        assert_eq!(
            merge_spans(vec![(10, 18), (11, 19), (30, 38), (19, 25)]),
            vec![(10, 25), (30, 38)]
        );
        // Adjacent spans (18 then 19) are one duplicated run, not two.
        assert_eq!(merge_spans(vec![(19, 20), (10, 18)]), vec![(10, 20)]);
        assert_eq!(merge_spans(vec![]), vec![]);
    }

    #[test]
    fn v1_score_shape_is_pinned() {
        // occurrences * token_total + files * 100 + lines: the arithmetic the
        // persisted v1 cluster scores were produced with.
        assert_eq!(v1_score(2, 302, 2, 16), 2 * 302 + 200 + 16);
        assert_eq!(v1_score(0, 0, 0, 0), 0);
    }

    #[test]
    fn a_shared_helper_outranks_plumbing_repeated_far_more_often() {
        // The regression: `occurrences * token_total` squares the occurrence
        // count, so 30 copies of a short standard-library window buried a
        // two-repo helper carrying five times the tokens per copy.
        let plumbing = score(30, 1, 40, PLUMBING);
        let helper = score(3, 3, 200, DOMAIN);
        assert!(
            helper > plumbing,
            "helper {helper} must outrank plumbing {plumbing}"
        );
        assert!(v1_score(30, 30 * 40, 30, 240) > v1_score(3, 3 * 200, 3, 24));
    }

    #[test]
    fn domain_anchors_lift_a_window_above_language_plumbing() {
        let plumbing = score(4, 2, 100, PLUMBING);
        let domain = score(4, 2, 100, DOMAIN);
        assert!(domain > plumbing, "{domain} must beat {plumbing}");
        // Plumbing keeps exactly a quarter; three or more domain anchors pay
        // full score, so the weight never inflates past the base.
        assert_eq!(plumbing, domain * NO_ANCHOR_PCT / 100);
        assert_eq!(
            score(4, 2, 100, "call:issue_token call:audit_log member:checksum"),
            domain
        );
    }

    #[test]
    fn cross_repo_spread_outranks_the_same_mass_in_one_repo() {
        let one_repo = score(4, 1, 100, DOMAIN);
        let four_repos = score(4, 4, 100, DOMAIN);
        assert!(four_repos > one_repo);
        // 25% per extra repo, and the bonus stops after eight extra repos.
        assert_eq!(four_repos, one_repo * 175 / 100);
        assert_eq!(score(4, 20, 100, DOMAIN), score(4, 9, 100, DOMAIN));
    }

    #[test]
    fn a_single_occurrence_removes_no_duplication() {
        // Nothing to extract, so nothing to rank: a lone window scores zero.
        let solo = cluster_score(&ClusterScoreInput {
            occurrence_count: 1,
            repo_count: 1,
            token_total: 200,
            preview: DOMAIN,
        });
        assert_eq!(solo, 0);
    }

    #[test]
    fn scoring_saturates_instead_of_overflowing() {
        let huge = cluster_score(&ClusterScoreInput {
            occurrence_count: usize::MAX,
            repo_count: usize::MAX,
            token_total: usize::MAX,
            preview: DOMAIN,
        });
        assert!(huge > 0);
    }

    #[test]
    fn distinct_domain_anchors_ignores_plumbing_and_repeats() {
        assert_eq!(distinct_domain_anchors(PLUMBING), 0);
        assert_eq!(distinct_domain_anchors("call:retry call:retry id kw:if"), 1);
        assert_eq!(distinct_domain_anchors(""), 0);
    }

    #[test]
    fn occurrence_lists_grow_from_one_to_many() {
        let occ = |norm_start| ScanOccurrence {
            repo_idx: 0,
            file_idx: 0,
            norm_start,
            start_line: norm_start + 1,
            end_line: norm_start + 8,
            token_count: 40,
        };
        let mut list = OccList::One(occ(0));
        assert_eq!(list.len(), 1);
        list.push(occ(1));
        list.extend_from(OccList::One(occ(2)));
        list.extend_from(OccList::Many(vec![occ(3), occ(4)]));
        assert_eq!(list.len(), 5);
        let starts: Vec<u32> = list.into_vec().iter().map(|o| o.norm_start).collect();
        assert_eq!(starts, vec![0, 1, 2, 3, 4], "scan order is preserved");
    }

    #[test]
    fn rebasing_shifts_every_file_index_by_the_shard_offset() {
        let occ = ScanOccurrence {
            repo_idx: 1,
            file_idx: 2,
            norm_start: 0,
            start_line: 1,
            end_line: 8,
            token_count: 40,
        };
        let mut one = OccList::One(occ);
        rebase_file_indexes(&mut one, 10);
        assert_eq!(one.into_vec()[0].file_idx, 12);
        let mut many = OccList::Many(vec![occ, occ]);
        rebase_file_indexes(&mut many, 5);
        assert!(many.into_vec().iter().all(|o| o.file_idx == 7));
    }

    #[test]
    fn window_keys_depend_on_line_content_and_line_boundaries() {
        let a = normalized_lines("let alpha = beta(gamma);\nlet delta = epsilon(zeta);\n");
        let same = normalized_lines("let alpha = beta(gamma);\nlet delta = epsilon(zeta);\n");
        let swapped = normalized_lines("let delta = epsilon(zeta);\nlet alpha = beta(gamma);\n");
        assert_eq!(window_key(&a), window_key(&same));
        assert_ne!(window_key(&a), window_key(&swapped));
        // Blank and comment lines never reach the hash, so an interleaved
        // comment cannot change a window's identity.
        let interleaved =
            normalized_lines("let alpha = beta(gamma);\n\n// note\nlet delta = epsilon(zeta);\n");
        assert_eq!(window_key(&a), window_key(&interleaved));
    }

    #[test]
    fn the_index_key_recovers_the_fingerprints_leading_hex() {
        let lines = normalized_lines(HANDLER);
        let key = window_key(&lines);
        let preview: Vec<&str> = lines.iter().map(|line| line.joined.as_str()).collect();
        let fingerprint = blake3::hash(preview.join("\n").as_bytes())
            .to_hex()
            .to_string();
        assert_eq!(key_hex_prefix(key), fingerprint[..16]);
    }

    #[test]
    fn window_filters_drop_thin_boilerplate_and_import_blocks() {
        let options = ToolBuildScanOptions::system_default();
        let window_lines = 4;
        let indexed = |source: &str, options: &ToolBuildScanOptions| {
            let normalized = normalized_lines(source);
            let mut index = HashMap::new();
            scan_windows(
                &normalized,
                0,
                0,
                false,
                false,
                window_lines,
                options,
                &mut index,
            );
            index.len()
        };

        // Braces and one-token lines never clear the token floor.
        assert_eq!(indexed("{\n}\n{\n}\n{\n}\n", &options), 0);

        let imports = "use alpha::beta::gamma;\nuse delta::epsilon::zeta;\nuse eta::theta::iota;\nuse kappa::lambda::mu;\n";
        assert_eq!(indexed(imports, &options), 0, "import blocks are not tools");

        // A dense domain body clears every floor.
        assert!(indexed(HANDLER, &options) > 0);

        // Waiving the anchor floor is what keeps the shell/config lanes alive:
        // the same body still indexes when structure is required, and a
        // plumbing-only body only indexes once structure is waived.
        let plumbing = "let a = values.iter().copied().collect::<Vec<_>>();\nlet b = names.iter().map(|n| n.to_string()).collect::<Vec<_>>();\nlet c = b.join(\", \").trim().to_owned();\nlet d = c.parse::<usize>().unwrap_or_default();\n";
        assert_eq!(indexed(plumbing, &options), 0);
        let waived = {
            let normalized = normalized_lines(plumbing);
            let mut index = HashMap::new();
            scan_windows(
                &normalized,
                0,
                0,
                false,
                true,
                window_lines,
                &options,
                &mut index,
            );
            index.len()
        };
        assert!(
            waived > 0,
            "shell/config lanes rely on the token floor alone"
        );
    }

    #[test]
    fn config_windows_face_a_higher_token_floor() {
        let mut options = ToolBuildScanOptions::system_default();
        let source = "alpha = 1\nbeta = 2\ngamma = 3\ndelta = 4\n";
        let normalized = normalized_lines(source);
        // Sit the plain floor exactly on the window: the config floor adds
        // half again on top, so only the config lane rejects it.
        options.base.min_normalized_tokens = normalized.iter().map(|line| line.token_count).sum();
        let count = |is_config: bool| {
            let mut index = HashMap::new();
            scan_windows(&normalized, 0, 0, is_config, true, 4, &options, &mut index);
            index.len()
        };
        assert!(count(false) > 0, "the plain floor admits this window");
        assert_eq!(count(true), 0, "config repetition needs a higher bar");
    }

    #[test]
    fn coverage_is_one_entry_per_file_with_coalesced_spans() {
        let roots = vec![
            ("repo-a".to_string(), PathBuf::from("/a")),
            ("repo-b".to_string(), PathBuf::from("/b")),
        ];
        let file = |rel: &str| FileEntry {
            abs: PathBuf::from(rel),
            rel: rel.to_string(),
            language: "rust".to_string(),
            class: PathClass::Source,
        };
        let file_table = vec![file("src/one.rs"), file("src/two.rs")];
        let occ = |repo_idx, file_idx, start_line, end_line| ScanOccurrence {
            repo_idx,
            file_idx,
            norm_start: 0,
            start_line,
            end_line,
            token_count: 40,
        };
        let coverage = file_coverage(
            &[
                occ(0, 0, 10, 17),
                occ(0, 0, 14, 21),
                occ(0, 0, 40, 47),
                occ(1, 1, 5, 12),
            ],
            &roots,
            &file_table,
        );
        assert_eq!(coverage.len(), 2);
        assert_eq!(coverage[0].repo_id, "repo-a");
        assert_eq!(coverage[0].path, "src/one.rs");
        assert_eq!(coverage[0].spans, vec![(10, 21), (40, 47)]);
        assert_eq!(coverage[1].repo_id, "repo-b");
        assert_eq!(coverage[1].spans, vec![(5, 12)]);
    }

    #[test]
    fn reconstruction_drops_a_window_whose_file_changed_mid_scan() {
        let root = tmp_dir("reconstruct");
        write(&root, "src/lib.rs", HANDLER);
        let abs = root.join("src/lib.rs");
        let normalized = normalized_lines(HANDLER);
        let window_lines = 4;
        let key = window_key(&normalized[..window_lines]);
        let file_table = vec![FileEntry {
            abs: abs.clone(),
            rel: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            class: PathClass::Source,
        }];
        let proto = ProtoCluster {
            key,
            occs: vec![ScanOccurrence {
                repo_idx: 0,
                file_idx: 0,
                norm_start: 0,
                start_line: 1,
                end_line: 4,
                token_count: 40,
            }],
            norm_len: window_lines,
            member_keys: Vec::new(),
        };
        let recon = reconstruct_previews(std::slice::from_ref(&proto), &file_table, window_lines);
        let recovered = recon[0].as_ref().expect("window still on disk");
        assert_eq!(recovered.fingerprint_hex[..16], key_hex_prefix(key));
        assert!(recovered.token_count > 0);
        assert_eq!(recovered.preview.lines().count(), window_lines);

        // Rewrite the file: the head window no longer hashes to the key.
        std::fs::write(&abs, HANDLER.replace("alpha_handler", "gamma_handler")).expect("rewrite");
        let stale = reconstruct_previews(&[proto], &file_table, window_lines);
        assert!(stale[0].is_none(), "a changed file yields no preview");

        // A vanished file is dropped the same way, never a panic.
        std::fs::remove_file(&abs).expect("remove");
        let gone = reconstruct_previews(
            &[ProtoCluster {
                key,
                occs: vec![ScanOccurrence {
                    repo_idx: 0,
                    file_idx: 0,
                    norm_start: 0,
                    start_line: 1,
                    end_line: 4,
                    token_count: 40,
                }],
                norm_len: window_lines,
                member_keys: Vec::new(),
            }],
            &file_table,
            window_lines,
        );
        assert!(gone[0].is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scanning_two_repos_yields_one_cross_repo_cluster() {
        let root = tmp_dir("scan-roots");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        write(&repo_a, "src/lib.rs", HANDLER);
        write(&repo_b, "src/service.rs", HANDLER);
        // Repeated only inside repo-a: it must not survive min_repo_count = 2.
        write(&repo_a, "src/copy.rs", HANDLER);

        let roots = vec![
            ("repo-a".to_string(), repo_a.clone()),
            ("repo-b".to_string(), repo_b.clone()),
        ];
        let mut options = ToolBuildScanOptions::system_default();
        options.threads = 1;
        options.use_git_ls_files = false;
        options.base.window_lines = 8;

        let phases = Mutex::new(Vec::new());
        let report = scan_roots(
            &roots,
            "system/host",
            "working-tree",
            &options,
            &|progress| {
                phases.lock().expect("phases").push(progress.phase);
            },
        )
        .expect("scan");

        assert_eq!(report.repo_id, "system/host");
        assert_eq!(report.commit_sha, "working-tree");
        assert_eq!(report.scanned_files, 3);
        assert!(!report.clusters.is_empty());
        for cluster in &report.clusters {
            assert_eq!(cluster.repo_count, 2, "min_repo_count = 2 is enforced");
            assert!(cluster.occurrence_count >= 3);
            assert_eq!(cluster.language, "rust");
            assert_eq!(cluster.category, ToolBuildCategory::ToolCandidate);
        }
        // Ranked by score, highest first.
        let scores: Vec<u64> = report.clusters.iter().map(|c| c.score).collect();
        let mut sorted = scores.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(scores, sorted);

        let phases = phases.into_inner().expect("phases");
        assert_eq!(phases.first(), Some(&ToolBuildScanPhase::Discover));
        assert_eq!(phases.last(), Some(&ToolBuildScanPhase::Finalize));

        // Thread count never changes the output.
        options.threads = 4;
        let parallel =
            scan_roots(&roots, "system/host", "working-tree", &options, &|_| {}).expect("scan");
        let ids = |report: &ToolBuildScanReport| -> Vec<String> {
            report
                .clusters
                .iter()
                .map(|cluster| cluster.cluster_id.clone())
                .collect()
        };
        assert_eq!(ids(&report), ids(&parallel));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_v1_path_surfaces_the_first_io_error_in_repo_order() {
        let root = tmp_dir("scan-v1-missing");
        let present = root.join("present");
        write(&present, "src/lib.rs", HANDLER);
        let roots = vec![
            ("present".to_string(), present.clone()),
            ("missing".to_string(), root.join("missing")),
        ];
        let options = ToolBuildScanOptions::v1_compat(ToolBuildScanConfig::default());
        let error = scan_roots(&roots, "parity", "c1", &options, &|_| {})
            .expect_err("a missing root is an error on the v1 path");
        assert!(matches!(error, CodeGraphError::Index { .. }), "{error:?}");

        // The v2 path counts the same root as skipped and keeps going.
        let mut tolerant = ToolBuildScanOptions::system_default();
        tolerant.threads = 1;
        tolerant.use_git_ls_files = false;
        let report = scan_roots(&roots, "system/host", "working-tree", &tolerant, &|_| {})
            .expect("v2 tolerates an unreadable root");
        assert_eq!(report.scanned_files, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn oversized_files_are_counted_as_skipped_not_scanned() {
        let root = tmp_dir("scan-oversize");
        let repo = root.join("repo-a");
        write(&repo, "src/lib.rs", HANDLER);
        let mut options = ToolBuildScanOptions::system_default();
        options.threads = 1;
        options.use_git_ls_files = false;
        options.base.max_file_bytes = 8;
        let report = scan_roots(
            &[("repo-a".to_string(), repo)],
            "system/host",
            "working-tree",
            &options,
            &|_| {},
        )
        .expect("scan");
        assert_eq!(report.scanned_files, 0);
        assert_eq!(report.skipped_files, 1);
        assert!(report.clusters.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
