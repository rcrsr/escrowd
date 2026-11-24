# escrowd Python SDK

Python client for [escrowd](https://github.com/rcrsr/escrowd). escrowd captures every
filesystem write a program makes inside a scope, holds it in escrow, and applies or discards
the whole change set when the scope closes.

Install the distribution `escrowd`; import the package `escrow`. The SDK needs the `escrow`
binary (on `PATH` or in `ESCROW_EXE`), Linux, and Python 3.11 or newer. It speaks daemon
protocol 7 or newer; `escrow.init()` refuses an older daemon.

```python
import escrow

escrow.init(project="/abs/path/proj", unscoped="deny")  # re-executes under `escrow run`

with escrow.scope("docs", decide=lambda cs: escrow.commit()) as s:
    open("/abs/path/proj/README.md", "w").write("# Demo\n")
print(s.outcome.status, s.outcome.paths)
```

`escrow.Client` and `escrow.Reviewer` return model types; `escrow.lowlevel` has the same
calls returning the protobuf messages. See the
[repository README](https://github.com/rcrsr/escrowd#readme) for policies, review and limits.
