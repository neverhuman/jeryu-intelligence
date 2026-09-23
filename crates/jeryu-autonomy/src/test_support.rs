//! Shared test data builders for the autonomy crate.
//!
//! Every suite needs the same two shapes — an `EvidencePack` and an
//! `AgentApprovalReceipt` bound to it — differing in a field or two. Spelling
//! those structs out per suite is how the 40-line literal ended up copied into
//! five modules, so the fluent builders below own the defaults once and each
//! test states only what it varies (clears HLT-043 copy-code and the
//! `severe-duplication-in-product-code` cap).
//!
//! Compiled under `cfg(test)` for the in-crate suites and under the
//! `test-support` feature for the integration tests in `tests/`, which reach it
//! as `jeryu_autonomy::test_support`.

use crate::evidence::{EvidenceInputs, build_evidence_pack};
use crate::policy_yaml::{PolicyBundle, fixtures};
use crate::types::*;
use chrono::Utc;
use jeryu_signing::Signature;

/// The default policy bundle shared by every suite.
pub fn bundle() -> PolicyBundle {
    fixtures::default_bundle()
}

/// The default bundle with `approvals.required_ci_lanes` set, so the pre-merge
/// CI gate is armed.
pub fn bundle_requiring(lanes: &[&str]) -> PolicyBundle {
    let mut b = bundle();
    b.approvals.required_ci_lanes = lanes.iter().map(|s| s.to_string()).collect();
    b
}

/// The ed25519 signature an evidence builder puts on a pack.
pub fn evidence_builder_signature() -> Signature {
    Signature {
        key_id: "evidence-builder.v1".into(),
        algo: "ed25519".into(),
        value: "0".repeat(128),
    }
}

/// Builder for an [`EvidencePack`].
///
/// Defaults: repo `org/p`, branch `agent/x` → `main`, the canonical
/// `a`/`b`/`c` SHAs, author `builder.x`, tier `R2`, every scan passed, revert
/// -commit rollback, no changed files, no CI status, unsigned.
pub struct PackBuilder {
    repo: String,
    source_branch: String,
    target_branch: String,
    head_sha: String,
    base_sha: String,
    policy_sha: String,
    author_agent: Option<String>,
    intent_id: Option<String>,
    risk: RiskTier,
    changed_files: Vec<ChangedFile>,
    tests: TestsSection,
    security: SecuritySection,
    ci_status: Vec<CiCheck>,
    signature: Option<Signature>,
}

impl Default for PackBuilder {
    fn default() -> Self {
        Self {
            repo: "org/p".into(),
            source_branch: "agent/x".into(),
            target_branch: "main".into(),
            head_sha: "a".repeat(40),
            base_sha: "b".repeat(40),
            policy_sha: "c".repeat(40),
            author_agent: Some("builder.x".into()),
            intent_id: None,
            risk: RiskTier::R2,
            changed_files: vec![],
            tests: TestsSection {
                targeted: vec![],
                full_required: false,
                skipped: vec![],
                coverage_delta: None,
            },
            security: SecuritySection {
                sast: ScanOutcome::Passed,
                dependency_scan: ScanOutcome::Passed,
                secret_scan: ScanOutcome::Passed,
            },
            ci_status: vec![],
            signature: None,
        }
    }
}

/// Start a pack with the shared defaults.
pub fn pack() -> PackBuilder {
    PackBuilder::default()
}

impl PackBuilder {
    /// A pack with the shared defaults; same as [`pack`].
    pub fn new() -> Self {
        Self::default()
    }

    pub fn repo(mut self, repo: &str) -> Self {
        self.repo = repo.into();
        self
    }

    pub fn source_branch(mut self, branch: &str) -> Self {
        self.source_branch = branch.into();
        self
    }

    pub fn head_sha(mut self, sha: &str) -> Self {
        self.head_sha = sha.into();
        self
    }

    pub fn policy_sha(mut self, sha: &str) -> Self {
        self.policy_sha = sha.into();
        self
    }

    pub fn author_agent(mut self, agent: Option<&str>) -> Self {
        self.author_agent = agent.map(Into::into);
        self
    }

    pub fn risk(mut self, tier: RiskTier) -> Self {
        self.risk = tier;
        self
    }

    pub fn security(
        mut self,
        sast: ScanOutcome,
        dependency: ScanOutcome,
        secret: ScanOutcome,
    ) -> Self {
        self.security = SecuritySection {
            sast,
            dependency_scan: dependency,
            secret_scan: secret,
        };
        self
    }

    pub fn secret_scan(mut self, outcome: ScanOutcome) -> Self {
        self.security.secret_scan = outcome;
        self
    }

    /// Secret scan failed / passed, the knob most gate tests toggle.
    pub fn secret_scan_failed(self, failed: bool) -> Self {
        self.secret_scan(if failed {
            ScanOutcome::Failed
        } else {
            ScanOutcome::Passed
        })
    }

    pub fn tests_section(mut self, tests: TestsSection) -> Self {
        self.tests = tests;
        self
    }

    /// `(path, lines_added, lines_removed)` triples, untagged.
    pub fn changed_files(mut self, files: &[(&str, u32, u32)]) -> Self {
        self.changed_files = files
            .iter()
            .map(|(path, added, removed)| ChangedFile {
                path: (*path).into(),
                risk_tags: vec![],
                lines_added: *added,
                lines_removed: *removed,
            })
            .collect();
        self
    }

    pub fn ci(mut self, checks: &[(&str, CiConclusion)]) -> Self {
        self.ci_status = checks
            .iter()
            .map(|(name, conclusion)| CiCheck {
                name: (*name).to_string(),
                conclusion: *conclusion,
            })
            .collect();
        self
    }

    /// Carry the evidence-builder signature, so `evidence_signature_invalid`
    /// accepts the pack.
    pub fn signed(mut self, signed: bool) -> Self {
        self.signature = signed.then(evidence_builder_signature);
        self
    }

    pub fn build(self) -> EvidencePack {
        let mut p = build_evidence_pack(EvidenceInputs {
            repo: &self.repo,
            source_branch: &self.source_branch,
            target_branch: &self.target_branch,
            head_sha: &self.head_sha,
            base_sha: &self.base_sha,
            policy_sha: &self.policy_sha,
            author_agent: self.author_agent.as_deref(),
            intent_id: self.intent_id.as_deref(),
            risk: self.risk,
            changed_files: self.changed_files,
            claims: vec![],
            tests: self.tests,
            security: self.security,
            supply_chain: SupplyChainSection::default(),
            rollback: RollbackSection {
                strategy: RollbackStrategy::RevertCommit,
                feature_flag: None,
                data_migration_reversible: Some(true),
            },
            gate_receipts: vec![],
            ci_status: self.ci_status,
        });
        p.signature = self.signature;
        p
    }
}

/// Builder for an [`AgentApprovalReceipt`].
///
/// Defaults: decision `Pass`, not the author, a stub raw-response digest, and
/// the canonical `a`/`c` SHAs — use [`ReceiptBuilder::bound_to`] to rebind it
/// to a pack.
pub struct ReceiptBuilder {
    id: String,
    evidence_pack_id: String,
    role: ReviewerRole,
    agent_id: String,
    head_sha: String,
    policy_sha: String,
    decision: ReviewDecision,
    reason: Option<String>,
    not_author: bool,
    raw_response_sha: Option<String>,
    signature: Signature,
}

/// Start a receipt for `role` by `agent`, with the shared defaults.
pub fn receipt(role: ReviewerRole, agent: &str) -> ReceiptBuilder {
    ReceiptBuilder {
        id: format!("aar_{agent}"),
        evidence_pack_id: "evp_x".into(),
        role,
        agent_id: agent.into(),
        head_sha: "a".repeat(40),
        policy_sha: "c".repeat(40),
        decision: ReviewDecision::Pass,
        reason: None,
        not_author: true,
        raw_response_sha: Some("sha256:beef".into()),
        signature: Signature::unsigned(),
    }
}

impl ReceiptBuilder {
    /// Bind the receipt to `pack`'s id and (head, policy) SHA tuple.
    pub fn bound_to(mut self, pack: &EvidencePack) -> Self {
        self.evidence_pack_id = pack.id.clone();
        self.head_sha = pack.head_sha.clone();
        self.policy_sha = pack.policy_sha.clone();
        self
    }

    pub fn id(mut self, id: &str) -> Self {
        self.id = id.into();
        self
    }

    pub fn decision(mut self, decision: ReviewDecision) -> Self {
        self.decision = decision;
        self
    }

    pub fn reason(mut self, reason: &str) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn not_author(mut self, not_author: bool) -> Self {
        self.not_author = not_author;
        self
    }

    pub fn head_sha(mut self, sha: &str) -> Self {
        self.head_sha = sha.into();
        self
    }

    pub fn policy_sha(mut self, sha: &str) -> Self {
        self.policy_sha = sha.into();
        self
    }

    pub fn raw_response_sha(mut self, sha: Option<&str>) -> Self {
        self.raw_response_sha = sha.map(Into::into);
        self
    }

    pub fn signature(mut self, signature: Signature) -> Self {
        self.signature = signature;
        self
    }

    /// The per-agent signature the judge suite mints.
    pub fn signed_by_agent(self) -> Self {
        let key_id = format!("{}.ed25519", self.agent_id);
        self.signature(Signature {
            key_id,
            algo: "hmac-sha256-insecure".into(),
            value: "0".repeat(64),
        })
    }

    pub fn build(self) -> AgentApprovalReceipt {
        AgentApprovalReceipt {
            schema: SchemaTag::new(),
            id: self.id,
            evidence_pack_id: self.evidence_pack_id,
            role: self.role,
            agent_id: self.agent_id,
            prompt_sha: None,
            provider: None,
            model: None,
            temperature: None,
            seed: None,
            raw_response_sha: self.raw_response_sha,
            head_sha: self.head_sha,
            policy_sha: self.policy_sha,
            decision: self.decision,
            reason: self.reason,
            findings: vec![],
            not_author: self.not_author,
            tokens: TokenCounts::default(),
            created_at: Utc::now(),
            signature: self.signature,
        }
    }
}

/// The four distinct reviewer roles passing on `pack` — enough to clear the
/// agent-reviewer quorum at any tier full-auto makes eligible (R3 needs 4), and
/// none of them the author.
pub fn full_passing_receipts(pack: &EvidencePack) -> Vec<AgentApprovalReceipt> {
    [
        (ReviewerRole::Security, "sec.v1"),
        (ReviewerRole::TestIntegrity, "test.v1"),
        (ReviewerRole::Runtime, "rt.v1"),
        (ReviewerRole::Lockfile, "lock.v1"),
    ]
    .into_iter()
    .map(|(role, agent)| receipt(role, agent).bound_to(pack).build())
    .collect()
}
