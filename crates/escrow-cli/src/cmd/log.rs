//! `escrow log`: the ledger, optionally for one scope.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

pub(crate) fn run(project: Option<PathBuf>, state: Option<PathBuf>, scope: Option<String>) -> anyhow::Result<()> {
    let state = match (state, project) {
        (Some(s), _) => s,
        (None, Some(p)) => escrowd::daemon::default_state_dir(&p.canonicalize()?)?,
        (None, None) => anyhow::bail!("pass --project or --state"),
    };
    let file = std::fs::File::open(state.join("ledger.log"))?;
    let mut out = std::io::stdout().lock();
    let Some(scope) = scope else {
        for line in BufReader::new(file).lines() {
            writeln!(out, "{}", line?)?;
        }
        return Ok(());
    };
    let needle = format!(" scope={scope} ");
    // A `proc` line comes before the first line naming it; print it (parents first)
    // only when a line of the scope does.
    let field = |line: &str, key: &str| -> Option<String> {
        line.split(' ').find_map(|f| f.strip_prefix(key)).map(str::to_string)
    };
    let mut procs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Some(id) = line.split(' ').nth(1).and_then(|f| f.strip_prefix("proc=")) {
            procs.insert(id.to_string(), line.clone());
            continue;
        }
        if !line.contains(&needle) {
            continue;
        }
        let mut chain = Vec::new();
        let mut next = field(&line, "proc=");
        while let Some(p) = next.take().and_then(|id| procs.remove(&id)) {
            next = field(&p, "parent=");
            chain.push(p);
        }
        for p in chain.iter().rev() {
            writeln!(out, "{p}")?;
        }
        writeln!(out, "{line}")?;
    }
    Ok(())
}
