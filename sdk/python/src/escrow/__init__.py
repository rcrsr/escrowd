"""escrow: Python SDK for escrowd.

```python
import escrow

escrow.init(project="./proj", unscoped="deny")  # re-executes under `escrow run` if needed

with escrow.scope("docs", decide=lambda cs: escrow.commit()) as s:
    open("./proj/README.md", "w").write("# Demo\\n")
print(s.outcome.status, s.outcome.paths)
```
"""

from escrow._client import Client, connect
from escrow._sdk import (
    Change,
    ChangeSet,
    Decision,
    EscrowError,
    EscrowStaleHandleError,
    EscrowUnscopedError,
    EscrowUnscopedWarning,
    Outcome,
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
    "Read",
    "Review",
    "Scope",
    "commit",
    "connect",
    "current",
    "discard",
    "init",
    "scope",
    "send_back",
    "settle_unscoped",
]
