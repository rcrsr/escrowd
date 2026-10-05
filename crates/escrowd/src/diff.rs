//! The content diff of a closed scope's change set, in git's extended unified format.
//!
//! Each change is diffed against the scope's snapshot, not the live base, so another
//! scope's commit never shows up. Text files diff line by line; binary files (a NUL
//! byte or invalid UTF-8) and files over the policy's `diff.file_bytes` are
//! summarized by size and SHA-256. A rename is `rename from`/`rename to`, with hunks
//! when the content changed too; a mode change is `old mode`/`new mode` with the
//! full mode. Directories and special files have no section; their files do. A path
//! the read gate denies (`read.deny`) shows no content, size or hash on either side.
//! Sections follow the change set's order and stop before the one that would take
//! the diff past `diff.max_bytes`; a last line counts the files left out.
//!
//! Paths are `a/<path>` and `b/<path>` with the path as the change set shows it
//! (`~/…` in `$HOME`, absolute in `/tmp`), C-quoted as git does when needed. For
//! project paths, `git apply` on a copy of the snapshot reproduces the staged tree,
//! summarized files aside.

use std::fmt::Write as _;
use std::io::{self, Read};
use std::os::fd::BorrowedFd;
use std::path::Path;
use std::time::Duration;

use rustix::fs::{OFlags, Stat};
use sha2::{Digest, Sha256};
use similar::TextDiff;

use crate::changeset::{Change, Kind};
use crate::snapshot::Base;
use crate::sys;

/// Per-file and total size caps (policy `diff:`).
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// A file larger than this on either side is summarized.
    pub file_bytes: u64,
    /// The diff stops before the section that would take it past this.
    pub max_bytes: u64,
}

/// One side of a change: what the snapshot or the staged tree holds at a path.
#[derive(Debug, PartialEq)]
pub enum Entry {
    /// Absent, a directory or a special file: no content to diff.
    None,
    /// A regular file or a symlink (`mode` has the type bits) with its text.
    Text { mode: u32, text: String },
    /// Under `read.deny`: the mode only.
    Withheld { mode: u32 },
    /// Binary or over the cap: size and SHA-256 only.
    Summary {
        mode: u32,
        size: u64,
        sha256: String,
        binary: bool,
    },
}

impl Entry {
    fn mode(&self) -> Option<u32> {
        match self {
            Entry::None => None,
            Entry::Text { mode, .. } | Entry::Withheld { mode } | Entry::Summary { mode, .. } => Some(*mode),
        }
    }
}

/// Where an entry is read from: the snapshot or the scope's upper tree.
pub enum Side<'a> {
    Base(&'a Base<'a>),
    Upper(BorrowedFd<'a>),
}

impl Side<'_> {
    fn lstat(&self, rel: &Path) -> io::Result<Stat> {
        match self {
            Side::Base(b) => b.lstat(rel),
            Side::Upper(fd) => sys::lstat(*fd, rel),
        }
    }

    /// The entry at `rel`; a missing path is `Entry::None`, a `withheld` one has no content.
    pub fn entry(&self, rel: &Path, cap: u64, withheld: bool) -> io::Result<Entry> {
        let st = match self.lstat(rel) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(Entry::None),
            Err(e) => return Err(e),
        };
        // git writes every symlink as 120000.
        let mode = match st.st_mode & libc::S_IFMT {
            libc::S_IFLNK => libc::S_IFLNK,
            _ => st.st_mode & (libc::S_IFMT | 0o7777),
        };
        match st.st_mode & libc::S_IFMT {
            libc::S_IFREG | libc::S_IFLNK if withheld => Ok(Entry::Withheld { mode }),
            libc::S_IFREG => {
                let mut f = match self {
                    Side::Base(b) => b.open(rel, OFlags::RDONLY)?,
                    Side::Upper(fd) => sys::open(*fd, rel, OFlags::RDONLY, 0)?,
                };
                if st.st_size as u64 > cap {
                    let (size, sha256) = hash(&mut f)?;
                    return Ok(Entry::Summary {
                        mode,
                        size,
                        sha256,
                        binary: false,
                    });
                }
                let mut bytes = Vec::with_capacity(st.st_size as usize);
                f.read_to_end(&mut bytes)?;
                Ok(text_or_summary(mode, bytes, cap))
            }
            libc::S_IFLNK => {
                let target = match self {
                    Side::Base(b) => b.readlink(rel)?,
                    Side::Upper(fd) => sys::readlink(*fd, rel)?,
                };
                Ok(text_or_summary(mode, target.into_os_string().into_encoded_bytes(), cap))
            }
            _ => Ok(Entry::None),
        }
    }
}

fn text_or_summary(mode: u32, bytes: Vec<u8>, cap: u64) -> Entry {
    let over = bytes.len() as u64 > cap;
    if !over && !bytes.contains(&0) {
        match String::from_utf8(bytes) {
            Ok(text) => return Entry::Text { mode, text },
            Err(e) => return summary(mode, e.as_bytes(), true),
        }
    }
    summary(mode, &bytes, !over)
}

fn summary(mode: u32, bytes: &[u8], binary: bool) -> Entry {
    Entry::Summary {
        mode,
        size: bytes.len() as u64,
        sha256: hex(&Sha256::digest(bytes)),
        binary,
    }
}

fn hash(f: &mut impl Read) -> io::Result<(u64, String)> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok((size, hex(&h.finalize())));
        }
        size += n as u64;
        h.update(&buf[..n]);
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A path as git writes it: C-quoted (with the quotes) when it holds a control
/// character, a quote, a backslash or a non-ASCII byte.
pub fn quote(p: &str) -> String {
    if !p.bytes().any(|b| !(0x20..0x7f).contains(&b) || b == b'"' || b == b'\\') {
        return p.to_string();
    }
    let mut out = String::from("\"");
    for b in p.bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            0x20..0x7f => out.push(b as char),
            _ => {
                let _ = write!(out, "\\{b:03o}");
            }
        }
    }
    out.push('"');
    out
}

/// The mode as git writes it: 100644, 100755 (any execute bit) or 120000.
fn git_mode(mode: u32) -> u32 {
    match mode & libc::S_IFMT {
        libc::S_IFLNK => libc::S_IFLNK,
        _ if mode & 0o111 != 0 => libc::S_IFREG | 0o755,
        _ => libc::S_IFREG | 0o644,
    }
}

/// The section of one file: `old` at `from` (the path itself unless renamed) and
/// `new` at `path`, empty when there is nothing to show; and a note for the end of
/// the diff when the mode changed in a way git's modes cannot show (0644 to 0600).
pub fn section(from: &str, path: &str, old: &Entry, new: &Entry) -> (String, Option<String>) {
    let (a, b) = (quote(&format!("a/{from}")), quote(&format!("b/{path}")));
    let types = |e: &Entry| e.mode().map(|m| m & libc::S_IFMT);
    // A type change (file to symlink) is a delete and a create, as in git.
    if types(old).is_some() && types(new).is_some() && types(old) != types(new) {
        let (deleted, _) = section(from, from, old, &Entry::None);
        let (created, note) = section(path, path, &Entry::None, new);
        return (deleted + &created, note);
    }
    let note = match (old.mode(), new.mode()) {
        (None, Some(n)) if git_mode(n) != n => Some(format!("# escrow: mode of {b}: {n:o}")),
        (Some(o), Some(n)) if o != n && (git_mode(o), git_mode(n)) != (o, n) => {
            Some(format!("# escrow: mode of {b}: {o:o} -> {n:o}"))
        }
        _ => None,
    };
    let mut out = format!("diff --git {a} {b}\n");
    let header = out.len();
    match (old.mode().map(git_mode), new.mode().map(git_mode)) {
        (None, None) => return (String::new(), None),
        (None, Some(m)) => {
            let _ = writeln!(out, "new file mode {m:o}");
        }
        (Some(m), None) => {
            let _ = writeln!(out, "deleted file mode {m:o}");
        }
        (Some(o), Some(n)) => {
            if from != path {
                let _ = writeln!(out, "rename from {}\nrename to {}", quote(from), quote(path));
            }
            if o != n {
                let _ = writeln!(out, "old mode {o:o}\nnew mode {n:o}");
            }
        }
    }
    let (minus, plus) = match (old, new) {
        (Entry::None, _) => ("/dev/null".to_string(), b),
        (_, Entry::None) => (a, "/dev/null".to_string()),
        _ => (a, b),
    };
    match (old, new) {
        (Entry::Withheld { .. }, _) | (_, Entry::Withheld { .. }) => {
            let _ = writeln!(out, "Files {minus} and {plus}: content withheld (read.deny)");
        }
        (Entry::Summary { .. }, _) | (_, Entry::Summary { .. }) => {
            if !same_summary(old, new) {
                let binary = [old, new]
                    .iter()
                    .any(|e| matches!(e, Entry::Summary { binary: true, .. }));
                let what = if binary { "Binary files" } else { "Files" };
                let _ = writeln!(
                    out,
                    "{what} {minus} and {plus} differ ({} -> {})",
                    describe(old),
                    describe(new)
                );
            }
        }
        _ => {
            let text = |e: &Entry| match e {
                Entry::Text { text, .. } => text.clone(),
                _ => String::new(),
            };
            let (old_text, new_text) = (text(old), text(new));
            if old_text != new_text {
                let diff = TextDiff::configure()
                    .newline_terminated(true)
                    .timeout(Duration::from_secs(1))
                    .diff_lines(old_text.as_str(), new_text.as_str());
                let _ = writeln!(out, "--- {minus}\n+++ {plus}");
                for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
                    let _ = write!(out, "{hunk}");
                }
            }
        }
    }
    if out.len() == header && from == path && old.mode().is_some() && new.mode().is_some() {
        out.clear(); // nothing git can show changed
    }
    (out, note)
}

fn same_summary(a: &Entry, b: &Entry) -> bool {
    match (a, b) {
        (Entry::Summary { sha256: x, .. }, Entry::Summary { sha256: y, .. }) => x == y,
        _ => false,
    }
}

fn describe(e: &Entry) -> String {
    match e {
        Entry::None | Entry::Withheld { .. } => "absent".to_string(),
        Entry::Text { text, .. } => {
            format!("{} bytes, sha256 {}", text.len(), hex(&Sha256::digest(text.as_bytes())))
        }
        Entry::Summary { size, sha256, .. } => format!("{size} bytes, sha256 {sha256}"),
    }
}

/// One root of a scope as the diff reads it.
pub struct Root<'a> {
    /// The scope's snapshot of the root.
    pub base: &'a Base<'a>,
    /// The scope's staged entries.
    pub upper: BorrowedFd<'a>,
    /// The path as the change set shows it.
    pub shown: &'a dyn Fn(&Path) -> String,
    /// True for a path whose content the diff must not show.
    pub withheld: &'a dyn Fn(&Path) -> bool,
}

/// A scope's diff, built root by root.
pub struct Diff {
    caps: Caps,
    text: String,
    /// Mode changes git's modes cannot show, for the end.
    notes: Vec<String>,
    /// Files left out once a section would pass `caps.max_bytes`.
    omitted: usize,
}

impl Diff {
    pub fn new(caps: Caps) -> Self {
        Diff {
            caps,
            text: String::new(),
            notes: Vec::new(),
            omitted: 0,
        }
    }

    /// Append the sections of `changes`, all in `root`.
    pub fn add(&mut self, root: &Root, changes: &[Change]) -> io::Result<()> {
        let (old_side, new_side) = (Side::Base(root.base), Side::Upper(root.upper));
        let cap = self.caps.file_bytes;
        // A rename onto an existing base path replaces it: delete that path first.
        let replaced: Vec<&Change> = changes
            .iter()
            .filter(|c| c.kind == Kind::Rename && root.base.lstat(&c.path).is_ok_and(|st| !sys::is_dir(&st)))
            .collect();
        let order = replaced
            .iter()
            .map(|c| (*c, true))
            .chain(changes.iter().map(|c| (c, false)));
        for (c, replaced) in order {
            if self.omitted > 0 {
                self.omitted += usize::from(!replaced);
                continue;
            }
            let path = (root.shown)(&c.path);
            let (from, old, new) = if replaced {
                let old = old_side.entry(&c.path, cap, (root.withheld)(&c.path))?;
                (path.clone(), old, Entry::None)
            } else {
                let src = c.from.as_deref().unwrap_or(&c.path);
                let hide = (root.withheld)(src) || (root.withheld)(&c.path);
                let old = old_side.entry(src, cap, hide)?;
                let new = match c.kind {
                    Kind::Delete => Entry::None,
                    _ => new_side.entry(&c.path, cap, hide)?,
                };
                ((root.shown)(src), old, new)
            };
            let (s, note) = section(&from, &path, &old, &new);
            let notes: usize = self.notes.iter().map(|n| n.len() + 1).sum();
            let more = s.len() + note.as_ref().map_or(0, |n| n.len() + 1);
            if (self.text.len() + notes + more) as u64 > self.caps.max_bytes {
                self.omitted += 1;
                continue;
            }
            self.text.push_str(&s);
            self.notes.extend(note);
        }
        Ok(())
    }

    /// The diff, then the mode notes, then a line counting the files left out, if any:
    /// `git apply` ignores lines after the last section.
    pub fn finish(mut self) -> String {
        for n in &self.notes {
            self.text.push_str(n);
            self.text.push('\n');
        }
        if self.omitted > 0 {
            let _ = writeln!(
                self.text,
                "# escrow: {} more changed file(s) left out of the diff (diff.max_bytes {})",
                self.omitted, self.caps.max_bytes
            );
        }
        self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Entry {
        Entry::Text {
            mode: libc::S_IFREG | 0o644,
            text: s.into(),
        }
    }

    #[test]
    fn modify_is_a_unified_diff() {
        let (s, _) = section("a.txt", "a.txt", &text("one\ntwo\n"), &text("one\nthree\n"));
        assert_eq!(
            s,
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+three\n"
        );
    }

    #[test]
    fn create_and_delete_use_dev_null() {
        let (s, _) = section("n", "n", &Entry::None, &text("x"));
        assert_eq!(
            s,
            "diff --git a/n b/n\nnew file mode 100644\n--- /dev/null\n+++ b/n\n@@ -0,0 +1 @@\n+x\n\\ No newline at end of file\n"
        );
        let (s, _) = section("d", "d", &text("y\n"), &Entry::None);
        assert_eq!(
            s,
            "diff --git a/d b/d\ndeleted file mode 100644\n--- a/d\n+++ /dev/null\n@@ -1 +0,0 @@\n-y\n"
        );
        let (s, _) = section("e", "e", &Entry::None, &text(""));
        assert_eq!(s, "diff --git a/e b/e\nnew file mode 100644\n");
    }

    #[test]
    fn pure_rename_and_mode_change_have_headers_only() {
        let (s, _) = section("old", "new", &text("x\n"), &text("x\n"));
        assert_eq!(s, "diff --git a/old b/new\nrename from old\nrename to new\n");
        let exe = Entry::Text {
            mode: libc::S_IFREG | 0o755,
            text: "x\n".into(),
        };
        assert_eq!(
            section("f", "f", &text("x\n"), &exe),
            ("diff --git a/f b/f\nold mode 100644\nnew mode 100755\n".into(), None)
        );
        assert_eq!(section("f", "f", &text("x\n"), &text("x\n")), (String::new(), None));
        let private = Entry::Text {
            mode: libc::S_IFREG | 0o600,
            text: "x\n".into(),
        };
        let note = Some("# escrow: mode of b/f: 100644 -> 100600".to_string());
        assert_eq!(section("f", "f", &text("x\n"), &private), (String::new(), note));
        let (s, note) = section("n", "n", &Entry::None, &private);
        assert!(s.starts_with("diff --git a/n b/n\nnew file mode 100644\n"));
        assert_eq!(note.as_deref(), Some("# escrow: mode of b/n: 100600"));
    }

    #[test]
    fn binary_and_over_cap_are_summarized() {
        let bin = text_or_summary(libc::S_IFREG | 0o644, vec![0, 1, 2], 100);
        assert!(matches!(
            bin,
            Entry::Summary {
                binary: true,
                size: 3,
                ..
            }
        ));
        let big = text_or_summary(libc::S_IFREG | 0o644, b"0123456789".to_vec(), 4);
        assert!(matches!(
            big,
            Entry::Summary {
                binary: false,
                size: 10,
                ..
            }
        ));
        let (s, _) = section("b", "b", &Entry::None, &bin);
        assert!(s.starts_with("diff --git a/b b/b\nnew file mode 100644\nBinary files /dev/null and b/b differ (absent -> 3 bytes, sha256 "));
        let same = text_or_summary(libc::S_IFREG | 0o644, vec![0, 1, 2], 100);
        assert_eq!(section("b", "b", &bin, &same).0, "");
    }

    #[test]
    fn type_change_is_delete_then_create() {
        let link = Entry::Text {
            mode: libc::S_IFLNK,
            text: "target".into(),
        };
        let (s, _) = section("p", "p", &text("x\n"), &link);
        assert!(s.starts_with("diff --git a/p b/p\ndeleted file mode 100644\n"));
        assert!(
            s.contains("diff --git a/p b/p\nnew file mode 120000\n--- /dev/null\n+++ b/p\n@@ -0,0 +1 @@\n+target\n")
        );
    }

    #[test]
    fn withheld_shows_no_content() {
        let w = Entry::Withheld {
            mode: libc::S_IFREG | 0o600,
        };
        let (s, _) = section(".env", ".env", &w, &w);
        assert_eq!(
            s,
            "diff --git a/.env b/.env\nFiles a/.env and b/.env: content withheld (read.deny)\n"
        );
    }

    #[test]
    fn quotes_like_git() {
        assert_eq!(quote("a/plain name.txt"), "a/plain name.txt");
        assert_eq!(quote("a/tab\there"), "\"a/tab\\there\"");
        assert_eq!(quote("a/é"), "\"a/\\303\\251\"");
    }
}
