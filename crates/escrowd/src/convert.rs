//! The protocol's messages from the daemon's own types, and back: the one place
//! that knows both. The RPC layer and the history's record format use it.

use std::collections::HashMap;
use std::path::Path;

use prost::Message;
use serde::Serialize;

use crate::changeset;
use crate::decision::{Outcome, Status};
use crate::policy::valid_reviewer_name;
use crate::proto;
use crate::review::{self, Decision};
use crate::store::Hold;
use crate::views::{Identity, Views};

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The change set with each path as the caller sees it (`~/x` in `$HOME`).
pub fn change_set(views: &Views, scope_id: String, cs: &changeset::ChangeSet, diff: &str) -> proto::ChangeSet {
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
            .iter()
            .map(|c| proto::Change {
                kind: kind(c.kind).into(),
                path: path_str(&views.shown(c.root, &c.path)),
                from_path: c
                    .from
                    .as_deref()
                    .map(|f| path_str(&views.shown(c.root, f)))
                    .unwrap_or_default(),
                writers: c.writers.clone(),
            })
            .collect(),
        reads: cs
            .reads
            .iter()
            .map(|(p, allowed)| proto::Read {
                path: path_str(p),
                decision: if *allowed {
                    proto::ReadDecision::Allow
                } else {
                    proto::ReadDecision::Deny
                }
                .into(),
            })
            .collect(),
        labels: cs.labels.clone(),
        diff: diff.to_string(),
        unscoped_ops: cs.unscoped,
        review: Some(review(&cs.review)),
        processes: cs
            .procs
            .values()
            .map(|p| proto::Process {
                id: p.id,
                pid: p.pid,
                program: path_str(&p.program),
                dev: p.dev,
                ino: p.ino,
                args: p.args.clone(),
                parent: p.parent,
            })
            .collect(),
    }
}

/// The protocol tier of a built-in reviewer; None for any other name.
fn builtin_tier(name: &str) -> Option<proto::Tier> {
    match name {
        "llm" => Some(proto::Tier::Llm),
        "human" => Some(proto::Tier::Human),
        _ => None,
    }
}

/// The built-in reviewer a protocol tier names; None for the others.
fn builtin_name(tier: i32) -> Option<&'static str> {
    match proto::Tier::try_from(tier).ok()? {
        proto::Tier::Llm => Some("llm"),
        proto::Tier::Human => Some("human"),
        proto::Tier::Software | proto::Tier::Unspecified => None,
    }
}

/// The `tiers` of a message for `reviewers`: the built-in ones, in order.
fn tiers_of(reviewers: &[String]) -> Vec<i32> {
    reviewers
        .iter()
        .filter_map(|n| builtin_tier(n))
        .map(Into::into)
        .collect()
}

/// The reviewer and token of a `ReviewRequest` (`tier`, `reviewer`, `token`). An empty
/// `reviewer` is the built-in the tier names; a `tier` given beside a name must agree
/// with it. The error is the text of an INVALID_ARGUMENT.
pub fn reviewer_request(tier: i32, reviewer: &str, token: &str) -> Result<(String, Option<String>), String> {
    let token = (!token.is_empty()).then(|| token.to_string());
    if reviewer.is_empty() {
        let name = builtin_name(tier).ok_or("tier must be llm or human")?;
        return Ok((name.to_string(), token));
    }
    if !valid_reviewer_name(reviewer) {
        return Err(format!("invalid reviewer name {reviewer:?}"));
    }
    if tier != i32::from(proto::Tier::Unspecified) && builtin_name(tier) != Some(reviewer) {
        return Err(format!("tier disagrees with reviewer {reviewer}"));
    }
    Ok((reviewer.to_string(), token))
}

/// The contract name in the request a command reviewer reads on stdin.
const REVIEW_CONTRACT: &str = "escrow.review.v1";

#[derive(Serialize)]
struct ReviewRequest<'a> {
    contract: &'static str,
    scope: RequestScope<'a>,
    reviewer: &'a str,
    attempt: u32,
    verdict_so_far: &'static str,
    reviews: Vec<RequestReview<'a>>,
    change_set: &'a proto::ChangeSet,
    session_history: Vec<proto::Decided>,
}

#[derive(Serialize)]
struct RequestScope<'a> {
    id: &'a str,
    name: &'a str,
    session: &'a str,
    labels: &'a HashMap<String, String>,
}

#[derive(Serialize)]
struct RequestReview<'a> {
    reviewer: &'a str,
    verdict: &'static str,
    reasons: &'a [String],
}

/// The JSON a command reviewer reads on stdin: the scope, the reviews so far, the change
/// set and the session's earlier decisions (`history`, as `record` encodes them; one
/// that does not decode is left out), the messages in protobuf JSON with the `.proto`
/// field names. It carries no token.
pub fn review_request(
    who: &Identity,
    reviewer: &str,
    attempt: u32,
    hold: &Hold,
    change_set: &proto::ChangeSet,
    history: &[Vec<u8>],
) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&ReviewRequest {
        contract: REVIEW_CONTRACT,
        scope: RequestScope {
            id: &who.id,
            name: &who.name,
            session: &who.session,
            labels: &who.labels,
        },
        reviewer,
        attempt,
        verdict_so_far: hold.verdict.as_str(),
        reviews: hold
            .reviews
            .iter()
            .map(|r| RequestReview {
                reviewer: &r.reviewer,
                verdict: r.verdict.as_str(),
                reasons: &r.reasons,
            })
            .collect(),
        change_set,
        session_history: history
            .iter()
            .filter_map(|e| proto::Decided::decode(&e[..]).ok())
            .collect(),
    })
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
            tier: builtin_tier(&r.reviewer).unwrap_or(proto::Tier::Unspecified).into(),
            reviewer: r.reviewer.clone(),
            verdict: verdict(r.verdict).into(),
            reasons: r.reasons.clone(),
            r#override: r.over,
        })
        .collect()
}

pub fn held(i: Identity) -> proto::HeldScope {
    let (reviewers, wait, verdict_so_far, reviews, at) = match &i.hold {
        Some(h) => (h.reviewers.as_slice(), h.wait, h.verdict, h.reviews.as_slice(), h.at_ms),
        None => (&[][..], false, Decision::Commit, &[][..], 0),
    };
    proto::HeldScope {
        tiers: tiers_of(reviewers),
        reviewers: reviewers.to_vec(),
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

pub fn review(r: &review::Review) -> proto::Review {
    proto::Review {
        verdict: verdict(r.verdict).into(),
        reasons: r.reasons(),
        tiers: tiers_of(&r.reviewers),
        wait_required: r.wait,
        reviewers: r.reviewers.clone(),
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
        tiers: tiers_of(&o.reviewers),
        reviewers: o.reviewers,
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
    // A protocol-7 history entry has tiers and no reviewers.
    let reviewers = match o.reviewers.is_empty() {
        true => o
            .tiers
            .iter()
            .filter_map(|t| builtin_name(*t))
            .map(str::to_string)
            .collect(),
        false => o.reviewers,
    };
    Some(Outcome {
        scope_id: o.scope_id,
        status,
        paths: o.paths,
        reasons: o.reasons,
        reopened: o.reopened,
        reviewers,
        wait: o.wait,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LLM: i32 = proto::Tier::Llm as i32;
    const HUMAN: i32 = proto::Tier::Human as i32;
    const SOFTWARE: i32 = proto::Tier::Software as i32;

    fn request(tier: i32, reviewer: &str, token: &str) -> Result<(String, Option<String>), String> {
        reviewer_request(tier, reviewer, token)
    }

    #[test]
    fn an_empty_reviewer_is_the_built_in_the_tier_names() {
        assert_eq!(request(LLM, "", ""), Ok(("llm".into(), None)));
        assert_eq!(request(HUMAN, "", "tok"), Ok(("human".into(), Some("tok".into()))));
        for tier in [SOFTWARE, 0, 99] {
            assert_eq!(request(tier, "", ""), Err("tier must be llm or human".into()));
        }
    }

    #[test]
    fn a_named_reviewer_may_leave_the_tier_unset_but_not_contradict_it() {
        assert_eq!(request(0, "security", ""), Ok(("security".into(), None)));
        assert_eq!(request(LLM, "llm", ""), Ok(("llm".into(), None)));
        assert!(request(HUMAN, "llm", "").unwrap_err().contains("disagrees"));
        assert!(request(LLM, "security", "").unwrap_err().contains("disagrees"));
        assert!(request(SOFTWARE, "software", "").is_err());
        assert!(
            request(0, "Not Valid", "")
                .unwrap_err()
                .contains("invalid reviewer name")
        );
    }

    #[test]
    fn tiers_follow_the_built_in_names_only() {
        let names: Vec<String> = ["security", "llm", "human"].map(String::from).to_vec();
        assert_eq!(tiers_of(&names), [LLM, HUMAN]);
        let reviews = tier_reviews(&[
            review::TierReview {
                reviewer: "security".into(),
                verdict: Decision::Return,
                reasons: vec![],
                over: false,
            },
            review::TierReview {
                reviewer: "human".into(),
                verdict: Decision::Commit,
                reasons: vec![],
                over: true,
            },
        ]);
        assert_eq!((reviews[0].tier, reviews[0].reviewer.as_str()), (0, "security"));
        assert_eq!((reviews[1].tier, reviews[1].reviewer.as_str()), (HUMAN, "human"));
    }

    #[test]
    fn an_outcome_round_trips_its_reviewers_and_derives_the_tiers() {
        let mut out = Outcome {
            scope_id: "s1".into(),
            status: Status::Held,
            paths: vec![],
            reasons: vec![],
            reopened: false,
            reviewers: vec!["security".into(), "llm".into()],
            wait: true,
        };
        let wire = outcome(out.clone());
        assert_eq!(
            (wire.tiers.clone(), wire.reviewers.clone()),
            (vec![LLM], out.reviewers.clone())
        );
        assert_eq!(outcome_from(wire), Some(out.clone()));
        out.reviewers.clear();
        assert_eq!(outcome_from(outcome(out.clone())), Some(out));
    }

    #[test]
    fn a_protocol_7_outcome_with_tiers_only_reads_as_reviewer_names() {
        let old = proto::Outcome {
            scope_id: "s1".into(),
            status: proto::OutcomeStatus::Held.into(),
            tiers: vec![LLM, HUMAN],
            wait: true,
            ..Default::default()
        };
        let got = outcome_from(old).unwrap();
        assert_eq!(got.reviewers, ["llm", "human"]);
        assert!(got.wait && got.status == Status::Held);
    }

    #[test]
    fn a_change_set_round_trips_through_protobuf_json_with_proto_names() {
        let set = proto::ChangeSet {
            scope_id: "s1".into(),
            changes: vec![proto::Change {
                kind: proto::ChangeKind::Rename as i32,
                path: "b.txt".into(),
                from_path: "a.txt".into(),
                writers: vec![7],
            }],
            labels: [("k".to_string(), "v".to_string())].into(),
            review: Some(proto::Review {
                verdict: proto::Verdict::Commit as i32,
                reviewers: vec!["security".into(), "llm".into()],
                ..Default::default()
            }),
            ..Default::default()
        };

        let json = serde_json::to_value(&set).unwrap();

        assert_eq!(json["scope_id"], "s1");
        assert_eq!(json["changes"][0]["from_path"], "a.txt");
        assert_eq!(json["changes"][0]["kind"], "CHANGE_KIND_RENAME");
        assert_eq!(json["changes"][0]["writers"][0], "7", "64-bit integers are strings");
        assert_eq!(json["unscoped_ops"], "0", "defaults are emitted");
        assert_eq!(json["reads"], serde_json::json!([]));
        assert_eq!(json["review"]["reviewers"][0], "security");
        let back: proto::ChangeSet = serde_json::from_value(json).unwrap();
        assert_eq!(back, set);
    }

    #[test]
    fn a_review_request_carries_the_contract_the_reviews_and_the_history_in_proto_names() {
        let who = Identity {
            id: "s7".into(),
            name: "fix".into(),
            session: "agent".into(),
            token_sha256: Some("hash".into()),
            labels: [("k".to_string(), "v".to_string())].into(),
            hold: None,
        };
        let hold = Hold {
            reviewers: vec!["lint".into()],
            wait: false,
            at_ms: 1,
            verdict: Decision::Return,
            reviews: vec![review::TierReview {
                reviewer: "sec".into(),
                verdict: Decision::Return,
                reasons: vec!["no".into()],
                over: false,
            }],
            run: None,
        };
        let set = proto::ChangeSet {
            scope_id: "s7".into(),
            diff: "+x\n".into(),
            ..Default::default()
        };
        let earlier = proto::Decided {
            scope_id: "s3".into(),
            decided_at_ms: 9,
            ..Default::default()
        };

        let bytes = review_request(&who, "lint", 2, &hold, &set, &[earlier.encode_to_vec(), vec![0xff]]).unwrap();

        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["contract"], "escrow.review.v1");
        assert_eq!(
            json["scope"],
            serde_json::json!({"id": "s7", "name": "fix", "session": "agent", "labels": {"k": "v"}})
        );
        assert_eq!(
            (json["reviewer"].as_str(), json["attempt"].as_u64()),
            (Some("lint"), Some(2))
        );
        assert_eq!(json["verdict_so_far"], "return");
        assert_eq!(
            json["reviews"],
            serde_json::json!([{"reviewer": "sec", "verdict": "return", "reasons": ["no"]}])
        );
        assert_eq!(json["change_set"]["diff"], "+x\n");
        assert_eq!(
            json["session_history"].as_array().map(Vec::len),
            Some(1),
            "an unreadable entry is left out"
        );
        assert_eq!(json["session_history"][0]["scope_id"], "s3");
        assert!(!String::from_utf8_lossy(&bytes).contains("hash"), "no token material");
    }
}
