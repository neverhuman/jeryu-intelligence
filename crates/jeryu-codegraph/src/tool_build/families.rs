//! Second-tier pattern families: clusters whose anchor signatures overlap are
//! variants of one repeated pattern even when their exact normalized windows
//! differ. Families are a pure function of the cluster list, so they can be
//! recomputed from persisted rows at read time — no second source of truth.

use std::collections::{BTreeMap, BTreeSet};

use super::{ToolBuildCluster, ToolBuildClusterFamily, enrich};

/// Jaccard similarity floor (x100) for grouping two anchor signatures.
const JACCARD_FLOOR_X100: usize = 60;
/// Minimum anchor-signature size before a cluster can group with another.
const MIN_SIGNATURE: usize = 2;

/// Group ranked clusters into pattern families. Grouping never crosses a
/// (language, category) boundary, ordering is deterministic, and singleton
/// clusters still emit a family of one so dashboards have a uniform model.
#[must_use]
pub fn group_pattern_families(clusters: &[ToolBuildCluster]) -> Vec<ToolBuildClusterFamily> {
    // Bucket cluster indexes by (language, category).
    let mut buckets: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (idx, cluster) in clusters.iter().enumerate() {
        buckets
            .entry((
                cluster.language.clone(),
                cluster.category.as_str().to_string(),
            ))
            .or_default()
            .push(idx);
    }

    let signatures: Vec<BTreeSet<String>> = clusters.iter().map(anchor_signature).collect();

    // Union-find with path halving; roots re-canonicalized afterwards to the
    // member with the lexically smallest cluster_id for determinism.
    let mut parent: Vec<usize> = (0..clusters.len()).collect();
    fn find(parent: &mut [usize], mut node: usize) -> usize {
        while parent[node] != node {
            parent[node] = parent[parent[node]];
            node = parent[node];
        }
        node
    }

    for indexes in buckets.values() {
        for (a_pos, &a) in indexes.iter().enumerate() {
            if signatures[a].len() < MIN_SIGNATURE {
                continue;
            }
            for &b in &indexes[a_pos + 1..] {
                if signatures[b].len() < MIN_SIGNATURE {
                    continue;
                }
                let intersection = signatures[a].intersection(&signatures[b]).count();
                let union = signatures[a].len() + signatures[b].len() - intersection;
                if union == 0 || intersection * 100 < JACCARD_FLOOR_X100 * union {
                    continue;
                }
                let root_a = find(&mut parent, a);
                let root_b = find(&mut parent, b);
                if root_a != root_b {
                    parent[root_a.max(root_b)] = root_a.min(root_b);
                }
            }
        }
    }

    // Collect members per root.
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for idx in 0..clusters.len() {
        let root = find(&mut parent, idx);
        groups.entry(root).or_default().push(idx);
    }

    let mut families: Vec<ToolBuildClusterFamily> = Vec::with_capacity(groups.len());
    for members in groups.values() {
        families.push(build_family(clusters, members, &signatures));
    }
    families.sort_by(|a, b| {
        b.anticipated_loc_saved_total
            .cmp(&a.anticipated_loc_saved_total)
            .then_with(|| b.score_total.cmp(&a.score_total))
            .then_with(|| a.family_id.cmp(&b.family_id))
    });
    families
}

/// Distinct spans, files and anticipated LOC saved over a family's unioned
/// coverage. One copy of the duplicated code is retained when a shared tool
/// absorbs the rest, so the largest span never counts as saved.
fn union_totals(
    coverage: &BTreeMap<(String, String), Vec<(usize, usize)>>,
) -> (usize, usize, usize) {
    let mut spans_total = 0;
    let mut lines_total = 0;
    let mut largest_span = 0;
    for spans in coverage.values() {
        for (start, end) in super::scan::merge_spans(spans.clone()) {
            let lines = end.saturating_sub(start) + 1;
            spans_total += 1;
            lines_total += lines;
            largest_span = largest_span.max(lines);
        }
    }
    (
        spans_total,
        coverage.len(),
        lines_total.saturating_sub(largest_span),
    )
}

/// The dedup'd domain `call:`/`macro:`/`member:` tokens of a cluster's preview.
///
/// Standard-library names are excluded: grouping on `iter`/`collect`/`unwrap`
/// would fuse unrelated clusters into one huge "family" of Rust boilerplate.
/// A cluster left with fewer than [`MIN_SIGNATURE`] domain anchors stays a
/// singleton.
fn anchor_signature(cluster: &ToolBuildCluster) -> BTreeSet<String> {
    cluster
        .normalized_preview
        .split_whitespace()
        .filter_map(super::anchors::domain_anchor)
        .map(str::to_string)
        .collect()
}

fn build_family(
    clusters: &[ToolBuildCluster],
    members: &[usize],
    signatures: &[BTreeSet<String>],
) -> ToolBuildClusterFamily {
    let mut cluster_ids: Vec<String> = Vec::with_capacity(members.len());
    let mut repo_ids: BTreeSet<String> = BTreeSet::new();
    let mut union_signature: BTreeSet<String> = BTreeSet::new();
    let mut anchor_frequency: BTreeMap<String, usize> = BTreeMap::new();
    let mut coverage: BTreeMap<(String, String), Vec<(usize, usize)>> = BTreeMap::new();
    let mut every_member_has_coverage = true;
    let mut summed_occurrences = 0;
    let mut summed_files = 0;
    let mut summed_anticipated = 0;
    let mut score_total: u64 = 0;
    let mut language = String::new();
    let mut category = super::ToolBuildCategory::ToolCandidate;

    for &idx in members {
        let cluster = &clusters[idx];
        cluster_ids.push(cluster.cluster_id.clone());
        for occ in &cluster.occurrences {
            repo_ids.insert(occ.repo_id.clone());
        }
        for anchor in &signatures[idx] {
            union_signature.insert(anchor.clone());
            *anchor_frequency.entry(anchor.clone()).or_default() += 1;
        }
        every_member_has_coverage &= !cluster.coverage.is_empty();
        for file in &cluster.coverage {
            repo_ids.insert(file.repo_id.clone());
            coverage
                .entry((file.repo_id.clone(), file.path.clone()))
                .or_default()
                .extend(file.spans.iter().copied());
        }
        summed_occurrences += cluster.occurrence_count;
        summed_files += cluster.file_count;
        summed_anticipated += enrich::anticipated_loc_saved(cluster);
        score_total = score_total.saturating_add(cluster.score);
        language = cluster.language.clone();
        category = cluster.category;
    }
    cluster_ids.sort();

    // Member clusters are variants of one pattern, so they routinely cover the
    // same files and even the same lines. Summing their counts reports those
    // files, occurrences and lines once per variant — which is how a family of
    // 30 window variants over 80 files claimed thousands of "occurrences" and
    // an equal number of "files". Aggregate over the union of the members'
    // coverage instead; fall back to the sums only for rows persisted before
    // coverage was recorded.
    let (occurrence_total, file_total, anticipated_total) = if every_member_has_coverage {
        union_totals(&coverage)
    } else {
        (summed_occurrences, summed_files, summed_anticipated)
    };

    // Label: the three most frequent domain anchors, ties broken lexically.
    let mut ranked: Vec<(&String, &usize)> = anchor_frequency.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let label_anchors: Vec<&str> = ranked
        .iter()
        .take(3)
        .map(|(anchor, _)| anchor.as_str())
        .collect();
    let label = if label_anchors.is_empty() {
        format!("{language} pattern")
    } else {
        label_anchors.join(", ")
    };

    let fingerprint = blake3::hash(
        format!(
            "{language}:{}:{}",
            category.as_str(),
            union_signature
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string();

    ToolBuildClusterFamily {
        family_id: format!("toolfam-{}", &fingerprint[..16]),
        label,
        language,
        category,
        cluster_count: cluster_ids.len(),
        cluster_ids,
        repo_ids: repo_ids.into_iter().collect(),
        occurrence_total,
        file_total,
        anticipated_loc_saved_total: anticipated_total,
        score_total,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        ToolBuildCategory, ToolBuildCluster, ToolBuildFileCoverage, ToolBuildOccurrence,
    };
    use super::*;

    fn cluster(id: &str, language: &str, preview: &str, repos: &[&str]) -> ToolBuildCluster {
        ToolBuildCluster {
            cluster_id: id.to_string(),
            repo_id: "system/host".to_string(),
            commit_sha: "working-tree".to_string(),
            fingerprint: format!("{id}-fp"),
            score: 100,
            occurrence_count: repos.len(),
            repo_count: repos.len(),
            file_count: repos.len(),
            total_lines: 40,
            language: language.to_string(),
            insight: String::new(),
            normalized_preview: preview.to_string(),
            category: ToolBuildCategory::ToolCandidate,
            member_cluster_ids: Vec::new(),
            // Every member covers the SAME file span in each repo: summing
            // member counts would report it once per member.
            coverage: repos
                .iter()
                .map(|repo| ToolBuildFileCoverage {
                    repo_id: (*repo).to_string(),
                    path: "src/lib.rs".to_string(),
                    spans: vec![(1, 10)],
                })
                .collect(),
            occurrences: repos
                .iter()
                .map(|repo| ToolBuildOccurrence {
                    repo_id: (*repo).to_string(),
                    commit_sha: "working-tree".to_string(),
                    path: "src/lib.rs".to_string(),
                    start_line: 1,
                    end_line: 10,
                    language: language.to_string(),
                    normalized_token_count: 40,
                    is_test: false,
                })
                .collect(),
            ignored: None,
        }
    }

    #[test]
    fn overlapping_anchor_signatures_group() {
        let a = cluster(
            "toolbuild-aaa",
            "rust",
            "kw:let id op:= call:retry op:( id op:)\nmember:is_ok call:call_remote",
            &["repo-a", "repo-b"],
        );
        let b = cluster(
            "toolbuild-bbb",
            "rust",
            "kw:let id op:= call:retry op:( lit:num op:)\nmember:is_ok call:call_remote kw:return",
            &["repo-b", "repo-c"],
        );
        let families = group_pattern_families(&[a, b]);
        assert_eq!(families.len(), 1);
        let family = &families[0];
        assert_eq!(family.cluster_count, 2);
        assert_eq!(family.repo_ids, vec!["repo-a", "repo-b", "repo-c"]);
        assert!(family.label.contains("retry"));
    }

    #[test]
    fn family_totals_union_member_coverage_instead_of_summing() {
        // Two window variants of one pattern, covering the SAME two files.
        let a = cluster(
            "toolbuild-aaa",
            "rust",
            "kw:let id op:= call:retry op:( id op:) call:call_remote",
            &["repo-a", "repo-b"],
        );
        let b = cluster(
            "toolbuild-bbb",
            "rust",
            "kw:let id op:= call:retry op:( lit:num op:) call:call_remote",
            &["repo-a", "repo-b"],
        );
        let families = group_pattern_families(&[a, b]);
        assert_eq!(families.len(), 1);
        let family = &families[0];
        // Summing members would claim 4 occurrences over 4 files; the union is
        // two spans in two files.
        assert_eq!(family.occurrence_total, 2);
        assert_eq!(family.file_total, 2);
        // 20 duplicated lines minus the one 10-line copy a tool would retain.
        assert_eq!(family.anticipated_loc_saved_total, 10);
    }

    #[test]
    fn totals_fall_back_to_sums_without_coverage() {
        // Rows persisted before coverage existed still aggregate, by sum.
        let mut a = cluster(
            "toolbuild-aaa",
            "rust",
            "call:retry call:call_remote",
            &["r1", "r2"],
        );
        let mut b = cluster(
            "toolbuild-bbb",
            "rust",
            "call:retry call:call_remote",
            &["r1", "r2"],
        );
        a.coverage.clear();
        b.coverage.clear();
        let families = group_pattern_families(&[a, b]);
        assert_eq!(families.len(), 1);
        assert_eq!(families[0].occurrence_total, 4);
        assert_eq!(families[0].file_total, 4);
    }

    #[test]
    fn stdlib_anchors_never_group_or_name_a_family() {
        // Two unrelated clusters whose only anchors are standard library.
        let a = cluster(
            "toolbuild-aaa",
            "rust",
            "id call:display member:ok call:read_to_string",
            &["repo-a", "repo-b"],
        );
        let b = cluster(
            "toolbuild-bbb",
            "rust",
            "id call:display member:ok call:read_to_string",
            &["repo-c", "repo-d"],
        );
        let families = group_pattern_families(&[a, b]);
        assert_eq!(families.len(), 2, "stdlib plumbing is not a shared pattern");
        for family in &families {
            assert_eq!(family.label, "rust pattern");
        }
    }

    #[test]
    fn languages_never_mix_and_output_is_deterministic() {
        let a = cluster(
            "toolbuild-aaa",
            "rust",
            "call:retry member:is_ok",
            &["r1", "r2"],
        );
        let b = cluster(
            "toolbuild-bbb",
            "typescript",
            "call:retry member:is_ok",
            &["r1", "r2"],
        );
        let forward = group_pattern_families(&[a.clone(), b.clone()]);
        let reversed = group_pattern_families(&[b, a]);
        assert_eq!(forward.len(), 2);
        let forward_ids: Vec<_> = forward.iter().map(|f| f.family_id.clone()).collect();
        let reversed_ids: Vec<_> = reversed.iter().map(|f| f.family_id.clone()).collect();
        assert_eq!(forward_ids, reversed_ids);
    }

    #[test]
    fn tiny_signatures_stay_singletons() {
        let a = cluster("toolbuild-aaa", "rust", "call:retry", &["r1", "r2"]);
        let b = cluster("toolbuild-bbb", "rust", "call:retry", &["r2", "r3"]);
        let families = group_pattern_families(&[a, b]);
        assert_eq!(families.len(), 2);
    }
}
