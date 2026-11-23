//! Held scopes: a closed scope reviewers have, each reviewer's verdict, and the
//! session's next scope waiting behind a hold with a wait.

use super::*;
use crate::store::Run;

/// A scope as reviewers see it.
pub struct Identity {
    pub id: String,
    pub name: String,
    pub session: String,
    pub token_sha256: Option<String>,
    pub labels: HashMap<String, String>,
    pub hold: Option<Hold>,
}

/// Who gives a verdict.
pub enum Giver {
    /// The reviewer, on the review socket.
    Socket,
    /// A delegate holding the token whose SHA-256 (hex) this is.
    Token(String),
    /// The reviewer's run `attempt`; `pid` of its program, `failure` the cause when the
    /// run failed and the verdict is the reviewer's `on_failure`.
    Run {
        attempt: u32,
        pid: Option<u32>,
        failure: Option<String>,
    },
    /// The token of the reviewer's waiting run `attempt` ran out; `cause` is what the
    /// ledger records beside the `on_failure` verdict.
    Expired { attempt: u32, cause: String },
    /// The reviewer is no longer in the policy: no run exists to answer; `cause` is what
    /// the ledger records beside the verdict.
    Missing { cause: String },
}

/// A reviewer's verdict on a held scope: still held for the reviewers left, the last
/// pending reviewer gave its verdict (`Hold::verdict`, with every review; not written),
/// or the verdict answers a run that is no longer current and counts for nothing.
pub enum Reviewed {
    Held(Hold),
    Final(Hold),
    Stale,
}

/// A command reviewer's run, recorded and ready to start.
pub struct RunTicket {
    /// 1 for the first run of the reviewer on the scope, then one more per rerun.
    pub attempt: u32,
    /// A delegated run's token, in the clear; only its hash is stored.
    pub token: Option<String>,
    /// The run's working directory: the scope's review files.
    pub cwd: PathBuf,
}

impl Views {
    /// The reviewer `name`, declared or built in.
    pub fn reviewer(&self, name: &str) -> Option<review::Reviewer> {
        self.review.reviewer(name).cloned()
    }

    /// The hold of scope `id`, if reviewers have it (`NoScope` if no such scope).
    pub fn held(&self, id: &str) -> error::Result<Option<Hold>> {
        Ok(self.handles(id)?[0].store_read().hold().cloned())
    }

    /// Scope `id`'s name, session, token hash and hold (`NoScope` if no such scope).
    pub fn identity(&self, id: &str) -> error::Result<Identity> {
        let hs = self.handles(id)?;
        let store = hs[0].store_read();
        Ok(Identity {
            id: id.to_string(),
            name: store.name.clone(),
            session: store.session.clone(),
            token_sha256: store.token_sha256().map(str::to_string),
            labels: store.labels()?,
            hold: store.hold().cloned(),
        })
    }

    /// Every held scope, oldest hold first.
    pub fn held_scopes(&self) -> error::Result<Vec<Identity>> {
        let ids: Vec<String> = {
            let scopes = self.scopes.read();
            scopes
                .values()
                .filter(|h| h.root == PROJECT && h.store_read().hold().is_some())
                .map(|h| h.group.clone())
                .collect()
        };
        let mut out = Vec::new();
        for id in ids {
            match self.identity(&id) {
                Ok(i) if i.hold.is_some() => out.push(i),
                Ok(_) => {}
                Err(e) if e.is_no_scope() => {}
                Err(e) => return Err(e),
            }
        }
        out.sort_by(|a, b| (a.hold.as_ref().map(|h| h.at_ms), &a.id).cmp(&(b.hold.as_ref().map(|h| h.at_ms), &b.id)));
        Ok(out)
    }

    /// Hold closed scope `id` for `reviewers` (in review order); `wait`: its session's
    /// next scope does not open until the verdict.
    pub fn hold_scope(&self, id: &str, reviewers: Vec<String>, wait: bool) -> error::Result<Hold> {
        let hs = self.closed_handles(id)?;
        let decision = format!("{}{}", reviewers.join(","), if wait { ",wait" } else { "" });
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let hold = Hold {
            reviewers,
            wait,
            at_ms,
            verdict: Decision::Commit,
            reviews: Vec::new(),
            run: None,
        };
        hs[0].store().set_hold(Some(hold.clone()))?;
        self.ledger.append(id, "hold", Path::new(""), None, &decision);
        Ok(hold)
    }

    /// Record a run of command reviewer `reviewer` on held scope `id`, replacing any
    /// recorded run, and write `files` (name, bytes) to the scope's review directory.
    /// A `delegate` run gets a token (32 random bytes, hex); the hold keeps its hash.
    pub fn start_run(
        &self,
        id: &str,
        reviewer: &str,
        delegate: bool,
        files: &[(&str, &[u8])],
    ) -> error::Result<RunTicket> {
        let hs = self.closed_handles(id)?;
        let mut store = hs[0].store();
        let Some(mut hold) = store.hold().cloned() else {
            return Err(Error::State(format!("scope {id} is not held")));
        };
        let token = match delegate {
            true => {
                let mut bytes = [0u8; 32];
                crate::sys::random(&mut bytes)?;
                Some(crate::diff::hex(&bytes))
            }
            false => None,
        };
        let cwd = store.write_review_files(files)?;
        let attempt = hold.next_attempt(reviewer);
        hold.run = Some(Run {
            reviewer: reviewer.to_string(),
            attempt,
            waiting: false,
            token_sha256: token.as_deref().map(token_sha256),
            deadline_ms: None,
        });
        store.set_hold(Some(hold))?;
        Ok(RunTicket { attempt, token, cwd })
    }

    /// A delegated run of `reviewer` (process `pid`) answered `pending`: its verdict comes
    /// by token until `deadline_ms` (milliseconds since the Unix epoch). False if that run
    /// is no longer the current, running one.
    pub fn await_delegate(
        &self,
        id: &str,
        reviewer: &str,
        attempt: u32,
        pid: Option<u32>,
        deadline_ms: u64,
    ) -> error::Result<bool> {
        let hs = self.closed_handles(id)?;
        let mut store = hs[0].store();
        let Some(mut hold) = store.hold().cloned() else {
            return Ok(false);
        };
        let Some(run) = hold
            .run
            .as_mut()
            .filter(|r| r.reviewer == reviewer && r.attempt == attempt && !r.waiting && r.token_sha256.is_some())
        else {
            return Ok(false);
        };
        run.waiting = true;
        run.deadline_ms = Some(deadline_ms);
        store.set_hold(Some(hold))?;
        self.ledger.append_review(id, reviewer, pid, None, "pending");
        Ok(true)
    }

    /// Ledger a verdict of `reviewer` on scope `id` that was refused for want of a valid
    /// token: `cause=token decision=deny`. The token is never written.
    pub fn deny_review(&self, id: &str, reviewer: &str) {
        self.ledger.append_review(id, reviewer, None, Some("token"), "deny");
    }

    /// `reviewer`'s verdict on held scope `id`, given by `giver`. The reviewer must be
    /// pending; one that may override also stands for the reviewers pending before it
    /// (`review::Rules::admit` decides). A verdict looser than the one so far needs the
    /// right to override and `over` (`Denied` otherwise); the ledger records each
    /// verdict and every override. While reviewers remain pending the hold keeps the
    /// verdict (written at once); after the last nothing is written: the caller applies
    /// the final verdict, which drops or reopens the scope.
    ///
    /// A socket giver cannot answer for a command reviewer (`Denied`: its run does).
    /// A run's answer for a run that is no longer the current one is `Stale`, and so
    /// is the expiry of a token that is not (or no longer) waiting.
    pub fn review_scope(
        &self,
        id: &str,
        reviewer: &str,
        giver: Giver,
        verdict: Decision,
        reasons: Vec<String>,
        over: bool,
    ) -> error::Result<Reviewed> {
        let hs = self.closed_handles(id)?;
        let mut store = hs[0].store();
        let Some(mut hold) = store.hold().cloned() else {
            return Err(Error::State(format!("scope {id} is not held")));
        };
        let current = hold.run.as_ref().filter(|r| r.reviewer == reviewer);
        let (pid, cause, from_run) = match &giver {
            Giver::Socket => {
                if matches!(
                    self.review.reviewer(reviewer).map(|r| &r.kind),
                    Some(review::Kind::Command(_))
                ) {
                    return Err(Error::Denied(format!(
                        "{reviewer} is a command reviewer: its run gives the verdict"
                    )));
                }
                (None, None, false)
            }
            Giver::Token(sha256) => {
                // Valid from the run's start (also while its program still runs) until used.
                if current.is_none_or(|r| r.token_sha256.as_deref() != Some(sha256.as_str())) {
                    return Err(Error::Denied(format!("scope {id}: {reviewer} has no such token")));
                }
                (None, None, false)
            }
            Giver::Run { attempt, pid, failure } => {
                if !current.is_some_and(|r| r.attempt == *attempt && !r.waiting) {
                    return Ok(Reviewed::Stale);
                }
                (*pid, failure.clone(), true)
            }
            Giver::Expired { attempt, cause } => {
                if !current.is_some_and(|r| r.waiting && r.attempt == *attempt) {
                    return Ok(Reviewed::Stale);
                }
                (None, Some(cause.clone()), true)
            }
            Giver::Missing { cause } => (None, Some(cause.clone()), true),
        };
        let admitted = self
            .review
            .admit(&hold.reviewers, hold.verdict, reviewer, verdict, over, from_run)
            .map_err(|refused| match refused {
                review::Refused::NotPending => Error::State(format!(
                    "scope {id} is not waiting for {reviewer}: it waits for {}",
                    hold.reviewers.join(", ")
                )),
                review::Refused::NotFirst(first) => Error::State(format!("scope {id} waits for {first} first")),
                review::Refused::Looser => Error::Denied(format!(
                    "{reviewer} cannot loosen {} to {}: only a reviewer that may override can, with an override",
                    hold.verdict.as_str(),
                    verdict.as_str()
                )),
            })?;
        if admitted.looser {
            let change = format!("{}-to-{}", hold.verdict.as_str(), verdict.as_str());
            self.ledger.append(id, "override", Path::new(""), None, &change);
        }
        hold.verdict = admitted.verdict_after;
        self.ledger
            .append_review(id, reviewer, pid, cause.as_deref(), verdict.as_str());
        hold.reviews.push(TierReview {
            reviewer: reviewer.to_string(),
            verdict,
            reasons,
            over: admitted.looser,
        });
        hold.reviewers.drain(..=admitted.at);
        hold.run = None;
        if admitted.done {
            hold.reviewers.clear();
            return Ok(Reviewed::Final(hold));
        }
        store.set_hold(Some(hold.clone()))?;
        Ok(Reviewed::Held(hold))
    }

    /// The held scope of `session` its next scope waits for, if any.
    pub fn blocking_hold(&self, session: &str) -> Option<String> {
        if session.is_empty() {
            return None;
        }
        let scopes = self.scopes.read();
        let mut ids: Vec<&String> = scopes
            .values()
            .filter(|h| h.root == PROJECT)
            .filter(|h| {
                let store = h.store_read();
                store.session == session && store.hold().is_some_and(|hold| hold.wait)
            })
            .map(|h| &h.group)
            .collect();
        ids.sort();
        ids.first().map(|id| id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: diff::Caps = diff::Caps {
        file_bytes: 1 << 20,
        max_bytes: 1 << 20,
    };

    const DECLARED: &str = "{sec: {}, boss: {can_override: true}, cmd: {run: [/bin/sh]}}";

    fn names(ns: &[&str]) -> Vec<String> {
        ns.iter().map(|n| n.to_string()).collect()
    }

    /// A closed scope held for the reviewers `ns`.
    fn held(name: &str, ns: &[&str]) -> (Views, PathBuf, String) {
        let (views, dir) = Views::fixture_with(name, DECLARED);
        let id = views.open_scope("t", &HashMap::new(), "").unwrap().id;
        views.close_scope_after(&id, &[], CAPS).unwrap();
        views.hold_scope(&id, names(ns), false).unwrap();
        (views, dir, id)
    }

    fn ledger(dir: &Path) -> String {
        fs::read_to_string(dir.join("state/ledger.log")).unwrap()
    }

    fn review(views: &Views, id: &str, who: &str, verdict: Decision, over: bool) -> error::Result<Reviewed> {
        views.review_scope(id, who, Giver::Socket, verdict, Vec::new(), over)
    }

    #[test]
    fn a_reviewer_that_is_not_pending_is_a_state_error() {
        let (views, dir, id) = held("hold-not-pending", &["sec"]);
        assert!(matches!(
            review(&views, &id, "llm", Decision::Commit, false),
            Err(Error::State(_))
        ));
        assert!(matches!(
            review(&views, &id, "nobody", Decision::Commit, false),
            Err(Error::State(_))
        ));
        assert_eq!(views.held(&id).unwrap().unwrap().reviewers, ["sec"]);
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_llm_reviewer_before_the_declared_one_is_a_state_error() {
        let (views, dir, id) = held("hold-order", &["sec", "llm"]);
        assert!(matches!(
            review(&views, &id, "llm", Decision::Commit, false),
            Err(Error::State(_))
        ));
        assert_eq!(views.held(&id).unwrap().unwrap().reviewers, ["sec", "llm"]);
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_looser_verdict_without_the_right_and_the_flag_is_denied() {
        let (views, dir, id) = held("hold-denied", &["sec", "llm", "human"]);
        assert!(matches!(
            review(&views, &id, "sec", Decision::Discard, false),
            Ok(Reviewed::Held(_))
        ));
        // A reviewer that cannot override is denied even with the flag; one that can needs it.
        assert!(matches!(
            review(&views, &id, "llm", Decision::Commit, true),
            Err(Error::Denied(_))
        ));
        assert!(matches!(
            review(&views, &id, "human", Decision::Commit, false),
            Err(Error::Denied(_))
        ));
        let hold = views.held(&id).unwrap().unwrap();
        assert_eq!(hold.verdict, Decision::Discard);
        assert_eq!(hold.reviewers, ["llm", "human"]);
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_override_loosens_the_verdict_and_logs_it_in_order() {
        let (views, dir, id) = held("hold-override", &["sec", "boss"]);
        review(&views, &id, "sec", Decision::Discard, false).unwrap();
        let Reviewed::Final(hold) = review(&views, &id, "boss", Decision::Commit, true).unwrap() else {
            panic!("boss is the last reviewer");
        };
        assert_eq!(hold.verdict, Decision::Commit);
        assert!(hold.reviews.last().is_some_and(|r| r.over && r.reviewer == "boss"));
        let log = ledger(&dir);
        assert!(log.contains("op=override") && log.contains("decision=discard-to-commit"));
        assert!(log.contains("op=review-sec path= decision=discard"));
        assert!(log.find("op=override") < log.find("op=review-boss"));
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_hold_is_ledgered_with_the_reviewer_names_and_wait() {
        let (views, dir) = Views::fixture_with("hold-ledger", DECLARED);
        let id = views.open_scope("t", &HashMap::new(), "").unwrap().id;
        views.close_scope_after(&id, &[], CAPS).unwrap();
        views.hold_scope(&id, names(&["sec", "human"]), true).unwrap();
        assert!(ledger(&dir).contains("op=hold path= decision=sec,human,wait"));
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_last_reviewer_is_final_and_writes_nothing() {
        let (views, dir, id) = held("hold-final", &["sec"]);
        let Reviewed::Final(hold) = review(&views, &id, "sec", Decision::Return, false).unwrap() else {
            panic!("sec is the last reviewer");
        };
        assert_eq!(hold.verdict, Decision::Return);
        assert!(hold.reviewers.is_empty());
        // The stored hold is as hold_scope left it.
        let stored = views.held(&id).unwrap().unwrap();
        assert_eq!(stored.reviewers, ["sec"]);
        assert_eq!(stored.verdict, Decision::Commit);
        assert!(stored.reviews.is_empty());
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reviewer_before_the_last_leaves_the_scope_held_with_the_rest() {
        let (views, dir, id) = held("hold-drain", &["sec", "llm", "human"]);
        let Reviewed::Held(hold) = review(&views, &id, "sec", Decision::Return, false).unwrap() else {
            panic!("reviewers remain");
        };
        assert_eq!(hold.reviewers, ["llm", "human"]);
        assert_eq!(hold.verdict, Decision::Return);
        assert_eq!(views.held(&id).unwrap().unwrap(), hold);
        // A human's verdict also stands for the reviewers pending before it.
        let Reviewed::Final(done) = review(&views, &id, "human", Decision::Return, false).unwrap() else {
            panic!("the human is last");
        };
        assert!(done.reviewers.is_empty());
        assert_eq!(done.reviews.len(), 2);
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_discard_nobody_later_can_override_ends_the_review_early() {
        let (views, dir, id) = held("hold-early", &["sec", "llm"]);
        let Reviewed::Final(hold) = review(&views, &id, "sec", Decision::Discard, false).unwrap() else {
            panic!("llm cannot override a discard");
        };
        assert_eq!((hold.verdict, hold.reviewers.len()), (Decision::Discard, 0));
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_command_reviewer_cannot_be_answered_on_the_socket() {
        let (views, dir, id) = held("hold-command", &["cmd"]);
        assert!(matches!(
            review(&views, &id, "cmd", Decision::Commit, false),
            Err(Error::Denied(_))
        ));
        assert_eq!(views.held(&id).unwrap().unwrap().reviewers, ["cmd"]);
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_answers_only_while_it_is_the_current_run() {
        let (views, dir, id) = held("hold-run", &["cmd", "sec"]);
        let run = |attempt| Giver::Run {
            attempt,
            pid: Some(5),
            failure: Some("exit 3".into()),
        };
        // No run is recorded: the answer is stale and the hold untouched.
        let none = views.review_scope(&id, "cmd", run(1), Decision::Return, Vec::new(), false);
        assert!(matches!(none, Ok(Reviewed::Stale)));
        let mut hold = views.held(&id).unwrap().unwrap();
        hold.run = Some(Run {
            reviewer: "cmd".into(),
            attempt: 2,
            waiting: false,
            token_sha256: None,
            deadline_ms: None,
        });
        views.closed_handles(&id).unwrap()[0]
            .store()
            .set_hold(Some(hold))
            .unwrap();
        let old = views.review_scope(&id, "cmd", run(1), Decision::Return, Vec::new(), false);
        assert!(matches!(old, Ok(Reviewed::Stale)));
        let Ok(Reviewed::Held(after)) = views.review_scope(&id, "cmd", run(2), Decision::Return, Vec::new(), false)
        else {
            panic!("the current run's answer is taken");
        };
        assert_eq!(
            (after.reviewers.as_slice(), after.run.as_ref()),
            (&names(&["sec"])[..], None)
        );
        assert!(ledger(&dir).contains("op=review-cmd path= pid=5 cause=exit%203 decision=return"));
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_token_verdict_needs_the_waiting_runs_token() {
        let (views, dir, id) = held("hold-token", &["cmd"]);
        let give = |sha: &str| {
            views.review_scope(
                &id,
                "cmd",
                Giver::Token(sha.into()),
                Decision::Commit,
                Vec::new(),
                false,
            )
        };
        assert!(matches!(give("aa"), Err(Error::Denied(_))));
        let mut hold = views.held(&id).unwrap().unwrap();
        hold.run = Some(Run {
            reviewer: "cmd".into(),
            attempt: 1,
            waiting: true,
            token_sha256: Some("aa".into()),
            deadline_ms: Some(1),
        });
        views.closed_handles(&id).unwrap()[0]
            .store()
            .set_hold(Some(hold))
            .unwrap();
        assert!(matches!(give("bb"), Err(Error::Denied(_))));
        assert!(matches!(give("aa"), Ok(Reviewed::Final(_))));
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_delegates_pending_answer_waits_once_and_its_token_is_good_from_the_start() {
        let (views, dir, id) = held("hold-pending", &["cmd", "sec"]);
        assert!(!views.await_delegate(&id, "cmd", 1, Some(9), 5).unwrap(), "no run yet");
        let ticket = views.start_run(&id, "cmd", true, &[]).unwrap();
        let token = ticket.token.unwrap();
        assert!(
            !views.await_delegate(&id, "cmd", 2, Some(9), 5).unwrap(),
            "another attempt"
        );
        // The token works while the program still runs; a wrong one does not.
        let give = |t: &str| {
            let giver = Giver::Token(token_sha256(t));
            views.review_scope(&id, "cmd", giver, Decision::Commit, Vec::new(), false)
        };
        assert!(matches!(give("nope"), Err(Error::Denied(_))));
        assert!(views.await_delegate(&id, "cmd", ticket.attempt, Some(9), 5).unwrap());
        assert!(
            !views.await_delegate(&id, "cmd", ticket.attempt, Some(9), 5).unwrap(),
            "only once"
        );
        let run = views.held(&id).unwrap().unwrap().run.unwrap();
        assert_eq!((run.waiting, run.deadline_ms), (true, Some(5)));
        assert!(ledger(&dir).contains("op=review-cmd path= pid=9 decision=pending"));
        assert!(!ledger(&dir).contains(&token));
        assert!(matches!(give(&token), Ok(Reviewed::Held(_))));
        // Used: the token is gone with the run.
        assert!(matches!(give(&token), Err(Error::Denied(_))));
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_expiry_is_stale_unless_the_same_run_waits() {
        let (views, dir, id) = held("hold-expiry", &["cmd"]);
        let expire = |attempt| {
            let giver = Giver::Expired {
                attempt,
                cause: "no verdict within 1s".into(),
            };
            views.review_scope(&id, "cmd", giver, Decision::Return, vec!["failed: x".into()], false)
        };
        views.start_run(&id, "cmd", true, &[]).unwrap();
        assert!(matches!(expire(1), Ok(Reviewed::Stale)), "still running");
        assert!(views.await_delegate(&id, "cmd", 1, None, 5).unwrap());
        assert!(matches!(expire(2), Ok(Reviewed::Stale)), "another attempt");
        assert!(matches!(expire(1), Ok(Reviewed::Final(_))));
        assert!(ledger(&dir).contains("op=review-cmd path= cause=no%20verdict%20within%201s decision=return"));
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }
}
