//! The exec socket (`<socket>.exec`): start a scope's child in its own sandbox.
//!
//! The app's sandbox cannot run bwrap (user namespaces are disabled inside), so
//! `escrow exec` asks the daemon. It sends its stdin, stdout and stderr over
//! SCM_RIGHTS with a `SpawnRequest`; the daemon starts the command in bwrap with
//! the scope's view over the project, in its own process group, and reports
//! `pid`, then `exit_code`; its other views (`$HOME`, `/tmp`) are mounted over
//! their roots. `SpawnSignal` frames are delivered to the command's
//! processes; a dropped connection kills the whole sandbox. The child gets no socket, so
//! it cannot reach the daemon. The request carries the scope's token; the child's
//! environment gets neither the socket nor the token (`ESCROW_SCOPE_TOKEN`).
//!
//! Frames: a little-endian u32 length, then one protobuf message.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use prost::Message;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage, SendFlags,
};

use crate::proto::{SpawnEvent, SpawnRequest, SpawnSignal, spawn_event::Event};
use crate::sandbox::{Mount, Sandbox};
use crate::views::{RootView, Views};

const MAX_FRAME: usize = 16 << 20;

/// Where `escrow exec` takes the scope's token from; never passed to the child.
pub const TOKEN_ENV: &str = "ESCROW_SCOPE_TOKEN";

/// The exec socket beside the gRPC socket.
pub fn exec_socket(socket: &Path) -> PathBuf {
    let mut s = socket.as_os_str().to_os_string();
    s.push(".exec");
    s.into()
}

fn write_frame(w: &mut impl Write, msg: &impl Message) -> io::Result<()> {
    let body = msg.encode_to_vec();
    let mut buf = (body.len() as u32).to_le_bytes().to_vec();
    buf.extend(body);
    w.write_all(&buf)
}

fn read_body<M: Message + Default>(r: &mut impl Read, len: usize) -> io::Result<M> {
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    M::decode(body.as_slice()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The next frame, or None at a clean end of stream.
fn read_frame<M: Message + Default>(r: &mut impl Read) -> io::Result<Option<M>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        r => r?,
    }
    read_body(r, u32::from_le_bytes(len) as usize).map(Some)
}

fn kill_group(pgid: i32, signal: i32) {
    // SAFETY: kill(2) has no memory-safety preconditions.
    unsafe { libc::kill(-pgid, signal) };
}

/// (parent pid, process group) of `pid`, from `/proc/<pid>/stat`.
fn ppid_pgrp(pid: i32) -> Option<(i32, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; fields resume after the last ')'.
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace().skip(1);
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

/// Deliver a relayed signal to the sandboxed command and its descendants, not to
/// bwrap itself: the outer bwrap (the group leader) dies of most signals, and
/// `--die-with-parent` then SIGKILLs the command before it can handle the signal.
/// The PID namespace's init (bwrap's child) is skipped too.
fn signal_command(pgid: i32, signal: i32) {
    let Ok(dir) = std::fs::read_dir("/proc") else { return };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        if pid == pgid {
            continue;
        }
        if let Some((ppid, pgrp)) = ppid_pgrp(pid)
            && pgrp == pgid
            && ppid != pgid
        {
            // SAFETY: kill(2) has no memory-safety preconditions.
            unsafe { libc::kill(pid, signal) };
        }
    }
}

/// Running children per scope. A closing scope takes no new children, so close
/// can stop them all, then freeze the scope.
#[derive(Default)]
struct Registry {
    /// Process groups of running children, per scope.
    running: HashMap<String, Vec<i32>>,
    /// Scopes being closed: no new children.
    closing: HashSet<String>,
}

pub struct Children {
    inner: Mutex<Registry>,
    gone: Condvar,
    /// How long a closing scope's children get between SIGTERM and SIGKILL.
    grace: Duration,
}

impl Default for Children {
    fn default() -> Self {
        Children::new(Duration::from_secs(2))
    }
}

impl Children {
    pub fn new(grace: Duration) -> Self {
        Children {
            inner: Mutex::default(),
            gone: Condvar::new(),
            grace,
        }
    }

    fn remove(&self, scope: &str, pgid: i32) {
        let mut g = self.inner.lock();
        if let Some(v) = g.running.get_mut(scope) {
            v.retain(|p| *p != pgid);
            if v.is_empty() {
                g.running.remove(scope);
            }
        }
        self.gone.notify_all();
    }

    /// Refuse new children of `scope` and stop its running ones: SIGTERM to each
    /// command (not to bwrap, whose death would SIGKILL it), up to the grace
    /// period for them to exit, then SIGKILL; wait for their sandboxes to be gone.
    /// Returns the sandboxes' process groups: their last processes may still be
    /// exiting, writing back dirty pages (`Views::close_scope_after`).
    pub fn stop(&self, scope: &str) -> Vec<i32> {
        let mut g = self.inner.lock();
        g.closing.insert(scope.to_string());
        let pgids = g.running.get(scope).cloned().unwrap_or_default();
        if pgids.is_empty() {
            return pgids;
        }
        for pgid in &pgids {
            signal_command(*pgid, libc::SIGTERM);
        }
        g = self.wait_gone(g, scope, self.grace);
        for pgid in g.running.get(scope).into_iter().flatten() {
            kill_group(*pgid, libc::SIGKILL);
        }
        drop(self.wait_gone(g, scope, Duration::from_secs(10)));
        pgids
    }

    fn wait_gone<'a>(
        &self,
        mut g: MutexGuard<'a, Registry>,
        scope: &str,
        within: Duration,
    ) -> MutexGuard<'a, Registry> {
        let deadline = Instant::now() + within;
        while g.running.contains_key(scope) && Instant::now() < deadline {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100));
            self.gone.wait_for(&mut g, left);
        }
        g
    }

    /// The scope accepts children again (return to agent), or is gone.
    pub fn release(&self, scope: &str) {
        self.inner.lock().closing.remove(scope);
    }

    pub fn stop_all(&self) {
        let scopes: Vec<String> = self.inner.lock().running.keys().cloned().collect();
        for s in scopes {
            self.stop(&s);
        }
    }
}

pub struct ExecServer {
    pub views: Arc<Views>,
    pub sandbox: Sandbox,
    pub project: PathBuf,
    pub mount: PathBuf,
    pub children: Arc<Children>,
}

impl ExecServer {
    /// Serve one connection: start the child, relay signals, report its exit.
    pub fn handle(&self, stream: UnixStream) -> io::Result<()> {
        let (len, fds) = recv_len_with_fds(&stream)?;
        let mut reader = &stream;
        let req: SpawnRequest = read_body(&mut reader, len)?;
        let mut out = &stream;
        let mut child = match self.start(&req, fds) {
            Ok(c) => c,
            Err(e) => {
                return write_frame(
                    &mut out,
                    &SpawnEvent {
                        event: Some(Event::Error(e.to_string())),
                    },
                );
            }
        };
        let pgid = child.id() as i32;
        let exited = Arc::new(AtomicBool::new(false));
        let relay = {
            let (stream, exited) = (stream.try_clone()?, exited.clone());
            std::thread::spawn(move || {
                let mut r = &stream;
                loop {
                    match read_frame::<SpawnSignal>(&mut r) {
                        Ok(Some(s)) if !exited.load(Ordering::Acquire) => signal_command(pgid, s.signal),
                        Ok(Some(_)) => {}
                        // The caller is gone: its child goes too.
                        _ => {
                            if !exited.load(Ordering::Acquire) {
                                kill_group(pgid, libc::SIGKILL);
                            }
                            return;
                        }
                    }
                }
            })
        };
        let _ = write_frame(
            &mut out,
            &SpawnEvent {
                event: Some(Event::Pid(pgid)),
            },
        );
        let status = child.wait();
        exited.store(true, Ordering::Release);
        self.children.remove(&req.scope_id, pgid);
        let code = match status {
            Ok(s) => s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)),
            Err(_) => 127,
        };
        let r = write_frame(
            &mut out,
            &SpawnEvent {
                event: Some(Event::ExitCode(code)),
            },
        );
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let _ = relay.join();
        r
    }

    /// The caller's cwd inside the scope's sandbox: a path in a view (as the app's
    /// sandbox or the host sees it) maps to its root, a path in a served root stays,
    /// anything else is the project root.
    fn chdir(&self, scope: &str, cwd: &str, roots: &[RootView]) -> PathBuf {
        let cwd = Path::new(cwd);
        let hosts = std::iter::once((self.mount.join(scope), self.project.as_path()))
            .chain(roots.iter().map(|r| (r.view.clone(), r.host.as_path())));
        for (view, host) in hosts {
            let name = view.file_name().unwrap_or_default();
            for view in [Path::new("/escrow").join(name), view.clone()] {
                if let Ok(rest) = cwd.strip_prefix(&view) {
                    return host.join(rest);
                }
            }
        }
        let mut served = std::iter::once(self.project.as_path()).chain(roots.iter().map(|r| r.host.as_path()));
        if served.any(|h| cwd.starts_with(h)) {
            return cwd.to_path_buf();
        }
        self.project.clone()
    }

    fn start(&self, req: &SpawnRequest, fds: Vec<OwnedFd>) -> io::Result<std::process::Child> {
        let invalid = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);
        if req.argv.is_empty() {
            return Err(invalid("empty argv".into()));
        }
        let [stdin, stdout, stderr]: [OwnedFd; 3] = fds
            .try_into()
            .map_err(|_| invalid("expected stdin, stdout and stderr".into()))?;
        let id = &req.scope_id;
        // Held while checking and spawning, so close never misses a child.
        let mut g = self.children.inner.lock();
        let h = self.views.scope(id).map_err(|_| invalid(format!("no scope {id}")))?;
        self.views.check_token(id, &req.token)?;
        if h.is_closed() || g.closing.contains(id) {
            return Err(invalid(format!("scope {id} is closed")));
        }
        let argv: Vec<OsString> = req.argv.iter().map(OsString::from).collect();
        let roots = self.views.root_views(id);
        let mut mounts = vec![Mount::Bind(self.mount.join(id), self.project.clone())];
        mounts.extend(roots.iter().map(|r| Mount::Bind(r.view.clone(), r.host.clone())));
        let mut cmd = self.sandbox.command(&mounts, &self.chdir(id, &req.cwd, &roots), &argv);
        cmd.env_clear().envs(
            req.env
                .iter()
                .filter(|(k, _)| !k.starts_with("ESCROW_SOCKET") && *k != TOKEN_ENV),
        );
        // A home view is mounted at the daemon's $HOME.
        if let Some(home) = roots
            .iter()
            .find(|r| Some(r.host.as_path()) == self.sandbox.home.as_deref())
        {
            cmd.env("HOME", &home.host);
        }
        cmd.stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .process_group(0);
        let child = cmd.spawn()?;
        self.views.log_sandbox(id, &self.sandbox.write);
        g.running.entry(id.clone()).or_default().push(child.id() as i32);
        Ok(child)
    }
}

fn recv_len_with_fds(stream: &UnixStream) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut len = [0u8; 4];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
    let mut cmsg = RecvAncillaryBuffer::new(&mut space);
    let n = rustix::net::recvmsg(
        stream,
        &mut [IoSliceMut::new(&mut len)],
        &mut cmsg,
        RecvFlags::CMSG_CLOEXEC,
    )?
    .bytes;
    let mut fds = Vec::new();
    for msg in cmsg.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = msg {
            fds.extend(received);
        }
    }
    if n < 4 {
        let mut r = stream;
        r.read_exact(&mut len[n..])?;
    }
    Ok((u32::from_le_bytes(len) as usize, fds))
}

/// Client side, used by `escrow exec`.
pub struct ExecConn {
    stream: UnixStream,
}

impl ExecConn {
    /// Connect to `<socket>.exec` and send the request with this process's stdin, stdout and stderr.
    pub fn start(socket: &Path, req: &SpawnRequest) -> io::Result<Self> {
        let stream = UnixStream::connect(exec_socket(socket))?;
        let body = req.encode_to_vec();
        let len = (body.len() as u32).to_le_bytes();
        // SAFETY: fds 0, 1 and 2 stay open for the whole call.
        let stdio = unsafe { [0, 1, 2].map(|fd| BorrowedFd::borrow_raw(fd)) };
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
        let mut cmsg = SendAncillaryBuffer::new(&mut space);
        cmsg.push(SendAncillaryMessage::ScmRights(&stdio));
        let n = rustix::net::sendmsg(stream.as_fd(), &[IoSlice::new(&len)], &mut cmsg, SendFlags::empty())?;
        let mut w = &stream;
        w.write_all(&len[n..])?;
        w.write_all(&body)?;
        Ok(ExecConn { stream })
    }

    /// A handle that delivers signals to the child while `wait` runs.
    pub fn signaller(&self) -> io::Result<Signaller> {
        Ok(Signaller(Mutex::new(self.stream.try_clone()?)))
    }

    /// Wait for the child; returns its exit code.
    pub fn wait(&self) -> io::Result<i32> {
        let mut r = &self.stream;
        loop {
            match read_frame::<SpawnEvent>(&mut r)?.and_then(|e| e.event) {
                Some(Event::Pid(_)) => {}
                Some(Event::ExitCode(c)) => return Ok(c),
                Some(Event::Error(m)) => return Err(io::Error::other(m)),
                None => return Err(io::Error::other("daemon closed the exec connection")),
            }
        }
    }
}

pub struct Signaller(Mutex<UnixStream>);

impl Signaller {
    pub fn send(&self, signal: i32) -> io::Result<()> {
        write_frame(&mut *self.0.lock(), &SpawnSignal { signal })
    }
}
