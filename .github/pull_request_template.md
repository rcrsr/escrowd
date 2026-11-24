## What and why

<!-- One paragraph: the change and the reason. Link the phase plan item or issue. -->

## Checks

- [ ] `pnpm run check:affected` and `pnpm run test:affected` pass locally
- [ ] `docs/protocol.md` updated with any proto change (additive only)
- [ ] `LIMITATIONS.md` updated if a limit was found or lifted
- [ ] `ci:repeat` label added if this touches the commit path, FUSE, the sandbox or concurrency
