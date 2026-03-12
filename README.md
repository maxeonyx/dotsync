# dotsync

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

dotsync is an agent-first Rust CLI for managing dotfiles with a scope DAG: changes are committed to the right scope branch, cascaded through descendants, and then synced from the repo to `~/` so the repository stays the source of truth while machine and OS-specific differences remain explicit.

## Quick install

Release binaries coming soon. For now, clone and build locally with Rust:

```bash
cargo build --release
```

## Learn more

- Read `DESIGN.md` when you want the full design story, scope model, and command contract.
- Visit [dotsync.maxeonyx.com](https://dotsync.maxeonyx.com) for the project landing page.
- Source code lives at [github.com/maxeonyx/dotsync](https://github.com/maxeonyx/dotsync).
