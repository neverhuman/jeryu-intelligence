//! Exact-SHA binding.
//!
//! A receipt or verdict is valid only against a specific (head_sha, policy_sha)
//! tuple. Any drift invalidates the receipt/verdict — the orchestrator must
//! re-run reviews or fail closed.

use crate::types::{AgentApprovalReceipt, EvidencePack};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShaBindError {
    #[error("head_sha mismatch: receipt says {receipt_head}, pack says {pack_head}")]
    HeadMismatch {
        receipt_head: String,
        pack_head: String,
    },
    #[error("policy_sha mismatch: receipt says {receipt_policy}, pack says {pack_policy}")]
    PolicyMismatch {
        receipt_policy: String,
        pack_policy: String,
    },
    #[error("evidence_pack_id mismatch: receipt says {receipt_id}, pack says {pack_id}")]
    PackIdMismatch { receipt_id: String, pack_id: String },
}

pub fn verify_sha_binding(
    pack: &EvidencePack,
    receipt: &AgentApprovalReceipt,
) -> Result<(), ShaBindError> {
    if receipt.evidence_pack_id != pack.id {
        return Err(ShaBindError::PackIdMismatch {
            receipt_id: receipt.evidence_pack_id.clone(),
            pack_id: pack.id.clone(),
        });
    }
    if receipt.head_sha != pack.head_sha {
        return Err(ShaBindError::HeadMismatch {
            receipt_head: receipt.head_sha.clone(),
            pack_head: pack.head_sha.clone(),
        });
    }
    if receipt.policy_sha != pack.policy_sha {
        return Err(ShaBindError::PolicyMismatch {
            receipt_policy: receipt.policy_sha.clone(),
            pack_policy: pack.policy_sha.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn pack() -> EvidencePack {
        crate::test_support::PackBuilder::new()
            .author_agent(Some("builder"))
            .build()
    }

    fn receipt_for(p: &EvidencePack) -> AgentApprovalReceipt {
        crate::test_support::receipt(ReviewerRole::Security, "sec.v1")
            .id("aar_x")
            .bound_to(p)
            .raw_response_sha(None)
            .build()
    }

    #[test]
    fn matching_sha_passes() {
        let p = pack();
        let r = receipt_for(&p);
        assert!(verify_sha_binding(&p, &r).is_ok());
    }

    #[test]
    fn head_drift_rejects() {
        let p = pack();
        let mut r = receipt_for(&p);
        r.head_sha = "d".repeat(40);
        let err = verify_sha_binding(&p, &r).unwrap_err();
        assert!(matches!(err, ShaBindError::HeadMismatch { .. }));
    }

    #[test]
    fn policy_drift_rejects() {
        let p = pack();
        let mut r = receipt_for(&p);
        r.policy_sha = "e".repeat(40);
        let err = verify_sha_binding(&p, &r).unwrap_err();
        assert!(matches!(err, ShaBindError::PolicyMismatch { .. }));
    }
}
