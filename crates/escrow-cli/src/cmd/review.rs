//! `escrow review`: list held scopes, show one, give a tier's verdict.

use std::io::Write;
use std::path::PathBuf;

use clap::{Subcommand, ValueEnum};
use escrowd::proto::reviewer_service_client::ReviewerServiceClient;
use escrowd::proto::{GetHeldRequest, HeldScope, ListHeldRequest, OutcomeStatus, ReviewRequest, Tier, Verdict};
use escrowd::rpc::review_socket;

use crate::client::{NOT_COMMITTED, channel, kind_name, socket_of, status_name, tiers, verdict_name};

#[derive(Subcommand)]
pub(crate) enum ReviewAction {
    /// The held scopes, oldest hold first.
    List,
    /// A held scope: its tiers, verdicts so far, changes with their writers, session
    /// history and diff.
    Show { scope: String },
    /// Commit the scope, as far as this tier goes.
    Commit(VerdictArgs),
    /// Discard the scope.
    Discard(VerdictArgs),
    /// Return the scope to its agent with the reasons.
    Return(VerdictArgs),
}

#[derive(clap::Args)]
pub(crate) struct VerdictArgs {
    scope: String,
    /// The tier giving the verdict; a human's also stands for the cheaper tiers pending.
    #[arg(long, value_enum, default_value = "human")]
    tier: ReviewTier,
    /// A reason for the verdict (repeatable); a return sends them to the agent.
    #[arg(long = "reason")]
    reasons: Vec<String>,
    /// Loosen the verdict so far (human tier only; the ledger records it).
    #[arg(long = "override")]
    over: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum ReviewTier {
    Llm,
    Human,
}

fn held_line(h: &HeldScope, now_ms: u64) -> String {
    let age = now_ms.saturating_sub(h.held_at_ms) / 1000;
    let or_dash = |s: &str| if s.is_empty() { "-".to_string() } else { s.to_string() };
    format!(
        "{} tiers={}{} verdict={} session={} held={age}s name={}",
        h.scope_id,
        tiers(&h.tiers),
        if h.wait { " wait" } else { "" },
        verdict_name(h.verdict),
        or_dash(&h.session),
        or_dash(&h.name),
    )
}

/// Exit code of `escrow review`: 124 when a commit verdict ended with the scope conflicted,
/// discarded or returned; a scope still held for another tier reads 0.
pub(crate) async fn run(
    socket: Option<PathBuf>,
    project: Option<PathBuf>,
    action: ReviewAction,
) -> anyhow::Result<i32> {
    let socket = review_socket(&socket_of(socket, project)?);
    let mut client = ReviewerServiceClient::new(channel(&socket).await?).max_decoding_message_size(usize::MAX);
    let err = anyhow::Error::from;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let mut out = std::io::stdout().lock();
    let (args, verdict) = match action {
        ReviewAction::List => {
            let held = client.list_held(ListHeldRequest {}).await.map_err(err)?.into_inner();
            for h in &held.scopes {
                writeln!(out, "{}", held_line(h, now_ms))?;
            }
            return Ok(0);
        }
        ReviewAction::Show { scope } => {
            let got = client
                .get_held(GetHeldRequest { scope_id: scope })
                .await
                .map_err(err)?
                .into_inner();
            let (h, cs) = (got.held.unwrap_or_default(), got.change_set.unwrap_or_default());
            writeln!(out, "{}", held_line(&h, now_ms))?;
            for (k, v) in &h.labels {
                writeln!(out, "label {k}={v}")?;
            }
            for r in &h.reviews {
                writeln!(
                    out,
                    "review {}: {}{}",
                    tiers(&[r.tier]),
                    verdict_name(r.verdict),
                    if r.r#override { " (override)" } else { "" }
                )?;
                for reason in &r.reasons {
                    writeln!(out, "  {reason}")?;
                }
            }
            let review = cs.review.unwrap_or_default();
            for reason in &review.reasons {
                writeln!(out, "software: {reason}")?;
            }
            let procs: std::collections::HashMap<u64, &escrowd::proto::Process> =
                cs.processes.iter().map(|p| (p.id, p)).collect();
            writeln!(out, "changes:")?;
            for c in &cs.changes {
                let kind = kind_name(c.kind);
                let from = if c.from_path.is_empty() {
                    String::new()
                } else {
                    format!(" from {}", c.from_path)
                };
                writeln!(out, "  {kind} {}{from}", c.path)?;
                for w in &c.writers {
                    // The writer, then its parents.
                    let mut next = procs.get(w).copied();
                    let mut by = "by";
                    while let Some(p) = next {
                        writeln!(out, "    {by} {} pid={} {}", p.program, p.pid, p.args.join(" "))?;
                        next = procs.get(&p.parent).copied();
                        by = "  from";
                    }
                }
            }
            if !got.history.is_empty() {
                writeln!(out, "session {} earlier:", h.session)?;
            }
            for d in &got.history {
                let o = d.outcome.clone().unwrap_or_default();
                let n = d.change_set.as_ref().map_or(0, |c| c.changes.len());
                writeln!(
                    out,
                    "  {} {} {} changes name={}",
                    d.scope_id,
                    status_name(o.status),
                    n,
                    d.name
                )?;
                for r in &d.reviews {
                    writeln!(out, "    review {}: {}", tiers(&[r.tier]), verdict_name(r.verdict))?;
                }
                for reason in &o.reasons {
                    writeln!(out, "    {reason}")?;
                }
            }
            writeln!(out, "diff:")?;
            out.write_all(cs.diff.as_bytes())?;
            return Ok(0);
        }
        ReviewAction::Commit(a) => (a, Verdict::Commit),
        ReviewAction::Discard(a) => (a, Verdict::Discard),
        ReviewAction::Return(a) => (a, Verdict::Return),
    };
    let tier = match args.tier {
        ReviewTier::Llm => Tier::Llm,
        ReviewTier::Human => Tier::Human,
    };
    let o = client
        .review(ReviewRequest {
            scope_id: args.scope,
            tier: tier.into(),
            verdict: verdict.into(),
            reasons: args.reasons,
            r#override: args.over,
        })
        .await
        .map_err(err)?
        .into_inner()
        .outcome
        .unwrap_or_default();
    // Unknown statuses read as held (protocol.md, versioning).
    match OutcomeStatus::try_from(o.status) {
        Ok(OutcomeStatus::Held) | Err(_) => writeln!(out, "{} held for {}", o.scope_id, tiers(&o.tiers))?,
        Ok(_) => writeln!(out, "{} {}", o.scope_id, status_name(o.status))?,
    }
    for p in &o.paths {
        writeln!(out, "  {p}")?;
    }
    for r in &o.reasons {
        writeln!(out, "  {r}")?;
    }
    let not_committed = matches!(
        OutcomeStatus::try_from(o.status),
        Ok(OutcomeStatus::Conflict | OutcomeStatus::Discarded | OutcomeStatus::Returned)
    );
    Ok(if verdict == Verdict::Commit && not_committed {
        NOT_COMMITTED
    } else {
        0
    })
}
