//! The command-reviewer runner: start a reviewer's program, feed it the request
//! on stdin, and turn what it did into one answer or one failure.
//!
//! A run has no shell and its own process group. Its environment is cleared, then
//! gets PATH, HOME, LANG and the variables the reviewer's `env:` names (when set);
//! a delegated run also gets `ESCROW_REVIEW_TOKEN` and `ESCROW_REVIEW_SOCKET`. Stdin is
//! written from a separate task (a child that never reads it cannot stall the run),
//! stdout is read up to 1 MiB, and stderr is forwarded line by line to the daemon's
//! stderr. A run waits for one of `reviewers_parallel` permits before it starts; its
//! timeout counts from the spawn. At the timeout the whole group gets SIGTERM, and
//! SIGKILL 5 s later. Only an exit 0 with a valid answer counts: one JSON object
//! `{"verdict": "commit"|"return"|"discard"|"pending", "reasons": [..]?}`, other keys
//! ignored, `pending` only from a delegating reviewer.
//!
//! The runner knows nothing of scopes' state or locks: `decision.rs` decides what a
//! report means. It imports only `review` for the command and the verdict type.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use rustix::process::{Pid, Signal};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr};
use tokio::sync::{Semaphore, watch};

use crate::review::{Command, Decision};

/// The most a run may write to stdout.
const MAX_ANSWER: usize = 1 << 20;
/// Between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// How long output left in the pipes is read after the child has gone.
const DRAIN: Duration = Duration::from_secs(2);
/// A stderr line is cut here.
const MAX_LINE: usize = 8 << 10;

/// What a run is asked to do.
pub struct RunRequest {
    pub scope: String,
    pub reviewer: String,
    pub attempt: u32,
    pub cwd: PathBuf,
    /// The request JSON.
    pub stdin: Vec<u8>,
    /// The delegated run's token; None for a reviewer without `delegate`.
    pub token: Option<String>,
}

/// What a finished run left.
pub struct RunReport {
    /// None when the program never started.
    pub pid: Option<u32>,
    pub result: Result<Answer, Failure>,
}

/// A valid answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Verdict(Decision, Vec<String>),
    /// A delegating reviewer hands the verdict to someone else, with its token.
    Pending(Vec<String>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The program could not be started (the OS error).
    Spawn(String),
    Timeout,
    Exit(i32),
    Signal(i32),
    TooLarge,
    BadAnswer(String),
    PendingNotDelegate,
    Cancelled,
}

impl Failure {
    /// The text stored as `failed: <cause>`; `timeout` is the reviewer's.
    pub fn cause(&self, timeout: Duration) -> String {
        match self {
            Failure::Spawn(e) => format!("cannot start: {e}"),
            Failure::Timeout => format!("timeout after {}", format_duration(timeout)),
            Failure::Exit(n) => format!("exit {n}"),
            Failure::Signal(n) => format!("signal {n}"),
            Failure::TooLarge => "answer over 1 MiB".to_string(),
            Failure::BadAnswer(d) => format!("bad answer: {d}"),
            Failure::PendingNotDelegate => "pending from a reviewer without delegate".to_string(),
            Failure::Cancelled => "cancelled".to_string(),
        }
    }
}

/// The policy's duration syntax, in the largest whole unit.
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms.is_multiple_of(3_600_000) {
        format!("{}h", ms / 3_600_000)
    } else if ms.is_multiple_of(60_000) {
        format!("{}m", ms / 60_000)
    } else if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

#[derive(Deserialize)]
struct RawAnswer {
    verdict: String,
    #[serde(default)]
    reasons: Vec<String>,
}

/// The answer in a run's stdout. `delegate`: the reviewer may answer `pending`.
pub fn parse_answer(stdout: &[u8], delegate: bool) -> Result<Answer, Failure> {
    let raw: RawAnswer = serde_json::from_slice(stdout).map_err(|e| Failure::BadAnswer(e.to_string()))?;
    if raw.verdict == "pending" {
        return if delegate {
            Ok(Answer::Pending(raw.reasons))
        } else {
            Err(Failure::PendingNotDelegate)
        };
    }
    match Decision::parse(&raw.verdict) {
        Some(d) => Ok(Answer::Verdict(d, raw.reasons)),
        None => Err(Failure::BadAnswer(format!("unknown verdict {:?}", raw.verdict))),
    }
}

/// Runs command reviewers, at most `parallel` at once.
pub struct Runner {
    permits: Arc<Semaphore>,
    review_socket: PathBuf,
    /// The cancel switch of each scope's run (queued or going), under a run id.
    runs: Mutex<HashMap<String, (u64, watch::Sender<bool>)>>,
    next_id: AtomicU64,
}

impl Runner {
    pub fn new(parallel: usize, review_socket: PathBuf) -> Self {
        Runner {
            permits: Arc::new(Semaphore::new(parallel.max(1))),
            review_socket,
            runs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Run `cmd` for `req` and report. Never panics, never leaves the group running.
    pub async fn run(&self, cmd: &Command, req: RunRequest) -> RunReport {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, mut cancelled) = watch::channel(false);
        self.runs.lock().insert(req.scope.clone(), (id, tx));
        let report = self.run_started(cmd, &req, &mut cancelled).await;
        let mut runs = self.runs.lock();
        if runs.get(&req.scope).is_some_and(|(i, _)| *i == id) {
            runs.remove(&req.scope);
        }
        report
    }

    /// Stop the scope's run, queued or going; its report is `Cancelled`.
    pub fn cancel(&self, scope: &str) {
        if let Some((_, tx)) = self.runs.lock().get(scope) {
            tx.send_replace(true);
        }
    }

    /// Whether the scope has a run, queued or going.
    pub fn running(&self, scope: &str) -> bool {
        self.runs.lock().contains_key(scope)
    }

    /// Stop every run.
    pub fn stop_all(&self) {
        for (_, tx) in self.runs.lock().values() {
            tx.send_replace(true);
        }
    }

    async fn run_started(&self, cmd: &Command, req: &RunRequest, cancelled: &mut watch::Receiver<bool>) -> RunReport {
        let fail = |pid, f| RunReport { pid, result: Err(f) };
        let permit = tokio::select! {
            p = self.permits.clone().acquire_owned() => p,
            _ = wait_cancelled(cancelled) => return fail(None, Failure::Cancelled),
        };
        // The semaphore is never closed.
        let Ok(_permit) = permit else {
            return fail(None, Failure::Cancelled);
        };

        let mut child = match self.spawn(cmd, req) {
            Ok(c) => c,
            Err(e) => return fail(None, Failure::Spawn(e.to_string())),
        };
        let pid = child.id();
        // The program has no group before spawn returns; its pid is its group.
        let group = pid.and_then(|p| Pid::from_raw(p as i32));
        let (Some(mut stdin), Some(mut stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return fail(pid, Failure::Spawn("no pipes".to_string()));
        };
        let body = req.stdin.clone();
        let writer = tokio::spawn(async move {
            // A reviewer may exit without reading its input.
            let _ = stdin.write_all(&body).await;
        });
        let forwarder = tokio::spawn(forward_stderr(
            stderr,
            req.reviewer.clone(),
            req.scope.clone(),
            req.token.clone(),
        ));

        let timeout = tokio::time::sleep(cmd.timeout);
        tokio::pin!(timeout);
        let mut out = Vec::new();
        let mut buf = vec![0u8; 16 << 10];
        let mut eof = false;
        let stop = loop {
            tokio::select! {
                n = stdout.read(&mut buf), if !eof => match n {
                    Ok(0) | Err(_) => eof = true,
                    Ok(n) => if !take(&mut out, &buf[..n]) { break Stop::TooLarge },
                },
                st = child.wait() => break Stop::Exited(st),
                _ = &mut timeout => break Stop::Timeout,
                _ = wait_cancelled(cancelled) => break Stop::Cancelled,
            }
        };

        let result = match stop {
            Stop::Exited(status) => {
                // Anything the program left behind in its group goes with it.
                signal_group(group, Signal::KILL);
                match status {
                    Err(e) => Err(Failure::Spawn(e.to_string())),
                    Ok(status) => {
                        if !eof && !drain(&mut stdout, &mut out).await {
                            Err(Failure::TooLarge)
                        } else {
                            finish(status, &out, cmd.delegate)
                        }
                    }
                }
            }
            Stop::TooLarge => Err(terminate(&mut child, group, Failure::TooLarge).await),
            Stop::Timeout => Err(terminate(&mut child, group, Failure::Timeout).await),
            Stop::Cancelled => Err(terminate(&mut child, group, Failure::Cancelled).await),
        };
        writer.abort();
        // The pipe closes once the group is gone; do not wait on a holdout.
        if tokio::time::timeout(DRAIN, forwarder).await.is_err() {
            eprintln!("escrowd: reviewer: {} {}: stderr left open", req.reviewer, req.scope);
        }
        RunReport { pid, result }
    }

    fn spawn(&self, cmd: &Command, req: &RunRequest) -> std::io::Result<Child> {
        let mut c = tokio::process::Command::new(&cmd.program);
        if let Some((arg0, args)) = cmd.argv.split_first() {
            c.arg0(arg0).args(args);
        }
        c.env_clear().current_dir(&req.cwd);
        for name in ["PATH", "HOME", "LANG"]
            .into_iter()
            .chain(cmd.env.iter().map(String::as_str))
        {
            if let Some(v) = std::env::var_os(name) {
                c.env(name, v);
            }
        }
        if cmd.delegate
            && let Some(token) = &req.token
        {
            c.env("ESCROW_REVIEW_TOKEN", token)
                .env("ESCROW_REVIEW_SOCKET", &self.review_socket);
        }
        c.process_group(0)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
}

enum Stop {
    Exited(std::io::Result<ExitStatus>),
    TooLarge,
    Timeout,
    Cancelled,
}

/// Resolves when the run is cancelled.
async fn wait_cancelled(rx: &mut watch::Receiver<bool>) {
    // A dropped sender never cancels: the run keeps going.
    if rx.wait_for(|c| *c).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Add a chunk to the answer; false when it passes the limit.
fn take(out: &mut Vec<u8>, chunk: &[u8]) -> bool {
    if out.len() + chunk.len() > MAX_ANSWER {
        return false;
    }
    out.extend_from_slice(chunk);
    true
}

/// Read what is left in stdout after the child exited; false when it passes the limit.
async fn drain(stdout: &mut tokio::process::ChildStdout, out: &mut Vec<u8>) -> bool {
    let mut buf = vec![0u8; 16 << 10];
    let read = async {
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => return true,
                Ok(n) => {
                    if !take(out, &buf[..n]) {
                        return false;
                    }
                }
            }
        }
    };
    tokio::time::timeout(DRAIN, read).await.unwrap_or(true)
}

fn finish(status: ExitStatus, out: &[u8], delegate: bool) -> Result<Answer, Failure> {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(0), _) => parse_answer(out, delegate),
        (Some(n), _) => Err(Failure::Exit(n)),
        (None, Some(s)) => Err(Failure::Signal(s)),
        (None, None) => Err(Failure::Exit(-1)),
    }
}

fn signal_group(group: Option<Pid>, signal: Signal) {
    if let Some(pgid) = group {
        // The group may be gone already.
        let _ = rustix::process::kill_process_group(pgid, signal);
    }
}

/// SIGTERM to the group, SIGKILL after the grace; returns `why`.
async fn terminate(child: &mut Child, group: Option<Pid>, why: Failure) -> Failure {
    signal_group(group, Signal::TERM);
    if tokio::time::timeout(KILL_GRACE, child.wait()).await.is_err() {
        signal_group(group, Signal::KILL);
        let _ = child.wait().await;
    }
    // Members that outlived the leader.
    signal_group(group, Signal::KILL);
    why
}

/// Forward stderr to the daemon's, a line at a time, as `escrowd: reviewer: <name> <scope>: <line>`.
/// The run's token, if it echoes it, is blanked.
async fn forward_stderr(mut stderr: ChildStderr, reviewer: String, scope: String, token: Option<String>) {
    let mut line: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 4 << 10];
    let emit = |line: &mut Vec<u8>| {
        let mut text = String::from_utf8_lossy(line).into_owned();
        if let Some(t) = &token {
            text = text.replace(t.as_str(), "<token>");
        }
        eprintln!("escrowd: reviewer: {reviewer} {scope}: {text}");
        line.clear();
    };
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\n' {
                        emit(&mut line);
                    } else {
                        line.push(b);
                        if line.len() >= MAX_LINE {
                            emit(&mut line);
                        }
                    }
                }
            }
        }
    }
    if !line.is_empty() {
        emit(&mut line);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn sh(script: &str, timeout: Duration, delegate: bool, env: &[&str]) -> Command {
        Command {
            program: PathBuf::from("/bin/sh"),
            argv: vec!["sh".into(), "-c".into(), script.into()],
            timeout,
            on_failure: Decision::Discard,
            env: env.iter().map(|s| s.to_string()).collect(),
            delegate,
        }
    }

    fn quick(script: &str) -> Command {
        sh(script, Duration::from_secs(10), false, &[])
    }

    fn request(scope: &str) -> RunRequest {
        RunRequest {
            scope: scope.into(),
            reviewer: "tester".into(),
            attempt: 1,
            cwd: std::env::temp_dir(),
            stdin: b"{}".to_vec(),
            token: None,
        }
    }

    fn runner(parallel: usize) -> Runner {
        Runner::new(parallel, PathBuf::from("/run/review.sock"))
    }

    #[tokio::test]
    async fn a_commit_answer_is_a_verdict_and_reports_the_pid() {
        let report = runner(1)
            .run(&quick(r#"echo '{"verdict":"commit"}'"#), request("s"))
            .await;

        assert!(report.pid.is_some());
        assert_eq!(report.result, Ok(Answer::Verdict(Decision::Commit, vec![])));
    }

    #[tokio::test]
    async fn a_return_answer_keeps_its_reasons_and_ignores_unknown_keys() {
        let script = r#"echo '{"verdict":"return","reasons":["a","b"],"extra":1}'"#;

        let report = runner(1).run(&quick(script), request("s")).await;

        let reasons = vec!["a".to_string(), "b".to_string()];
        assert_eq!(report.result, Ok(Answer::Verdict(Decision::Return, reasons)));
    }

    #[tokio::test]
    async fn a_nonzero_exit_fails_even_with_an_answer() {
        let report = runner(1).run(&quick("exit 3"), request("s")).await;
        assert_eq!(report.result, Err(Failure::Exit(3)));

        let report = runner(1)
            .run(&quick(r#"echo '{"verdict":"commit"}'; exit 1"#), request("s"))
            .await;
        assert_eq!(report.result, Err(Failure::Exit(1)));
    }

    #[tokio::test]
    async fn a_signal_death_is_a_signal_failure() {
        let report = runner(1).run(&quick("kill -9 $$"), request("s")).await;
        assert_eq!(report.result, Err(Failure::Signal(9)));
    }

    #[tokio::test]
    async fn a_run_that_ignores_term_is_killed_after_the_grace() {
        let cmd = sh(
            r#"trap "" TERM; while :; do sleep 30; done"#,
            Duration::from_millis(300),
            false,
            &[],
        );
        let t = Instant::now();

        let report = runner(1).run(&cmd, request("s")).await;

        let took = t.elapsed();
        assert_eq!(report.result, Err(Failure::Timeout));
        assert!(
            took >= KILL_GRACE,
            "SIGTERM was ignored, so the kill waits out the grace: {took:?}"
        );
        assert!(
            took < Duration::from_millis(300) + KILL_GRACE + Duration::from_secs(2),
            "{took:?}"
        );
        // SIGKILL went to the whole group; its last member (the shell's `sleep`, now
        // an orphan) is gone once init reaps it, which a busy host may take a moment for.
        let pgid = Pid::from_raw(report.pid.unwrap() as i32).unwrap();
        let reaped_by = Instant::now() + Duration::from_secs(5);
        while rustix::process::test_kill_process_group(pgid).is_ok() && Instant::now() < reaped_by {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            rustix::process::test_kill_process_group(pgid).is_err(),
            "the group is gone"
        );
    }

    #[tokio::test]
    async fn a_run_that_stops_on_term_ends_at_the_timeout() {
        let cmd = sh("sleep 30", Duration::from_millis(200), false, &[]);
        let t = Instant::now();

        let report = runner(1).run(&cmd, request("s")).await;

        assert_eq!(report.result, Err(Failure::Timeout));
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn an_answer_over_one_mib_is_too_large() {
        let report = runner(1).run(&quick("head -c 2097152 /dev/zero"), request("s")).await;
        assert_eq!(report.result, Err(Failure::TooLarge));
    }

    #[tokio::test]
    async fn a_bad_answer_is_named() {
        let report = runner(1).run(&quick("echo not json"), request("s")).await;
        assert!(
            matches!(report.result, Err(Failure::BadAnswer(_))),
            "{:?}",
            report.result
        );

        let report = runner(1).run(&quick("true"), request("s")).await;
        assert!(
            matches!(report.result, Err(Failure::BadAnswer(_))),
            "{:?}",
            report.result
        );
    }

    #[test]
    fn parse_answer_checks_the_verdict_word() {
        let bad = parse_answer(br#"{"verdict":"maybe"}"#, false).unwrap_err();
        assert_eq!(bad, Failure::BadAnswer("unknown verdict \"maybe\"".into()));
        assert!(matches!(
            parse_answer(br#"{"reasons":[]}"#, false),
            Err(Failure::BadAnswer(_))
        ));
        assert_eq!(
            parse_answer(b"{\"verdict\":\"discard\",\"reasons\":[\"x\"]}\n", false),
            Ok(Answer::Verdict(Decision::Discard, vec!["x".into()]))
        );
    }

    #[tokio::test]
    async fn pending_without_delegate_fails_and_with_it_is_pending() {
        let script = r#"echo '{"verdict":"pending","reasons":["ask a human"]}'"#;

        let plain = runner(1).run(&quick(script), request("s")).await;
        let delegating = runner(1)
            .run(&sh(script, Duration::from_secs(10), true, &[]), request("s"))
            .await;

        assert_eq!(plain.result, Err(Failure::PendingNotDelegate));
        assert_eq!(delegating.result, Ok(Answer::Pending(vec!["ask a human".into()])));
    }

    #[tokio::test]
    async fn a_child_that_never_reads_its_large_stdin_still_answers() {
        let mut req = request("s");
        req.stdin = vec![b'x'; 4 << 20];

        let report = runner(1)
            .run(&quick(r#"sleep 0.2; echo '{"verdict":"commit"}'"#), req)
            .await;

        assert_eq!(report.result, Ok(Answer::Verdict(Decision::Commit, vec![])));
    }

    #[tokio::test]
    async fn the_environment_is_the_allowlist_and_the_token_only_for_delegates() {
        let script = r#"printf '{"verdict":"return","reasons":["%s"]}' "$(env | cut -d= -f1 | tr '\n' ' ')""#;
        let mut req = request("s");
        req.token = Some("tok".into());
        let names = |report: RunReport| match report.result {
            Ok(Answer::Verdict(_, reasons)) => reasons[0].split(' ').map(String::from).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };

        let plain = names(
            runner(1)
                .run(&sh(script, Duration::from_secs(10), false, &["CARGO_PKG_NAME"]), req)
                .await,
        );
        let mut req = request("s");
        req.token = Some("tok".into());
        let delegated = names(
            runner(1)
                .run(&sh(script, Duration::from_secs(10), true, &["CARGO_PKG_NAME"]), req)
                .await,
        );

        for set in [&plain, &delegated] {
            assert!(set.contains(&"PATH".to_string()));
            assert!(
                set.contains(&"CARGO_PKG_NAME".to_string()),
                "a named variable is copied"
            );
            assert!(!set.contains(&"CARGO_MANIFEST_DIR".to_string()), "others are cleared");
        }
        assert!(!plain.contains(&"ESCROW_REVIEW_TOKEN".to_string()));
        assert!(!plain.contains(&"ESCROW_REVIEW_SOCKET".to_string()));
        assert!(delegated.contains(&"ESCROW_REVIEW_TOKEN".to_string()));
        assert!(delegated.contains(&"ESCROW_REVIEW_SOCKET".to_string()));
    }

    #[tokio::test]
    async fn cancel_stops_a_going_run() {
        let r = runner(1);
        let cmd = quick("sleep 30");

        let (report, ()) = tokio::join!(r.run(&cmd, request("s")), async {
            while !r.running("s") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            r.cancel("s");
        });

        assert_eq!(report.result, Err(Failure::Cancelled));
        assert!(report.pid.is_some());
        assert!(!r.running("s"));
    }

    #[tokio::test]
    async fn cancel_stops_a_queued_run_before_it_starts() {
        let r = runner(1);
        let long = quick("sleep 30");
        let queued = quick("sleep 30");

        let (first, second, ()) = tokio::join!(r.run(&long, request("a")), r.run(&queued, request("b")), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            r.cancel("b");
            tokio::time::sleep(Duration::from_millis(100)).await;
            r.stop_all();
        });

        assert_eq!(second.result, Err(Failure::Cancelled));
        assert_eq!(second.pid, None);
        assert_eq!(first.result, Err(Failure::Cancelled));
    }

    #[tokio::test]
    async fn one_permit_serializes_two_runs() {
        let cmd = quick(r#"sleep 0.4; echo '{"verdict":"commit"}'"#);
        let serial = runner(1);
        let t = Instant::now();
        let (a, b) = tokio::join!(serial.run(&cmd, request("a")), serial.run(&cmd, request("b")));
        let serial_took = t.elapsed();

        let both = runner(2);
        let t = Instant::now();
        let (c, d) = tokio::join!(both.run(&cmd, request("a")), both.run(&cmd, request("b")));
        let parallel_took = t.elapsed();

        for r in [a, b, c, d] {
            assert_eq!(r.result, Ok(Answer::Verdict(Decision::Commit, vec![])));
        }
        assert!(serial_took >= Duration::from_millis(800), "{serial_took:?}");
        assert!(parallel_took < Duration::from_millis(790), "{parallel_took:?}");
    }

    #[tokio::test]
    async fn a_program_that_is_missing_fails_to_start() {
        let mut cmd = quick("true");
        cmd.program = PathBuf::from("/nonexistent/reviewer");

        let report = runner(1).run(&cmd, request("s")).await;

        assert_eq!(report.pid, None);
        assert!(matches!(&report.result, Err(Failure::Spawn(_))));
        let cause = report.result.unwrap_err().cause(Duration::from_secs(1));
        assert!(cause.starts_with("cannot start: "), "{cause}");
    }

    #[test]
    fn failure_causes_read_as_the_stored_text() {
        let t = Duration::from_secs(90);
        assert_eq!(Failure::Timeout.cause(t), "timeout after 90s");
        assert_eq!(Failure::Timeout.cause(Duration::from_secs(120)), "timeout after 2m");
        assert_eq!(
            Failure::Timeout.cause(Duration::from_millis(250)),
            "timeout after 250ms"
        );
        assert_eq!(Failure::Exit(3).cause(t), "exit 3");
        assert_eq!(Failure::Signal(9).cause(t), "signal 9");
        assert_eq!(Failure::TooLarge.cause(t), "answer over 1 MiB");
        assert_eq!(Failure::BadAnswer("x".into()).cause(t), "bad answer: x");
        assert_eq!(
            Failure::PendingNotDelegate.cause(t),
            "pending from a reviewer without delegate"
        );
    }
}
