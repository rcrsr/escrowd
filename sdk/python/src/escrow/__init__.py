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
from escrow._sdk import (
    Change,
    ChangeSet,
    Decision,
    EscrowError,
    EscrowStaleHandleError,
    EscrowUnscopedError,
    EscrowUnscopedWarning,
    Outcome,
    Process,
    Read,
    Review,
    Scope,
    commit,
    current,
    discard,
    init,
    scope,
    send_back,
    settle_unscoped,
)

__all__ = [
    "Change",
    "ChangeSet",
    "Client",
    "Decision",
    "EscrowError",
    "EscrowStaleHandleError",
    "EscrowUnscopedError",
    "EscrowUnscopedWarning",
    "Outcome",
    "Process",
    "Read",
    "Review",
    "Reviewer",
    "Scope",
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
