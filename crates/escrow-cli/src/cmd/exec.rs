//! `escrow exec`: a command in a scope's sandbox, attached to this terminal.

use std::ffi::OsString;
use std::io::{self, IoSlice, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use escrowd::exec::{TOKEN_ENV, exec_socket, read_frame, write_frame};
use escrowd::proto::{SpawnEvent, SpawnRequest, SpawnSignal, spawn_event::Event};
use parking_lot::Mutex;
use prost::Message;
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
use tokio::signal::unix::{SignalKind, signal};

/// Signals relayed to a child: the ones a terminal or a supervisor sends.
const RELAYED: [i32; 6] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

/// The exec connection: sends the request with this process's stdio, then waits for the exit.
struct ExecConn {
    stream: UnixStream,
}

impl ExecConn {
    /// Connect to `<socket>.exec` and send the request with this process's stdin, stdout and stderr.
    fn start(socket: &Path, req: &SpawnRequest) -> io::Result<Self> {
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
    fn signaller(&self) -> io::Result<Signaller> {
        Ok(Signaller(Mutex::new(self.stream.try_clone()?)))
    }

    /// Wait for the child; returns its exit code.
    fn wait(&self) -> io::Result<i32> {
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

struct Signaller(Mutex<UnixStream>);

impl Signaller {
    fn send(&self, signal: i32) -> io::Result<()> {
        write_frame(&mut *self.0.lock(), &SpawnSignal { signal })
    }
}

fn lossy(s: impl AsRef<std::ffi::OsStr>) -> String {
    s.as_ref().to_string_lossy().into_owned()
}

pub(crate) async fn run(scope: String, socket: PathBuf, token: String, cmd: Vec<OsString>) -> anyhow::Result<i32> {
    let req = SpawnRequest {
        scope_id: scope,
        argv: cmd.iter().map(lossy).collect(),
        cwd: std::env::current_dir().map(lossy).unwrap_or_default(),
        env: std::env::vars_os()
            .filter(|(k, _)| k != TOKEN_ENV)
            .map(|(k, v)| (lossy(k), lossy(v)))
            .collect(),
        token,
    };
    let conn = ExecConn::start(&socket, &req).with_context(|| format!("connecting to {}", socket.display()))?;
    let signaller = Arc::new(conn.signaller()?);
    for sig in RELAYED {
        let mut stream = signal(SignalKind::from_raw(sig))?;
        let signaller = signaller.clone();
        tokio::spawn(async move {
            while stream.recv().await.is_some() {
                let _ = signaller.send(sig);
            }
        });
    }
    Ok(tokio::task::spawn_blocking(move || conn.wait()).await??)
}
