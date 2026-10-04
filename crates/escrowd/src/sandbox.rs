//! bwrap command lines for escrowd's sandboxes: the app's (from `escrow run`) and
//! each scope's children (from the exec socket).
//!
//! Binds are deny-by-default: the system directories read-only, the policy's
//! `sandbox.read` paths read-only, the `sandbox.write` paths read-write (outside
//! escrow: package stores, caches), a private `/tmp` and an empty `$HOME`, then
//! the caller's mounts (the project view, the socket). escrowd's own state, its
//! views and its sockets are hidden even when a read path contains them. User
//! namespaces are disabled inside, so nothing in the sandbox can remount.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SYSTEM: &[&str] = &[
    "/usr", "/etc", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/opt",
];

pub enum Mount {
    /// Read-write bind of a host path (source) at a sandbox path (destination).
    Bind(PathBuf, PathBuf),
    /// Read-only bind.
    RoBind(PathBuf, PathBuf),
}

pub struct Sandbox {
    pub bwrap: PathBuf,
    /// Host paths bound read-only besides the system directories (policy `sandbox.read`).
    pub read: Vec<PathBuf>,
    /// Host paths bound read-write, outside escrow (policy `sandbox.write`); they exist.
    pub write: Vec<PathBuf>,
    /// Directories that must stay hidden (state, views) even under a read path.
    pub hide_dirs: Vec<PathBuf>,
    /// Files that must stay hidden (the sockets) even under a read path.
    pub hide_files: Vec<PathBuf>,
    /// Replaced by an empty tmpfs.
    pub home: Option<PathBuf>,
}

/// `explicit`, else `$ESCROW_BWRAP`, else escrowd's own bwrap (Ubuntu, AppArmor profile), else `bwrap`.
pub fn find_bwrap(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(p) = std::env::var_os("ESCROW_BWRAP").filter(|p| !p.is_empty()) {
        return p.into();
    }
    let ours = Path::new("/usr/lib/escrowd/bwrap");
    if ours.is_file() {
        return ours.to_path_buf();
    }
    "bwrap".into()
}

impl Sandbox {
    fn exposed(&self, p: &Path) -> bool {
        SYSTEM.iter().any(|s| p.starts_with(s)) || self.read.iter().chain(&self.write).any(|r| p.starts_with(r))
    }

    /// `bwrap … -- argv` with `mounts` applied last and the working directory `chdir`.
    pub fn command(&self, mounts: &[Mount], chdir: &Path, argv: &[OsString]) -> Command {
        let mut a: Vec<OsString> = Vec::new();
        let mut push = |args: &[&dyn AsRef<std::ffi::OsStr>]| a.extend(args.iter().map(|s| s.as_ref().to_os_string()));
        push(&[
            &"--unshare-user",
            &"--disable-userns",
            &"--unshare-pid",
            &"--die-with-parent",
        ]);
        for dir in SYSTEM {
            match fs::symlink_metadata(dir) {
                Ok(m) if m.file_type().is_symlink() => {
                    if let Ok(target) = fs::read_link(dir) {
                        push(&[&"--symlink", &target, dir]);
                    }
                }
                Ok(m) if m.is_dir() => push(&[&"--ro-bind", dir, dir]),
                _ => {}
            }
        }
        push(&[&"--dev", &"/dev", &"--proc", &"/proc", &"--tmpfs", &"/tmp"]);
        if let Some(home) = &self.home {
            push(&[&"--tmpfs", home]);
        }
        for r in &self.read {
            push(&[&"--ro-bind-try", r, r]);
        }
        for w in &self.write {
            push(&[&"--bind", w, w]);
        }
        for d in self.hide_dirs.iter().filter(|d| self.exposed(d)) {
            push(&[&"--tmpfs", d]);
        }
        for f in self.hide_files.iter().filter(|f| self.exposed(f)) {
            push(&[&"--ro-bind", &"/dev/null", f]);
        }
        for m in mounts {
            match m {
                Mount::Bind(src, dst) => push(&[&"--bind", src, dst]),
                Mount::RoBind(src, dst) => push(&[&"--ro-bind", src, dst]),
            }
        }
        push(&[&"--chdir", &chdir, &"--"]);
        a.extend(argv.iter().cloned());
        let mut cmd = Command::new(&self.bwrap);
        cmd.args(a);
        cmd
    }
}
