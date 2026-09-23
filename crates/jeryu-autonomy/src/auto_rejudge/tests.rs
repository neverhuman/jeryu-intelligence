use super::*;
use crate::ledger::MemoryLedger;
use crate::policy_yaml::fixtures;
use crate::seam::LedgerFilter;
use crate::test_support::{self, PackBuilder};
use crate::types::*;
use crate::verdict_store::MemoryVerdictStore;
use async_trait::async_trait;
use chrono::{Duration, Utc};

fn signed_pack(repo: &str) -> EvidencePack {
    PackBuilder::new()
        .repo(repo)
        .source_branch("jeryu-pr-1")
        .signed(true)
        .build()
}

fn receipt(role: ReviewerRole, agent: &str, pack: &EvidencePack) -> AgentApprovalReceipt {
    test_support::receipt(role, agent).bound_to(pack).build()
}

/// A controllable evidence source.
struct FakeEvidence {
    roles_to_pass: Vec<ReviewerRole>,
    build_fails: bool,
    reviews_fail: bool,
}

#[async_trait]
impl EvidenceSource for FakeEvidence {
    async fn build_pack(&self, repo: &str, _pr_id: &str) -> SeamResult<EvidencePack> {
        if self.build_fails {
            return Err(SeamError::new("forge", "diff fetch failed"));
        }
        Ok(signed_pack(repo))
    }
    async fn run_reviews(
        &self,
        pack: &EvidencePack,
        _required: &[ReviewerRole],
    ) -> SeamResult<Vec<AgentApprovalReceipt>> {
        if self.reviews_fail {
            return Err(SeamError::new("orchestrator", "reviewer outage"));
        }
        Ok(self
            .roles_to_pass
            .iter()
            .map(|r| receipt(*r, &format!("{r:?}.v1"), pack))
            .collect())
    }
}

fn prior_verdict(repo: &str) -> VibeGateVerdict {
    let now = Utc::now();
    VibeGateVerdict {
        schema: SchemaTag::new(),
        id: "vgv_prior".into(),
        evidence_pack_id: "ep_prior".into(),
        pull_request: Some("1".into()),
        repo: repo.into(),
        target_branch: "main".into(),
        head_sha: "0".repeat(40),
        policy_sha: "c".repeat(40),
        evidence_pack_digest: "sha256:prior".into(),
        risk: RiskTier::R2,
        hard_stops: vec![],
        required_reviews: vec![],
        approval_receipts: vec![],
        decision: GateDecision::AllowMerge,
        valid_for_head_sha_only: true,
        rebind_on_train: true,
        expires_at: now - Duration::minutes(1),
        created_at: now - Duration::minutes(61),
        signature: Signature::unsigned(),
    }
}

fn service(
    evidence: FakeEvidence,
) -> (
    AutoRejudgeService,
    Arc<MemoryLedger>,
    Arc<MemoryVerdictStore>,
) {
    let ledger = Arc::new(MemoryLedger::new());
    let store = Arc::new(MemoryVerdictStore::new());
    let svc = AutoRejudgeService::new(
        Arc::new(evidence),
        store.clone(),
        ledger.clone(),
        Arc::new(EdSigningKey::generate("judge.v1")),
        Arc::new(fixtures::default_bundle()),
    );
    (svc, ledger, store)
}

#[tokio::test]
async fn one_cycle_produces_one_verdict_one_ledger_entry_and_saves() {
    let evidence = FakeEvidence {
        roles_to_pass: vec![ReviewerRole::Security, ReviewerRole::TestIntegrity],
        build_fails: false,
        reviews_fail: false,
    };
    let (svc, ledger, store) = service(evidence);
    let prior = prior_verdict("owner/repo");
    store.save(&prior).await.unwrap();

    let out = svc.rejudge("owner/repo", "1", &prior).await.unwrap();
    assert_eq!(
        out.new_decision,
        GateDecision::AllowMerge,
        "quorum met → AllowMerge"
    );
    assert_eq!(out.receipts_count, 2);

    // Exactly one new VerdictIssued ledger entry, ed25519-signed.
    let entries = ledger
        .list(&LedgerFilter {
            kind: Some(LedgerKind::VerdictIssued),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].signature.algo, "ed25519");
    assert_eq!(entries[0].payload["wave_scope"], "auto_rejudge");

    // The new verdict superseded the prior one for (repo, pr).
    let latest = store
        .load_latest("owner/repo", Some("1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, out.new_verdict_id);
    assert_ne!(latest.id, prior.id);
}

#[tokio::test]
async fn orchestrator_failure_degrades_to_require_human_not_abort() {
    let evidence = FakeEvidence {
        roles_to_pass: vec![],
        build_fails: false,
        reviews_fail: true, // reviewer outage
    };
    let (svc, ledger, _store) = service(evidence);
    let prior = prior_verdict("owner/repo");
    let out = svc.rejudge("owner/repo", "1", &prior).await.unwrap();
    // No receipts → insufficient quorum at R2 → RequireHuman (not an abort).
    assert_eq!(out.new_decision, GateDecision::RequireHuman);
    assert_eq!(out.receipts_count, 0);
    let entries = ledger.list(&LedgerFilter::default()).await.unwrap();
    assert_eq!(entries.len(), 1, "still produced one signed ledger entry");
}

#[tokio::test]
async fn pack_builder_failure_bubbles_up() {
    let evidence = FakeEvidence {
        roles_to_pass: vec![],
        build_fails: true,
        reviews_fail: false,
    };
    let (svc, _ledger, _store) = service(evidence);
    let prior = prior_verdict("owner/repo");
    let err = svc.rejudge("owner/repo", "1", &prior).await.unwrap_err();
    assert_eq!(err.source, "forge");
}
