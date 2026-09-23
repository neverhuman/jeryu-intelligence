//! End-to-end integration test: the FULL-AUTO auto-merge brain (#7).
//!
//! Drives the *real* dogfood decision through the public API only —
//! [`FullAutoProfile`] → derived [`PolicyBundle`] → the pure [`judge`] +
//! [`ConditionRegistry`] hard-stop walk + the new CI-check gating
//! ([`EvidencePack::ci_status`] vs. `approvals.required_ci_lanes`) → the
//! post-judge `resolve` floor → the [`KillBell`] hot-path downgrade.
//!
//! Each scenario is the auto-merge decision an owner who installed a full-auto
//! profile actually gets:
//!   (a) full-auto R0..=R4 PR, every required CI lane Success + clean evidence
//!       → AllowMerge;
//!   (b) same but one required lane Failure → blocked (Reject — veto > approval);
//!   (c) a required lane Missing from `ci_status` → blocked (Reject, fail-closed);
//!   (d) an R5 change → RequireHuman regardless of green CI;
//!   (e) a registry hard_stop (`secret_scan_failed`) → Reject even with green CI;
//!   (f) kill-bell engaged → an AllowMerge downgraded to RequireHuman.

use std::sync::Arc;

use chrono::Utc;
use jeryu_autonomy::test_support::{PackBuilder, bundle_requiring, full_passing_receipts};
use jeryu_autonomy::{
    CiConclusion, ConditionRegistry, EdSigningKey, EvidencePack, FullAutoProfile, GateDecision,
    JudgeInputs, KillBell, MemoryLedger, RiskTier, VerdictLedger, judge,
};

/// A signed, SHA-bound pack at `tier` with clean (or, if `secret_failed`,
/// secret-scan-failing) security evidence and the supplied CI status.
fn pack_at_tier(tier: RiskTier, secret_failed: bool, ci: &[(&str, CiConclusion)]) -> EvidencePack {
    // A full-auto pack is signed by the evidence builder; the unsigned case is
    // covered elsewhere (it trips `evidence_signature_invalid`).
    PackBuilder::new()
        .source_branch("jeryu-pr-7")
        .risk(tier)
        .secret_scan_failed(secret_failed)
        .ci(ci)
        .signed(true)
        .build()
}

/// The full dogfood decision: derive the full-auto bundle, fuse the pack through
/// the *real* judge against full passing receipts, then re-assert the floor via
/// `resolve`. This is exactly the auto-merge brain a loaded profile runs.
fn full_auto_decision(profile: &FullAutoProfile, pack: &EvidencePack) -> GateDecision {
    let derived = profile.apply();
    let receipts = full_passing_receipts(pack);
    let out = judge(JudgeInputs {
        pack,
        receipts: &receipts,
        policy: &derived,
        repo: "org/p",
        target_branch: "main",
        pull_request: Some("7"),
        author_agent: Some("builder.x"),
        external_hard_stops: &[],
    });
    profile.resolve(pack.risk, out.verdict.decision)
}

// (a) ----------------------------------------------------------------------

#[test]
fn full_auto_all_required_lanes_green_allows_merge_r0_through_r4() {
    let profile = FullAutoProfile::new(bundle_requiring(&["ci-fast", "ci-full"])).unwrap();
    for t in [
        RiskTier::R0,
        RiskTier::R1,
        RiskTier::R2,
        RiskTier::R3,
        RiskTier::R4,
    ] {
        let pack = pack_at_tier(
            t,
            false,
            &[
                ("ci-fast", CiConclusion::Success),
                ("ci-full", CiConclusion::Success),
            ],
        );
        assert_eq!(
            full_auto_decision(&profile, &pack),
            GateDecision::AllowMerge,
            "full-auto with all required lanes green + clean evidence must AllowMerge at {t:?}"
        );
    }
}

// (b) ----------------------------------------------------------------------

#[test]
fn full_auto_failed_required_lane_blocks() {
    let profile = FullAutoProfile::new(bundle_requiring(&["ci-fast", "ci-full"])).unwrap();
    for t in [
        RiskTier::R0,
        RiskTier::R1,
        RiskTier::R2,
        RiskTier::R3,
        RiskTier::R4,
    ] {
        let pack = pack_at_tier(
            t,
            false,
            &[
                ("ci-fast", CiConclusion::Success),
                ("ci-full", CiConclusion::Failure),
            ],
        );
        let decision = full_auto_decision(&profile, &pack);
        assert_eq!(
            decision,
            GateDecision::Reject,
            "a failed required lane must block (veto > approval) at {t:?}, got {decision:?}"
        );
        assert!(
            matches!(decision, GateDecision::Reject | GateDecision::RequireHuman),
            "blocked == not AllowMerge at {t:?}"
        );
    }

    // And the verdict names the specific CI hard stop so an operator can see why.
    let pack = pack_at_tier(
        RiskTier::R2,
        false,
        &[
            ("ci-fast", CiConclusion::Success),
            ("ci-full", CiConclusion::Failure),
        ],
    );
    let derived = profile.apply();
    let receipts = full_passing_receipts(&pack);
    let out = judge(JudgeInputs {
        pack: &pack,
        receipts: &receipts,
        policy: &derived,
        repo: "org/p",
        target_branch: "main",
        pull_request: Some("7"),
        author_agent: Some("builder.x"),
        external_hard_stops: &[],
    });
    assert_eq!(out.verdict.decision, GateDecision::Reject);
    assert!(
        out.verdict
            .hard_stops
            .iter()
            .any(|n| n == "failed_required_ci_check"),
        "expected failed_required_ci_check, got {:?}",
        out.verdict.hard_stops
    );
}

// (c) ----------------------------------------------------------------------

#[test]
fn full_auto_missing_required_lane_blocks() {
    let profile = FullAutoProfile::new(bundle_requiring(&["ci-fast", "ci-full"])).unwrap();
    for t in [
        RiskTier::R0,
        RiskTier::R1,
        RiskTier::R2,
        RiskTier::R3,
        RiskTier::R4,
    ] {
        // ci-full is required by policy but never reported in ci_status.
        let pack = pack_at_tier(t, false, &[("ci-fast", CiConclusion::Success)]);
        let decision = full_auto_decision(&profile, &pack);
        assert_eq!(
            decision,
            GateDecision::Reject,
            "a missing required lane must block (fail-closed) at {t:?}, got {decision:?}"
        );
    }

    // The verdict names the missing-check hard stop.
    let pack = pack_at_tier(RiskTier::R2, false, &[("ci-fast", CiConclusion::Success)]);
    let derived = profile.apply();
    let receipts = full_passing_receipts(&pack);
    let out = judge(JudgeInputs {
        pack: &pack,
        receipts: &receipts,
        policy: &derived,
        repo: "org/p",
        target_branch: "main",
        pull_request: Some("7"),
        author_agent: Some("builder.x"),
        external_hard_stops: &[],
    });
    assert_eq!(out.verdict.decision, GateDecision::Reject);
    assert!(
        out.verdict
            .hard_stops
            .iter()
            .any(|n| n == "missing_required_ci_check"),
        "expected missing_required_ci_check, got {:?}",
        out.verdict.hard_stops
    );
}

// (d) ----------------------------------------------------------------------

#[test]
fn full_auto_r5_requires_human_even_with_green_ci() {
    let profile = FullAutoProfile::new(bundle_requiring(&["ci-fast"])).unwrap();
    let pack = pack_at_tier(RiskTier::R5, false, &[("ci-fast", CiConclusion::Success)]);
    assert_eq!(
        full_auto_decision(&profile, &pack),
        GateDecision::RequireHuman,
        "R5 stays fail-closed (human required) regardless of green CI"
    );
    // And the profile never claims R5 is auto-merge-eligible.
    assert!(!profile.tier_auto_merge_eligible(RiskTier::R5));
}

// (e) ----------------------------------------------------------------------

#[test]
fn full_auto_registry_hard_stop_rejects_even_with_green_ci() {
    let profile = FullAutoProfile::new(bundle_requiring(&["ci-fast"])).unwrap();
    // secret_scan failed → the registry's secret_scan_failed hard stop fires,
    // even though the required CI lane is green and full-auto is enabled.
    let pack = pack_at_tier(RiskTier::R2, true, &[("ci-fast", CiConclusion::Success)]);
    let derived = profile.apply();
    let receipts = full_passing_receipts(&pack);
    let out = judge(JudgeInputs {
        pack: &pack,
        receipts: &receipts,
        policy: &derived,
        repo: "org/p",
        target_branch: "main",
        pull_request: Some("7"),
        author_agent: Some("builder.x"),
        external_hard_stops: &[],
    });
    assert_eq!(
        out.verdict.decision,
        GateDecision::Reject,
        "veto > approval: secret_scan_failed rejects even with green CI under full-auto"
    );
    assert_eq!(
        profile.resolve(pack.risk, out.verdict.decision),
        GateDecision::Reject
    );
    assert!(
        out.verdict
            .hard_stops
            .iter()
            .any(|n| n == "secret_scan_failed"),
        "expected secret_scan_failed, got {:?}",
        out.verdict.hard_stops
    );

    // The registry itself reports the same named hard stop directly.
    let reg = ConditionRegistry::default();
    let hits = reg.evaluate(&["secret_scan_failed".into()], &pack, &receipts);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "secret_scan_failed");
}

// (f) ----------------------------------------------------------------------

#[tokio::test]
async fn full_auto_kill_bell_downgrades_allow_merge_to_require_human() {
    let profile = FullAutoProfile::new(bundle_requiring(&["ci-fast"])).unwrap();
    let pack = pack_at_tier(RiskTier::R4, false, &[("ci-fast", CiConclusion::Success)]);
    // Pre-bell, full-auto with green CI AllowMerges at R4.
    assert_eq!(
        full_auto_decision(&profile, &pack),
        GateDecision::AllowMerge,
        "pre-bell, green CI lets full-auto AllowMerge at R4"
    );

    // Engage the kill bell, then run the AllowMerge through the hot-path
    // downgrade — it must become RequireHuman.
    let ledger: Arc<dyn VerdictLedger> = Arc::new(MemoryLedger::new());
    let bell = KillBell::new(ledger);
    let key = EdSigningKey::generate("operator.alice");
    let now = Utc::now();
    bell.pause("incident", "alice", 3600, &key, now)
        .await
        .unwrap();

    let (decision, why) = bell
        .downgrade_if_paused(full_auto_decision(&profile, &pack), now)
        .await
        .unwrap();
    assert_eq!(
        decision,
        GateDecision::RequireHuman,
        "kill bell downgrades full-auto AllowMerge to RequireHuman"
    );
    assert!(why.unwrap().contains("incident"));
}
