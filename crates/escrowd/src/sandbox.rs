//! bwrap command lines for escrowd's sandboxes: the app's (from `escrow run`) and
//! each scope's children (from the exec socket).
//!
//! Binds are deny-by-default: the system directories read-only, a private `/tmp`
//! and an empty `$HOME` (unless a root's view is mounted there), the read paths
//! read-only, the passthrough paths read-write (outside escrow: package stores,
//! caches), then the caller's mounts (the views, the socket). Binds apply
//! shallowest first, so a path inside a view (the project in `$HOME`, a package
//! cache) is bound over it. escrowd's own state, its views and its sockets are
//! hidden even when a read path contains them. User namespaces are disabled
//! inside, so nothing in the sandbox can remount.

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

impl Mount {
    pub fn dst(&self) -> &Path {
        match self {
            Mount::Bind(_, d) | Mount::RoBind(_, d) => d,
        }
    }
}

pub struct Sandbox {
    pub bwrap: PathBuf,
    /// Host paths bound read-only besides the system directories (policy `roots.other.read`).
    pub read: Vec<PathBuf>,
    /// Host paths bound read-write, outside escrow (policy passthrough rules); they exist.
    pub write: Vec<PathBuf>,
    /// Directories that must stay hidden (state, views) even under a read path.
    pub hide_dirs: Vec<PathBuf>,
    /// Files that must stay hidden (the sockets) even under a read path.
    pub hide_files: Vec<PathBuf>,
    /// An empty tmpfs unless a mount puts a view there.
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
        let mounted = |p: &Path| mounts.iter().any(|m| m.dst() == p);
        push(&[&"--dev", &"/dev", &"--proc", &"/proc"]);
        if !mounted(Path::new("/tmp")) {
            push(&[&"--tmpfs", &"/tmp"]);
        }
        if let Some(home) = self.home.as_ref().filter(|h| !mounted(h)) {
            push(&[&"--tmpfs", home]);
        }
        // Shallowest destination first; a stable sort keeps hides after what exposes them.
        let mut binds: Vec<(&Path, [&dyn AsRef<std::ffi::OsStr>; 3])> = Vec::new();
        for m in mounts {
            binds.push(match m {
                Mount::Bind(src, dst) => (dst.as_path(), [&"--bind", src, dst]),
                Mount::RoBind(src, dst) => (dst.as_path(), [&"--ro-bind", src, dst]),
            });
        }
        for r in &self.read {
            binds.push((r, [&"--ro-bind-try", r, r]));
        }
        for w in &self.write {
            binds.push((w, [&"--bind", w, w]));
        }
        for d in self.hide_dirs.iter().filter(|d| self.exposed(d)) {
            binds.push((d, [&"--tmpfs", d, &""]));
        }
        for f in self.hide_files.iter().filter(|f| self.exposed(f)) {
            binds.push((f, [&"--ro-bind", &"/dev/null", f]));
        }
        binds.sort_by_key(|(dst, _)| dst.components().count());
        for (_, args) in &binds {
            let n = if args[2].as_ref().is_empty() { 2 } else { 3 };
            push(&args[..n]);
        }
        push(&[&"--chdir", &chdir, &"--"]);
        a.extend(argv.iter().cloned());
        let mut cmd = Command::new(&self.bwrap);
        cmd.args(a);
        cmd
    }
}
