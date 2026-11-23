"""escrow: Python SDK for escrowd.

```python
import escrow

escrow.init(project="./proj", unscoped="deny")  # re-executes under `escrow run` if needed

with escrow.scope("docs", decide=lambda cs: escrow.commit()) as s:
    open("./proj/README.md", "w").write("# Demo\\n")
print(s.outcome.status, s.outcome.paths)
```
"""

from escrow._client import Client, Reviewer, connect, connect_reviewer
from escrow._errors import (
    EscrowAbortedError,
    EscrowError,
    EscrowInvalidArgumentError,
    EscrowNotFoundError,
    EscrowPermissionError,
    EscrowRpcError,
    EscrowStaleHandleError,
    EscrowStateError,
    EscrowTimeoutError,
    EscrowUnavailableError,
    EscrowUnscopedError,
    EscrowUnscopedWarning,
    EscrowUnsupportedError,
)
from escrow._launch import init
from escrow._model import (
    Change,
    ChangeKind,
    ChangeSet,
    DaemonInfo,
    Decided,
    Decision,
    HeldDetail,
    HeldScope,
    OpenedScope,
    Outcome,
    Process,
    Read,
    Result,
    Review,
    ScopeRoot,
    Status,
    Tier,
    TierReview,
    Verdict,
    commit,
    discard,
    send_back,
)
from escrow._scope import Scope, current, scope, settle_unscoped

__all__ = [
    "Change",
    "ChangeKind",
    "ChangeSet",
    "Client",
    "DaemonInfo",
    "Decided",
    "Decision",
    "EscrowAbortedError",
    "EscrowError",
    "EscrowInvalidArgumentError",
    "EscrowNotFoundError",
    "EscrowPermissionError",
    "EscrowRpcError",
    "EscrowStaleHandleError",
    "EscrowStateError",
    "EscrowTimeoutError",
    "EscrowUnavailableError",
    "EscrowUnscopedError",
    "EscrowUnscopedWarning",
    "EscrowUnsupportedError",
    "HeldDetail",
    "HeldScope",
    "OpenedScope",
    "Outcome",
    "Process",
    "Read",
    "Result",
    "Review",
    "Reviewer",
    "Scope",
    "ScopeRoot",
    "Status",
    "Tier",
    "TierReview",
    "Verdict",
    "commit",
    "connect",
    "connect_reviewer",
    "current",
    "discard",
    "init",
    "scope",
    "send_back",
    "settle_unscoped",
]
