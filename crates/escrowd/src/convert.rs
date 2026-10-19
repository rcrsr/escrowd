//! The protocol's messages from the daemon's own types, and back: the one place
//! that knows both. The RPC layer and the history's record format use it.

use std::path::Path;

use crate::changeset;
use crate::decision::{Outcome, Status};
use crate::policy;
use crate::proto;
use crate::review::{self, Decision};
use crate::views::{Identity, Views};

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The change set with each path as the caller sees it (`~/x` in `$HOME`).
pub fn change_set(views: &Views, scope_id: String, cs: changeset::ChangeSet, diff: String) -> proto::ChangeSet {
    let kind = |k| match k {
        changeset::Kind::Create => proto::ChangeKind::Create,
        changeset::Kind::Modify => proto::ChangeKind::Modify,
        changeset::Kind::Delete => proto::ChangeKind::Delete,
        changeset::Kind::Rename => proto::ChangeKind::Rename,
    };
    proto::ChangeSet {
        scope_id,
        changes: cs
            .changes
            .into_iter()
            .map(|c| proto::Change {
                kind: kind(c.kind).into(),
                path: path_str(&views.shown(c.root, &c.path)),
                from_path: c
                    .from
                    .as_deref()
                    .map(|f| path_str(&views.shown(c.root, f)))
                    .unwrap_or_default(),
                writers: c.writers,
            })
            .collect(),
        reads: cs
            .reads
            .into_iter()
            .map(|(p, allowed)| proto::Read {
                path: path_str(&p),
                decision: if allowed {
                    proto::ReadDecision::Allow
                } else {
                    proto::ReadDecision::Deny
                }
                .into(),
            })
            .collect(),
        labels: cs.labels,
        diff,
        unscoped_ops: cs.unscoped,
        review: Some(review(cs.review)),
        processes: cs
            .procs
            .into_values()
            .map(|p| proto::Process {
                id: p.id,
                pid: p.pid,
                program: path_str(&p.program),
                dev: p.dev,
                ino: p.ino,
                args: p.args,
                parent: p.parent,
            })
            .collect(),
    }
}

pub fn tier(t: policy::Tier) -> proto::Tier {
    match t {
        policy::Tier::Software => proto::Tier::Software,
        policy::Tier::Llm => proto::Tier::Llm,
        policy::Tier::Human => proto::Tier::Human,
    }
}

/// The tiers a reviewer can give a verdict for.
pub fn reviewer_tier(t: i32) -> Option<policy::Tier> {
    match proto::Tier::try_from(t).ok()? {
        proto::Tier::Llm => Some(policy::Tier::Llm),
        proto::Tier::Human => Some(policy::Tier::Human),
        proto::Tier::Software | proto::Tier::Unspecified => None,
    }
}

pub fn verdict(d: Decision) -> proto::Verdict {
    match d {
        Decision::Commit => proto::Verdict::Commit,
        Decision::Return => proto::Verdict::Return,
        Decision::Discard => proto::Verdict::Discard,
    }
}

/// None for `VERDICT_UNSPECIFIED` and values this daemon does not know.
pub fn decision(v: i32) -> Option<Decision> {
    match proto::Verdict::try_from(v).ok()? {
        proto::Verdict::Commit => Some(Decision::Commit),
        proto::Verdict::Return => Some(Decision::Return),
        proto::Verdict::Discard => Some(Decision::Discard),
        proto::Verdict::Unspecified => None,
    }
}

pub fn tier_reviews(rs: &[review::TierReview]) -> Vec<proto::TierReview> {
    rs.iter()
        .map(|r| proto::TierReview {
            tier: tier(r.tier).into(),
            verdict: verdict(r.verdict).into(),
            reasons: r.reasons.clone(),
            r#override: r.over,
        })
        .collect()
}

pub fn held(i: Identity) -> proto::HeldScope {
    let (tiers, wait, verdict_so_far, reviews, at) = match &i.hold {
        Some(h) => (h.tiers.as_slice(), h.wait, h.verdict, h.reviews.as_slice(), h.at_ms),
        None => (&[][..], false, Decision::Commit, &[][..], 0),
    };
    proto::HeldScope {
        tiers: tiers.iter().map(|t| tier(*t).into()).collect(),
        wait,
        verdict: verdict(verdict_so_far).into(),
        reviews: tier_reviews(reviews),
        held_at_ms: at,
        scope_id: i.id,
        name: i.name,
        labels: i.labels,
        session: i.session,
    }
}

pub fn review(r: review::Review) -> proto::Review {
    proto::Review {
        verdict: verdict(r.verdict).into(),
        reasons: r.reasons(),
        tiers: r.tiers.into_iter().map(|t| tier(t).into()).collect(),
        wait_required: r.wait,
    }
}

pub fn outcome(o: Outcome) -> proto::Outcome {
    let status = match o.status {
        Status::Committed => proto::OutcomeStatus::Committed,
        Status::Discarded => proto::OutcomeStatus::Discarded,
        Status::Returned => proto::OutcomeStatus::Returned,
        Status::Conflict => proto::OutcomeStatus::Conflict,
        Status::Held => proto::OutcomeStatus::Held,
    };
    proto::Outcome {
        scope_id: o.scope_id,
        status: status.into(),
        paths: o.paths,
        reasons: o.reasons,
        reopened: o.reopened,
        tiers: o.tiers.into_iter().map(|t| tier(t).into()).collect(),
        wait: o.wait,
    }
}

/// None for a status this daemon does not know (a later daemon's history).
pub fn outcome_from(o: proto::Outcome) -> Option<Outcome> {
    let status = match o.status() {
        proto::OutcomeStatus::Committed => Status::Committed,
        proto::OutcomeStatus::Discarded => Status::Discarded,
        proto::OutcomeStatus::Returned => Status::Returned,
        proto::OutcomeStatus::Conflict => Status::Conflict,
        proto::OutcomeStatus::Held => Status::Held,
        proto::OutcomeStatus::Unspecified => return None,
    };
    let tiers = o
        .tiers
        .iter()
        .filter_map(|t| match proto::Tier::try_from(*t).ok()? {
            proto::Tier::Software => Some(policy::Tier::Software),
            proto::Tier::Llm => Some(policy::Tier::Llm),
            proto::Tier::Human => Some(policy::Tier::Human),
            proto::Tier::Unspecified => None,
        })
        .collect();
    Some(Outcome {
        scope_id: o.scope_id,
        status,
        paths: o.paths,
        reasons: o.reasons,
        reopened: o.reopened,
        tiers,
        wait: o.wait,
    })
}
