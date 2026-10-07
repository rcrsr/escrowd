//! The history's record of one decision: a protocol `Decided` message, so a reviewer
//! reads the session's earlier decisions as the protocol defines them
//! (`GetHeldResponse.history`). `history.rs` stores the bytes; this module writes
//! and reads them.

use prost::Message;

use crate::convert;
use crate::decision::Outcome;
use crate::history::Fate;
use crate::proto::{self, Decided, OutcomeStatus};
use crate::review::TierReview;
use crate::views::Identity;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The record of `who`'s decision: its change set as decided (diff included; None for
/// an open scope), its outcome and each tier's review.
pub fn encode(
    who: &Identity,
    change_set: Option<&proto::ChangeSet>,
    outcome: Outcome,
    reviews: &[TierReview],
) -> Vec<u8> {
    Decided {
        scope_id: who.id.clone(),
        name: who.name.clone(),
        change_set: change_set.cloned(),
        outcome: Some(convert::outcome(outcome)),
        reviews: convert::tier_reviews(reviews),
        decided_at_ms: now_ms(),
    }
    .encode_to_vec()
}

/// The outcome a record holds; None if it is unreadable.
pub fn outcome(entry: &[u8]) -> Option<Outcome> {
    Decided::decode(entry).ok()?.outcome.and_then(convert::outcome_from)
}

/// A pending record as its scope's fate at start shows it applied, re-encoded; None:
/// it was not applied (or is unreadable), so the history cancels it.
pub fn settled(entry: &[u8], fate: Fate) -> Option<Vec<u8>> {
    let mut d = Decided::decode(entry).ok()?;
    let out = d.outcome.as_mut()?;
    let conflict = |out: &mut proto::Outcome, reopened: bool| {
        out.set_status(OutcomeStatus::Conflict);
        out.paths.clear();
        out.reasons = vec!["conflict: the project changed since the scope's snapshot".into()];
        out.reopened = reopened;
    };
    match (out.status(), fate) {
        (OutcomeStatus::Committed, Fate::Committed)
        | (OutcomeStatus::Returned, Fate::Open)
        | (OutcomeStatus::Discarded, Fate::Gone) => {}
        // A commit that found a conflict reopened or dropped the scope.
        (OutcomeStatus::Committed, Fate::Open) => conflict(out, true),
        (OutcomeStatus::Committed, Fate::Gone) => conflict(out, false),
        _ => return None,
    }
    Some(d.encode_to_vec())
}
