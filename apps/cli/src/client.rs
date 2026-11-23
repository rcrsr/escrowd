//! What every client command shares: finding the daemon's socket, connecting to it and
//! rendering protocol enums as names.

use std::path::{Path, PathBuf};

use anyhow::Context;
use escrowd::daemon::default_runtime_dir;
use escrowd::proto::{ChangeKind, OutcomeStatus, Tier, Verdict};

/// The socket of `--socket`, or of `escrow run --project`.
pub(crate) fn socket_of(socket: Option<PathBuf>, project: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    Ok(match (socket, project) {
        (Some(s), _) => s,
        (None, Some(p)) => default_runtime_dir(&p.canonicalize()?)?.join("escrow.sock"),
        (None, None) => anyhow::bail!("pass --socket (or ESCROW_SOCKET) or --project"),
    })
}

/// Exit code of a commit that ended without committing (conflict, discard, return).
pub(crate) const NOT_COMMITTED: i32 = 124;

/// Exit code of a failure of `escrow run` or `escrow exec` itself, not of its child.
pub(crate) const OWN_FAILURE: i32 = 125;

/// The canonical name of a gRPC code (`FAILED_PRECONDITION`), as `docs/protocol.md` writes
/// it; tonic's `Display` for a code is a sentence.
fn code_name(code: tonic::Code) -> &'static str {
    use tonic::Code::*;
    match code {
        Ok => "OK",
        Cancelled => "CANCELLED",
        Unknown => "UNKNOWN",
        InvalidArgument => "INVALID_ARGUMENT",
        DeadlineExceeded => "DEADLINE_EXCEEDED",
        NotFound => "NOT_FOUND",
        AlreadyExists => "ALREADY_EXISTS",
        PermissionDenied => "PERMISSION_DENIED",
        ResourceExhausted => "RESOURCE_EXHAUSTED",
        FailedPrecondition => "FAILED_PRECONDITION",
        Aborted => "ABORTED",
        OutOfRange => "OUT_OF_RANGE",
        Unimplemented => "UNIMPLEMENTED",
        Internal => "INTERNAL",
        Unavailable => "UNAVAILABLE",
        DataLoss => "DATA_LOSS",
        Unauthenticated => "UNAUTHENTICATED",
    }
}

/// The one-line message of a failed command: `escrow <cmd>: <CODE>: <message>` when the
/// error carries a daemon status, else `escrow <cmd>: <error chain>`.
fn failure_line(cmd: &str, err: &anyhow::Error) -> String {
    match err.chain().find_map(|c| c.downcast_ref::<tonic::Status>()) {
        Some(s) => format!("escrow {cmd}: {}: {}", code_name(s.code()), s.message()),
        None => format!("escrow {cmd}: {err:#}"),
    }
}

/// Print a command's failure to stderr and exit with `code`.
pub(crate) fn fail(cmd: &str, err: anyhow::Error, code: i32) -> ! {
    eprintln!("{}", failure_line(cmd, &err));
    std::process::exit(code)
}

pub(crate) async fn channel(socket: &Path) -> anyhow::Result<tonic::transport::Channel> {
    let path = socket.to_path_buf();
    tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_| {
            let path = path.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(path).await?,
                ))
            }
        }))
        .await
        .with_context(|| format!("connecting to {}", socket.display()))
}

/// A protocol enum's name without its prefix, lower case (`OUTCOME_STATUS_HELD` ->
/// `held`); a number outside the v1 enum reads `unknown`.
fn name(full: Option<&str>, prefix: &str) -> String {
    full.map_or_else(
        || "unknown".to_string(),
        |f| f.strip_prefix(prefix).unwrap_or(f).to_lowercase(),
    )
}

pub(crate) fn tiers(ts: &[i32]) -> String {
    let names: Vec<String> = ts
        .iter()
        .map(|t| name(Tier::try_from(*t).ok().map(|t| t.as_str_name()), "TIER_"))
        .collect();
    names.join(",")
}

/// The reviewers of a hold, outcome or review by name; a daemon older than protocol 8
/// names none, so its tiers stand in.
pub(crate) fn reviewer_names(reviewers: &[String], tier_values: &[i32]) -> String {
    if reviewers.is_empty() {
        tiers(tier_values)
    } else {
        reviewers.join(",")
    }
}

/// One review's reviewer: its name, else (an older daemon) its tier.
pub(crate) fn reviewer_of(reviewer: &str, tier: i32) -> String {
    if reviewer.is_empty() {
        tiers(&[tier])
    } else {
        reviewer.to_string()
    }
}

pub(crate) fn verdict_name(v: i32) -> String {
    name(Verdict::try_from(v).ok().map(|v| v.as_str_name()), "VERDICT_")
}

pub(crate) fn status_name(s: i32) -> String {
    name(
        OutcomeStatus::try_from(s).ok().map(|s| s.as_str_name()),
        "OUTCOME_STATUS_",
    )
}

pub(crate) fn kind_name(k: i32) -> String {
    name(ChangeKind::try_from(k).ok().map(|k| k.as_str_name()), "CHANGE_KIND_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_failure_names_its_code_and_message() {
        let err = anyhow::Error::from(tonic::Status::failed_precondition("scope is not closed")).context("ignored");
        assert_eq!(
            failure_line("diff", &err),
            "escrow diff: FAILED_PRECONDITION: scope is not closed"
        );
    }

    #[test]
    fn a_failure_without_a_status_prints_its_chain() {
        let err = anyhow::anyhow!("no socket").context("connecting");
        assert_eq!(failure_line("log", &err), "escrow log: connecting: no socket");
    }

    #[test]
    fn tier_outside_v1_renders_unknown() {
        assert_eq!(tiers(&[99]), "unknown");
    }

    #[test]
    fn known_tiers_render_lowercase_names_joined() {
        assert_eq!(tiers(&[Tier::Llm as i32, Tier::Human as i32]), "llm,human");
    }

    #[test]
    fn reviewer_names_join_and_an_older_daemon_falls_back_to_tiers() {
        let names = vec!["lint".to_string(), "llm".to_string()];
        assert_eq!(reviewer_names(&names, &[Tier::Llm as i32]), "lint,llm");
        assert_eq!(
            reviewer_names(&[], &[Tier::Llm as i32, Tier::Human as i32]),
            "llm,human"
        );
        assert_eq!(reviewer_names(&[], &[]), "");
    }

    #[test]
    fn a_review_names_its_reviewer_else_its_tier() {
        assert_eq!(reviewer_of("lint", Tier::Unspecified as i32), "lint");
        assert_eq!(reviewer_of("", Tier::Human as i32), "human");
    }

    #[test]
    fn verdict_outside_v1_renders_unknown() {
        assert_eq!(verdict_name(99), "unknown");
    }

    #[test]
    fn known_verdict_renders_lowercase_name() {
        assert_eq!(verdict_name(Verdict::Commit as i32), "commit");
    }

    #[test]
    fn status_outside_v1_renders_unknown() {
        assert_eq!(status_name(99), "unknown");
    }

    #[test]
    fn known_status_renders_lowercase_name() {
        assert_eq!(status_name(OutcomeStatus::Held as i32), "held");
    }

    #[test]
    fn change_kind_outside_v1_renders_unknown() {
        assert_eq!(kind_name(99), "unknown");
    }

    #[test]
    fn known_change_kind_renders_lowercase_name() {
        assert_eq!(kind_name(ChangeKind::Create as i32), "create");
    }
}
