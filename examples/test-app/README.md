# test-app

The phase 1 test app: each exit test's IO with ordinary Python file and subprocess calls,
run under escrowd through the Python SDK (`docs/phase-1-poc.md`, sub-phase 1.7).

```bash
python app.py PROJECT MODE POLICY CHECK
```

`escrow.init()` re-executes the app under `escrow run` with PROJECT in the unscoped MODE
(`passthrough`, `implicit` or `deny`) and the policy file POLICY. CHECK is one of
`escrowed`, `child`, `concurrent`, `discarded`, `atomic`, `conflict`, `denied-read`,
`unscoped` and `snapshot`. The app prints one JSON object: what it saw, each scope's
outcome and, under `scopes`, the path prefixes each scope id may touch.

`tests/conformance/test_app.py` seeds the project, runs each check and fails on any ledger
entry that names a scope for an operation it did not perform.
