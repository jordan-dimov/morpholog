//! Verifying an audit prefix one row at a time, so memory does not grow
//! with the log. The live verifier and every complete-prefix pack feed
//! the same verifier, so they cannot drift.
//!
//! What a failure outranks is fixed: the anchor first, then each
//! checkpoint in chain order (its link, then its root, then its
//! signatures), then signing-key authority, which counts only once the
//! whole tree is intact. The first failure found is kept; nothing found
//! later replaces it. What the input itself must look like (row order, row
//! count, a non-empty chain) belongs to the caller.

use std::collections::HashSet;

use crate::audit::AuditRow;
use crate::checkpoints::{
    Checkpoint, TreeVerification, checkpoint_hash, same_tree_head, signature_crypto_violation,
    unauthorized_signature,
};
use crate::keys::{KeyTriple, apply_key_claims};
use crate::merkle::{Digest, Frontier, audit_leaf_hash};
use crate::role_rebindings::RebindingFold;

pub(crate) struct PrefixVerifier<'a> {
    checkpoints: &'a [Checkpoint],
    anchor: Option<&'a Checkpoint>,
    /// How many checkpoints have been checked and held.
    held: usize,
    frontier: Frontier,
    keys: HashSet<KeyTriple>,
    failure: Option<TreeVerification>,
    unauthorized: Option<TreeVerification>,
    anchor_unauthorized: Option<TreeVerification>,
    rebindings: RebindingFold,
}

impl<'a> PrefixVerifier<'a> {
    /// `checkpoints` in chain order; `anchor` is an externally held copy
    /// of one of them.
    pub(crate) fn new(checkpoints: &'a [Checkpoint], anchor: Option<&'a Checkpoint>) -> Self {
        let mut verifier = PrefixVerifier {
            checkpoints,
            anchor,
            held: 0,
            frontier: Frontier::default(),
            keys: HashSet::new(),
            failure: anchor.and_then(|a| anchor_failure(checkpoints, a)),
            unauthorized: None,
            anchor_unauthorized: None,
            rebindings: RebindingFold::default(),
        };
        verifier.settle();
        verifier
    }

    /// The next row in log order. A row that cannot be hashed is an error,
    /// not a verdict: it has no leaf to judge.
    pub(crate) fn push(&mut self, row: &AuditRow) -> Result<(), serde_json::Error> {
        let leaf = audit_leaf_hash(row)?;
        self.frontier.push(leaf);
        apply_key_claims(&mut self.keys, row);
        self.rebindings.observe(row);
        self.settle();
        Ok(())
    }

    /// The verdict, and the role rebindings among the rows fed.
    pub(crate) fn finish(self) -> (TreeVerification, RebindingFold) {
        let unreached = self.checkpoints.get(self.held);
        let prev_hash = self.held_hash();
        let rows = self.frontier.len();
        let verdict =
            match (self.failure, unreached) {
                (Some(failure), _) => failure,
                (None, Some(cp)) => {
                    chain_failure(cp, prev_hash).unwrap_or_else(|| TreeVerification::Tampered {
                        tree_size: cp.tree_size,
                        recorded_root: cp.root_hash,
                        recomputed_root: format!("only {rows} rows present"),
                    })
                }
                (None, None) => self.unauthorized.or(self.anchor_unauthorized).unwrap_or(
                    TreeVerification::Intact {
                        checkpoints: self.checkpoints.len(),
                        tree_size: self.checkpoints.last().map_or(0, |c| c.tree_size),
                    },
                ),
            };
        (verdict, self.rebindings)
    }

    fn held_hash(&self) -> Option<Digest> {
        self.held
            .checked_sub(1)
            .map(|i| self.checkpoints[i].checkpoint_hash)
    }

    /// Judge whatever the rows fed so far make judgeable: each checkpoint
    /// the prefix has reached, and the anchor's keys at its own size.
    fn settle(&mut self) {
        let len = self.frontier.len();
        if let Some(anchor) = self.anchor
            && anchor.tree_size as usize == len
            && self.anchor_unauthorized.is_none()
        {
            self.anchor_unauthorized = unauthorized_signature(anchor, &self.keys);
        }
        while self.failure.is_none()
            && let Some(cp) = self.checkpoints.get(self.held)
            && cp.tree_size as usize <= len
        {
            self.failure = self.checkpoint_failure(cp, len);
            if self.failure.is_none() {
                if self.unauthorized.is_none() {
                    self.unauthorized = unauthorized_signature(cp, &self.keys);
                }
                self.held += 1;
            }
        }
    }

    fn checkpoint_failure(&self, cp: &Checkpoint, len: usize) -> Option<TreeVerification> {
        if let Some(broken) = chain_failure(cp, self.held_hash()) {
            return Some(broken);
        }
        if (cp.tree_size as usize) < len {
            return Some(TreeVerification::ChainBroken {
                detail: format!(
                    "checkpoint at tree_size {} follows one at tree_size {len}",
                    cp.tree_size
                ),
            });
        }
        let recomputed = Digest::from_bytes(self.frontier.root());
        if recomputed != cp.root_hash {
            return Some(TreeVerification::Tampered {
                tree_size: cp.tree_size,
                recorded_root: cp.root_hash,
                recomputed_root: recomputed.to_string(),
            });
        }
        signature_crypto_violation(cp)
    }
}

/// A coordinated rewrite is internally consistent, so only the external
/// copy can expose it. Its own signatures must verify, even if the stored
/// copy had its signatures stripped; its authority is judged later.
fn anchor_failure(checkpoints: &[Checkpoint], anchor: &Checkpoint) -> Option<TreeVerification> {
    let stored_at_size = checkpoints.iter().find(|c| c.tree_size == anchor.tree_size);
    if !stored_at_size.is_some_and(|c| same_tree_head(c, anchor)) {
        return Some(TreeVerification::AnchorMismatch {
            tree_size: anchor.tree_size,
            anchor_checkpoint_hash: anchor.checkpoint_hash,
            stored_checkpoint_hash: stored_at_size.map(|c| c.checkpoint_hash),
        });
    }
    signature_crypto_violation(anchor)
}

/// A checkpoint whose contents do not hash to its identity, or which does
/// not link to the one before it.
fn chain_failure(cp: &Checkpoint, prev_hash: Option<Digest>) -> Option<TreeVerification> {
    let expected = checkpoint_hash(
        cp.tree_size,
        &cp.root_hash,
        cp.prev_checkpoint_hash.as_ref(),
    );
    if expected != cp.checkpoint_hash {
        return Some(TreeVerification::ChainBroken {
            detail: format!(
                "checkpoint at tree_size {} has hash {} but its contents hash to {expected}",
                cp.tree_size, cp.checkpoint_hash
            ),
        });
    }
    if cp.prev_checkpoint_hash != prev_hash {
        return Some(TreeVerification::ChainBroken {
            detail: format!(
                "checkpoint at tree_size {} links to prev {:?}, expected {:?}",
                cp.tree_size,
                cp.prev_checkpoint_hash.as_ref(),
                prev_hash.as_ref()
            ),
        });
    }
    None
}

#[cfg(test)]
mod tests;
