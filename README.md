# escrowd

escrowd holds every file write a program makes until you decide what to do with it.

You wrap a unit of work in a **scope**: a tool call, an agent turn, a whole request. Inside it, writes succeed and read back as normal, but nothing reaches the real project. When the scope closes, you get the complete change set with a diff and the process that made each change. Then one decision applies all of it or none of it.

escrowd is built for LLM agent harnesses. A Write tool, a bash redirect, a Python heredoc and a `git commit` all land in the same escrow, so your policy checks what an agent actually changed, not which tool it called.

```
  agent / tool call                        escrowd                          your project
 ┌────────────────────┐   writes   ┌──────────────────────────┐  commit  ┌──────────────┐
 │ open(), bash, git, │ ─────────▶ │ scope (copy-on-write)    │ ───────▶ │ real files   │
 │ subprocesses       │ ◀───────── │ change set + diff        │          │ (all or none)│
 └────────────────────┘ reads back └──────────────────────────┘          └──────────────┘
                                     │ policy rules → reviewers (LLM, human) → verdict
```

## Contents

- [Status](#status)
- [Requirements](#requirements)
- [Install](#install)
- [Quickstart: the CLI](#quickstart-the-cli)
- [Quickstart: the Python SDK](#quickstart-the-python-sdk)
- [Policy](#policy)
- [Reviewing held changes](#reviewing-held-changes)
- [Commands](#commands)
- [Where escrowd keeps its files](#where-escrowd-keeps-its-files)
- [Limitations](#limitations)
- [Further reading](#further-reading)

## Status

escrowd is pre-1.0 (version 0.1.0).

- **Protocol:** the gRPC protocol is frozen at v1 (`proto/escrow/v1/escrow.proto`, spec in [docs/protocol.md](docs/protocol.md)). Later versions only add fields and calls.
- **Tested on:** Ubuntu 24.04, Ubuntu 26.04, Debian 13 and Fedora 44.
- **Crash safety:** in 1,000 runs that kill the daemon mid-commit, every commit was applied whole or rolled back whole.
- **Speed:** on real repositories (installs, builds, test suites), escrowd runs at about 1.1× to 1.5× native speed. Read-heavy work costs more; see [Limitations](#limitations).

There are no prebuilt binaries yet; you build escrowd from source.

## Requirements

| Requirement | Why |
| --- | --- |
| Linux, kernel 6.8 or later | FUSE and namespace features escrowd relies on. macOS is not supported. |
| `fuse3` (provides `fusermount3`) | Mounts the scope views without root. |
| `bubblewrap` (`bwrap`) | Runs your program in a sandbox whose only way to the project is escrowd. |
| Rust 1.93 or later | Builds escrowd. The repository pins its toolchain, which `rustup` installs for you. |
| Python 3.11 or later | Only for the Python SDK. |

Install the system packages:

```bash
sudo apt install fuse3 bubblewrap     # Ubuntu, Debian
sudo dnf install fuse3 bubblewrap     # Fedora
```

## Install

1. Clone the repository and build the `escrow` binary:

   ```bash
   git clone https://github.com/rcrsr/escrowd.git
   cd escrowd
   cargo install --locked --path crates/escrow-cli
   ```

   This puts `escrow` in `~/.cargo/bin`. Check that it is on your `PATH`:

   ```bash
   escrow --version
   ```

2. **Ubuntu 24.04 and later only:** install escrowd's copy of bwrap with its AppArmor profile.

   ```bash
   packaging/ubuntu/install.sh
   ```

   Ubuntu blocks unprivileged user namespaces for programs without an AppArmor profile, so the system `bwrap` fails. The script copies `bwrap` to `/usr/lib/escrowd/bwrap` and loads a profile for that path; escrowd uses it automatically. Do not turn off the `kernel.apparmor_restrict_unprivileged_userns` setting instead. Run `packaging/ubuntu/install.sh remove` to uninstall.

3. **Python SDK (optional):** install it from the repository.

   ```bash
   uv add "escrow @ git+https://github.com/rcrsr/escrowd#subdirectory=sdk/python"
   # or
   pip install "escrow @ git+https://github.com/rcrsr/escrowd#subdirectory=sdk/python"
   ```

   The SDK runs the `escrow` binary. It finds it on `PATH`, or at the path in `ESCROW_EXE`.

## Quickstart: the CLI

`escrow run` starts a daemon for one project, runs your command in a sandbox, and settles the command's writes when it exits. Try it on a scratch project:

```bash
mkdir -p demo && echo base > demo/README.md

escrow run --project demo --unscoped implicit -- sh -c 'echo changed > README.md; cat README.md'
# changed
# escrow: 1 unscoped change(s) discarded

cat demo/README.md
# base
```

The command saw its own write, but the project kept the original. Add `--on-exit commit` to apply the writes instead:

```bash
escrow run --project demo --unscoped implicit --on-exit commit -- sh -c 'echo changed > README.md'
# escrow: 1 unscoped change(s) committed
```

`--unscoped` sets what happens to writes made outside any scope:

| Mode | Writes outside a scope | Use it for |
| --- | --- | --- |
| `deny` | Fail with "Read-only file system" (`EscrowUnscopedError` in Python) | Agent hosts: every change must go through a scope. |
| `implicit` | Go to a default scope named `unscoped`, settled at exit by `--on-exit` (default `discard`) | Wrapping an existing program whole. |
| `passthrough` | Reach the project directly, without escrow | Programs that open scopes for only part of their work. |

An `--on-exit commit` still goes through your policy. A change set that breaks a `write:` rule is discarded, and one that a `review:` rule sends to reviewers is held (see [Reviewing held changes](#reviewing-held-changes)).

Every operation is recorded in a ledger. Print it with:

```bash
escrow log --project demo
```

## Quickstart: the Python SDK

The SDK lets a program open scopes around parts of its work and decide each one in code. Save this as `agent.py` next to the `demo` folder:

```python
import subprocess
from pathlib import Path

import escrow

PROJECT = Path(__file__).resolve().parent / "demo"
escrow.init(project=PROJECT, unscoped="deny")


def check(cs: escrow.ChangeSet) -> escrow.Decision:
    if any(c.path.endswith(".tmp") for c in cs.changes):
        return escrow.discard("no temp files")
    return escrow.commit()


with escrow.scope("tool-call-1", decide=check) as s:
    (PROJECT / "notes.md").write_text("written by the agent\n")
    subprocess.run(["sh", "-c", "echo from a subprocess >> notes.md"], cwd=PROJECT, check=True)

print(s.outcome.status, s.outcome.paths)  # committed ['notes.md']
print(s.outcome.diff)
```

Run it with an absolute path to the script:

```bash
python "$PWD/agent.py"
```

What happens:

1. `escrow.init()` re-runs the script under `escrow run`, in the sandbox. The sandbox starts in the project folder, so the script needs an absolute path, and project paths in the script should be absolute too.
2. Inside the `with` block, file calls (`open`, `pathlib`, `os`, `shutil`) and subprocesses (`subprocess`, `os.system`, `os.posix_spawn`) write into the scope.
3. When the block exits, `check` receives the change set: each change's kind, path and writing process, plus a diff in git format.
4. Its decision is applied all or nothing. `s.outcome` holds the result.

An exception inside the block discards the scope. Decisions:

| Call | Effect |
| --- | --- |
| `escrow.commit()` | Apply the change set to the project. The default when `decide` is omitted. |
| `escrow.discard("reason")` | Drop the changes. |
| `escrow.send_back("reason")` | Keep the scope open with its changes and return the reasons, so the agent can fix its work. Continue it with `escrow.scope(resume=s)`. |

Your decision is a proposal: policy rules only make it stricter. `async with escrow.scope(...)` works the same way, and concurrent asyncio tasks keep separate scopes.

## Policy

A policy is a YAML file you pass with `--policy` (or `policy=` in `escrow.init()`). The daemon enforces it for every scope, whichever SDK opened it. Every key is optional except `version`.

```yaml
version: 1

read:
  deny: [".env", "secrets/**"]        # reads fail; a pattern without "/" matches at any depth

write:                                # broken rules discard the change set
  deny: ["*.pem"]                     # paths it must not create, change, delete or rename
  deny_content: ["BEGIN PRIVATE KEY"] # text no written file may contain
  only_by:                            # paths only these programs may change
    - {paths: [".git/**"], programs: ["/usr/bin/git"]}

review:                               # tiers a change set needs, by path; first match wins
  - {paths: ["src/auth/**"], tier: human}
  - {paths: ["docs/**"], tier: llm, wait: optional}

conflict:
  verdict: discard                    # or "return": reopen the scope, conflicting paths as reasons
  reads: false                        # true: files the scope only read also conflict

roots:                                # what the sandbox sees outside the project
  home:                               # $HOME (omitted: an empty folder)
    default: capture                  # rule for unlisted paths (default: deny)
    deny: [~/.ssh, ~/.aws, ~/.gnupg]
    passthrough: [~/.cache/pip, ~/.npm]
    ephemeral: [~/.bash_history]
  tmp: {default: ephemeral}           # /tmp (omitted: a private, empty /tmp)
  other:
    read: ["~/.local/share/mise"]     # host paths the sandbox may read
    passthrough: ["~/.cache/pnpm"]    # host paths the sandbox may write, outside escrow

close:
  grace_ms: 2000                      # SIGTERM to a closing scope's processes, SIGKILL after this

diff:
  file_bytes: 262144                  # larger files show as size and SHA-256 only
  max_bytes: 1048576                  # the diff stops before passing this size
```

Root rules for `roots.home` and `roots.tmp`:

| Rule | Effect |
| --- | --- |
| `capture` | Escrowed like the project: committed or discarded with the scope. |
| `ephemeral` | Writable scratch space per scope, always discarded. |
| `passthrough` | Shared with the host read-write, outside escrow (package caches). |
| `deny` | Access fails with "Permission denied" and is logged. |

**Conflicts.** Each scope works on a snapshot of the project. If a file the scope changed was also changed in the project since the snapshot (by an editor, or another scope's commit), the commit conflicts. Nothing is applied, and the scope is discarded or returned according to `conflict.verdict`.

**Sandbox defaults.** Without a policy, the sandbox sees the system folders (`/usr`, `/etc`, `/opt`) read-only, the project as `--unscoped` says, an empty `$HOME` and a private `/tmp`. Toolchains installed under your home folder need a `roots.other.read` entry or `escrow run --read PATH`.

## Reviewing held changes

A commit of a change set that matches a `review:` rule is **held**. Nothing reaches the project until every required tier approves it, and only reviewers can commit it. The tiers are `llm` and `human`; a change set that needs `human` also needs `llm`, and a human's verdict also counts for a pending `llm` tier.

While the program runs, review from another terminal:

```bash
escrow review --project demo list               # held scopes, oldest first
escrow review --project demo show s2            # tiers, changes with their writers, and the diff
escrow review --project demo commit s2 --reason "looks fine"
escrow review --project demo return s2 --reason "add a test"   # back to the agent to fix
escrow review --project demo discard s2
```

`--tier llm` gives an LLM tier's verdict; the default is `human`. Verdicts only get stricter, unless a human passes `--override`, which the ledger records.

By default, `escrow.scope()` waits for the verdict. Pass `wait=False` to continue: `s.outcome.status` is then `"held"`, and `s.wait_decided()` or `await s.decided()` returns the verdict later. Scopes that share a `session=` open one at a time while one of them is held with a wait.

The review socket (`<socket>.review`) has file mode 0600: anyone who can open it can review as any tier. No LLM reviewer ships yet; connect your own with `escrow.connect_reviewer()`.

If `escrow run` exits while its unscoped changes are held, the next `escrow run` on the project refuses to start until they are decided. Start a daemon on the project and decide them, then stop the daemon:

```bash
escrow daemon --project demo --socket "$XDG_RUNTIME_DIR/escrow-demo.sock" &
escrow review --socket "$XDG_RUNTIME_DIR/escrow-demo.sock" commit unscoped
kill %1
```

Unix socket paths are limited to 107 bytes, so keep `--socket` short.

## Commands

| Command | Does |
| --- | --- |
| `escrow run --project P --unscoped MODE -- CMD` | Start a daemon for project `P`, run `CMD` in the sandbox, settle at exit. |
| `escrow daemon --socket S --project P` | Run a long-lived daemon in the foreground, for clients that connect to socket `S`. |
| `escrow exec --scope ID -- CMD` | Run `CMD` in a scope's sandbox. Needs the scope's token in `ESCROW_SCOPE_TOKEN`. |
| `escrow diff SCOPE` | Print a closed, undecided scope's diff in git format. |
| `escrow review list\|show\|commit\|return\|discard` | Review held scopes. |
| `escrow log [SCOPE]` | Print the ledger, optionally for one scope. |

Commands that talk to a daemon take `--project P` (to find the socket of `escrow run`) or `--socket S`. Run `escrow <command> --help` for every option.

## Where escrowd keeps its files

| What | Default location |
| --- | --- |
| Scope stores, journal, history, ledger (`ledger.log`) | `$XDG_STATE_HOME/escrowd/<project-id>/` (usually `~/.local/state/escrowd/…`) |
| Scope views (FUSE mount) | `$XDG_RUNTIME_DIR/escrowd/<project-id>/view/` |
| Sockets (`escrow.sock`, `.exec`, `.review`) | `$XDG_RUNTIME_DIR/escrowd/<project-id>/` |

`<project-id>` is the project folder's name plus a hash of its full path. One daemon serves a state folder at a time; a second one fails to start. If a daemon is killed, its mount goes stale. Unmount it with `fusermount3 -u -z <view path>`; the next start rolls back any half-applied commit.

## Limitations

The main ones:

- **Native code** that does its own file IO, and `os.fork`, `os.exec*` and `os.spawn*`, bypass the SDK's scope. Their writes fall to the `--unscoped` mode. Run such work in a subprocess.
- **Network IO** is not captured.
- **Read-heavy work is slower.** Every file open costs a round trip to the daemon: warm `rg` runs at about 3.6× native, warm `git log -p` at about 1.8×.
- **Linux only**, kernel 6.8 or later.
- **No nested scopes**, and one daemon per `escrow run`.

[LIMITATIONS.md](LIMITATIONS.md) lists every known limit and where it is tracked.

## Further reading

- [docs/protocol.md](docs/protocol.md): the protocol, for writing a client in another language.
- [docs/escrowd-proposal.md](docs/escrowd-proposal.md): the design, threat model and roadmap.
- [examples/test-app](examples/test-app): a Python app that exercises every capture case.
- [CLAUDE.md](CLAUDE.md): contributor notes (layout, build, test commands).

## License

Apache License 2.0. See [LICENSE](LICENSE).
