use super::*;
use crate::test_support::{PackBuilder, receipt};
use crate::types::*;

fn pack_with_security(sast: ScanOutcome, dep: ScanOutcome, sec: ScanOutcome) -> EvidencePack {
    PackBuilder::new().security(sast, dep, sec).build()
}

fn clean_pack() -> EvidencePack {
    PackBuilder::new().build()
}

fn blocked_receipt() -> AgentApprovalReceipt {
    receipt(ReviewerRole::Security, "reviewer-security.v1")
        .id("aar_x")
        .decision(ReviewDecision::Block)
        .reason("sql injection")
        .raw_response_sha(None)
        .build()
}

#[test]
fn unknown_condition_fail_closes() {
    let reg = ConditionRegistry::default();
    let p = clean_pack();
    let hits = reg.evaluate(&["does_not_exist".into()], &p, &[]);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].name.starts_with("unknown_condition:"));
}

fn with_files(paths_and_lines: &[(&str, u32, u32)]) -> EvidencePack {
    PackBuilder::new().changed_files(paths_and_lines).build()
}

/// R-7 (D1): after the glob scrub, a contributor editing a path under the
/// removed legacy external-host CI prefix MUST NOT fire
/// `changes_release_or_deploy_policy`. The `.jeryu/ci/` and `deploy/` cases
/// still fire.
#[test]
fn legacy_external_ci_path_does_not_fire_after_glob_scrub() {
    let reg = ConditionRegistry::default();
    // A path under a removed external-host CI prefix must NOT match. (The
    // prefix that used to live here was scrubbed per D1; any path that is
    // no longer in the prefix list simply does not fire.)
    let removed = with_files(&[("legacy-host/ci/build.yml", 5, 0)]);
    let hits = reg.evaluate(&["changes_release_or_deploy_policy".into()], &removed, &[]);
    assert!(
        hits.is_empty(),
        "removed CI glob must not fire; got {hits:?}"
    );
    // The jeryu-native CI prefix DOES fire.
    let jeryu_ci = with_files(&[(".jeryu/ci/release.yml", 5, 0)]);
    let hits = reg.evaluate(&["changes_release_or_deploy_policy".into()], &jeryu_ci, &[]);
    assert_eq!(hits.len(), 1, "jeryu-native CI glob must fire");
    // And the generic `.github/...` prefix still fires.
    let gh = with_files(&[(".github/workflows/release.yml", 5, 0)]);
    let hits = reg.evaluate(&["changes_release_or_deploy_policy".into()], &gh, &[]);
    assert_eq!(hits.len(), 1);
}

#[test]
fn wave3_release_conditions_are_registered() {
    let reg = ConditionRegistry::default();
    for name in [
        "release_artifact_unsigned",
        "release_sbom_missing",
        "release_provenance_missing",
        "rollback_drill_failed",
    ] {
        assert!(
            reg.lookup(name).is_some(),
            "release condition `{name}` must be registered"
        );
    }
}

#[test]
fn wave3_release_conditions_are_externally_supplied() {
    let reg = ConditionRegistry::default();
    let p = clean_pack();
    for name in [
        "release_artifact_unsigned",
        "release_sbom_missing",
        "release_provenance_missing",
        "rollback_drill_failed",
    ] {
        let nc = reg.lookup(name).expect("registered above");
        assert!(
            (nc.func)(&p, &[]).is_none(),
            "{name} must be a no-op locally"
        );
        let hits = reg.evaluate(&[name.to_string()], &p, &[]);
        assert!(hits.is_empty(), "{name} fired unexpectedly: {hits:?}");
    }
}

#[test]
fn path_matcher_does_not_misfire_on_windows_style_separators() {
    let reg = ConditionRegistry::default();
    let p = with_files(&[("repo\\Cargo.lock", 20, 5), ("repo\\src\\main.rs", 3, 1)]);
    let hits = reg.evaluate(&["lockfile_diff_without_manifest_diff".into()], &p, &[]);
    assert!(
        hits.is_empty(),
        "backslash paths must not match; got: {hits:?}"
    );
    let win = with_files(&[("repo\\tests\\foo_test.rs", 0, 100)]);
    let _ = reg.evaluate(&["removes_or_weakens_tests".into()], &win, &[]);
}

#[test]
fn empty_pack_request_list_returns_no_hits() {
    let reg = ConditionRegistry::default();
    let p = clean_pack();
    let hits = reg.evaluate(&[], &p, &[]);
    assert!(hits.is_empty(), "empty request must produce zero hits");
}

#[test]
fn clean_pack_no_hard_stops() {
    let reg = ConditionRegistry::default();
    let p = clean_pack();
    let asked: Vec<String> = reg.names().iter().map(|s| s.to_string()).collect();
    let hits = reg.evaluate(&asked, &p, &[]);
    // evidence_signature_invalid fires because the pack is unsigned here.
    assert!(hits.iter().any(|h| h.name == "evidence_signature_invalid"));
    assert!(!hits.iter().any(|h| h.name == "secret_scan_failed"));
    assert!(!hits.iter().any(|h| h.name == "sast_failed"));
}

/// One named condition paired with the evidence that must (or must not) trip
/// it. Every deterministic, pack-local condition is exercised through this one
/// table, so a new condition is a new row rather than another copy of the same
/// test.
struct Case {
    condition: &'static str,
    pack: EvidencePack,
    receipts: Vec<AgentApprovalReceipt>,
    fires: bool,
    why: &'static str,
}

fn pack_with_coverage_delta(delta: f64) -> EvidencePack {
    let mut p = clean_pack();
    p.tests.coverage_delta = Some(delta);
    p
}

fn pack_with_skipped_tests(n: usize) -> EvidencePack {
    let mut p = clean_pack();
    p.tests.skipped = (0..n).map(|i| format!("test::skip_{i}")).collect();
    p.tests.targeted.clear();
    p
}

fn pack_with_external_source(url: &str) -> EvidencePack {
    let mut p = clean_pack();
    p.supply_chain.external_code_sources = vec![url.into()];
    p
}

fn cases() -> Vec<Case> {
    let fire = |condition, pack, why| Case {
        condition,
        pack,
        receipts: vec![],
        fires: true,
        why,
    };
    let quiet = |condition, pack, why| Case {
        condition,
        pack,
        receipts: vec![],
        fires: false,
        why,
    };
    vec![
        fire(
            "secret_scan_failed",
            pack_with_security(ScanOutcome::Passed, ScanOutcome::Passed, ScanOutcome::Failed),
            "a failed secret scan is a hard stop",
        ),
        Case {
            condition: "reviewer_blocked",
            pack: clean_pack(),
            receipts: vec![blocked_receipt()],
            fires: true,
            why: "one blocking reviewer is a hard stop",
        },
        fire(
            "removes_or_weakens_tests",
            with_files(&[
                ("src/foo.rs", 30, 5),
                ("tests/foo_test.rs", 0, 40),
                ("src/foo/__tests__/bar.test.ts", 1, 20),
            ]),
            "test files deleted across several paths",
        ),
        quiet(
            "removes_or_weakens_tests",
            with_files(&[("tests/util_test.rs", 8, 12)]),
            "a small single-file refactor is not a weakening",
        ),
        quiet(
            "removes_or_weakens_tests",
            pack_with_skipped_tests(50),
            "skipped tests without file deletions must not fire",
        ),
        fire(
            "coverage_threshold_lowered",
            pack_with_coverage_delta(-3.5),
            "coverage dropped",
        ),
        quiet(
            "coverage_threshold_lowered",
            pack_with_coverage_delta(0.0),
            "flat coverage is not a drop",
        ),
        fire(
            "snapshot_mass_replacement",
            with_files(&[("src/__snapshots__/widget.snap", 150, 80)]),
            "a snapshot rewritten wholesale",
        ),
        fire(
            "changes_security_scanner_config",
            with_files(&[("deny.toml", 3, 1)]),
            "the dependency-deny config is scanner config",
        ),
        fire(
            "changes_release_or_deploy_policy",
            with_files(&[("deploy/prod/k8s.yaml", 5, 0)]),
            "a deploy manifest is release policy",
        ),
        fire(
            "changes_agent_prompts_or_judge_policy",
            with_files(&[(".jeryu/autonomy/prompts/reviewer-security.md", 10, 2)]),
            "a reviewer prompt is judge policy",
        ),
        fire(
            "touches_secret_handling",
            with_files(&[("src/secrets/store.rs", 12, 0)]),
            "a path under src/secrets handles secrets",
        ),
        fire(
            "introduces_new_external_code_source",
            pack_with_external_source("https://example.com/gist/foo"),
            "code pulled from a gist is a new external source",
        ),
        fire(
            "lockfile_diff_without_manifest_diff",
            with_files(&[("Cargo.lock", 20, 5), ("src/foo.rs", 3, 1)]),
            "a lockfile moved with no manifest behind it",
        ),
        quiet(
            "lockfile_diff_without_manifest_diff",
            with_files(&[("Cargo.lock", 20, 5), ("Cargo.toml", 1, 1)]),
            "a matching manifest explains the lockfile diff",
        ),
    ]
}

#[test]
fn named_conditions_fire_exactly_on_their_evidence() {
    let reg = ConditionRegistry::default();
    for Case {
        condition,
        pack,
        receipts,
        fires,
        why,
    } in cases()
    {
        let hits = reg.evaluate(&[condition.to_string()], &pack, &receipts);
        if fires {
            assert_eq!(hits.len(), 1, "{condition}: {why}; got {hits:?}");
            assert_eq!(hits[0].name, condition, "{condition}: {why}");
        } else {
            assert!(hits.is_empty(), "{condition}: {why}; got {hits:?}");
        }
    }
}

/// The property the registry owes policy: every condition name the shipped
/// policy bundle references resolves, so a policy-driven walk never degrades
/// into a fail-closed `unknown_condition:` hit. A bare count of registered
/// names would still pass while the one name a policy needs went missing.
#[test]
fn every_condition_named_by_policy_is_registered() {
    let reg = ConditionRegistry::default();
    let bundle = crate::test_support::bundle();
    let referenced: Vec<String> = bundle
        .approvals
        .hard_stops
        .iter()
        .map(|h| h.name.clone())
        .chain(
            bundle
                .risk
                .tiers
                .iter()
                .flat_map(|t| t.matchers.iter())
                .flat_map(|m| m.conditions.iter().cloned()),
        )
        .chain(bundle.protected_paths.semantic_triggers.iter().cloned())
        .collect();
    assert!(
        !referenced.is_empty(),
        "the policy fixtures must reference conditions"
    );
    for name in &referenced {
        assert!(
            reg.lookup(name).is_some(),
            "policy references `{name}`, which is not in the registry"
        );
    }
    let mut names = reg.names();
    let registered = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), registered, "condition names must be unique");
}

// --- CI gate (required-check lanes) -------------------------------------

#[test]
fn ci_conditions_are_registered() {
    let reg = ConditionRegistry::default();
    for name in ["missing_required_ci_check", "failed_required_ci_check"] {
        assert!(
            reg.lookup(name).is_some(),
            "CI condition `{name}` must be registered"
        );
    }
}

#[test]
fn ci_conditions_are_no_ops_in_the_registry_walk() {
    // The registry functions are placeholders; the real hit is computed by the
    // judge via `ci_hard_stops`. A registry walk over the names alone fires
    // nothing.
    let reg = ConditionRegistry::default();
    let p = clean_pack();
    let hits = reg.evaluate(
        &[
            "missing_required_ci_check".into(),
            "failed_required_ci_check".into(),
        ],
        &p,
        &[],
    );
    assert!(hits.is_empty(), "registry placeholders must not fire");
}

fn pack_with_ci(checks: &[(&str, CiConclusion)]) -> EvidencePack {
    PackBuilder::new().ci(checks).build()
}

#[test]
fn ci_hard_stops_empty_required_is_no_gate() {
    let p = pack_with_ci(&[("ci", CiConclusion::Failure)]);
    assert!(super::ci_hard_stops(&p, &[]).is_empty());
}

#[test]
fn ci_hard_stops_all_green_yields_no_hits() {
    let p = pack_with_ci(&[
        ("ci-fast", CiConclusion::Success),
        ("ci-full", CiConclusion::Success),
    ]);
    let lanes = vec!["ci-fast".to_string(), "ci-full".to_string()];
    assert!(super::ci_hard_stops(&p, &lanes).is_empty());
}

#[test]
fn ci_hard_stops_missing_lane_fires_missing() {
    let p = pack_with_ci(&[("ci-fast", CiConclusion::Success)]);
    let lanes = vec!["ci-fast".to_string(), "ci-full".to_string()];
    let hits = super::ci_hard_stops(&p, &lanes);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "missing_required_ci_check");
}

#[test]
fn ci_hard_stops_non_success_lane_fires_failed() {
    for bad in [
        CiConclusion::Failure,
        CiConclusion::Cancelled,
        CiConclusion::TimedOut,
        CiConclusion::Pending,
    ] {
        let p = pack_with_ci(&[("ci-fast", bad)]);
        let lanes = vec!["ci-fast".to_string()];
        let hits = super::ci_hard_stops(&p, &lanes);
        assert_eq!(hits.len(), 1, "{bad:?} must fire");
        assert_eq!(hits[0].name, "failed_required_ci_check", "{bad:?}");
    }
}

#[test]
fn ci_hard_stops_reports_both_missing_and_failed() {
    let p = pack_with_ci(&[("ci-full", CiConclusion::Failure)]);
    let lanes = vec!["ci-fast".to_string(), "ci-full".to_string()];
    let hits = super::ci_hard_stops(&p, &lanes);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].name, "missing_required_ci_check");
    assert_eq!(hits[1].name, "failed_required_ci_check");
}
