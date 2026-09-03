# Contributing to composemux

Contributions are welcome — bug reports, fixes, docs, and features alike.

Please read the section below before opening a pull request that changes
behaviour. This project has an unusual design constraint, and it is the most
common reason a well-written PR would be turned down.

## The one unusual rule: this is a port

composemux deliberately reproduces the interaction model of the
[Nx terminal UI](https://nx.dev/blog/nx-21-terminal-ui) — its keybindings, its
layout arithmetic, its colours, its pinning semantics. That is the point of the
tool. People arrive already knowing how to drive it because they use the Nx TUI
on other projects, and that transfer is the feature.

So **matching Nx is a constraint, not an accident**:

- A change that makes a keybinding "more intuitive" but diverges from Nx will
  usually be declined, even if the new binding is genuinely better in isolation.
- Layout constants (`⌊width/3⌋` sidebars, the auto-layout breakpoints, scroll
  momentum figures) are ported values, not tuned ones. Don't adjust them to
  taste.
- Modules derived from Nx carry a header comment naming the upstream file. Keep
  those comments accurate when you edit the module.

Deviations are possible, but they need a reason that comes from *compose
services being long-running where Nx tasks are short*. Two exist today, both
documented in the README under "Deliberate differences from the Nx TUI". If you
think you have a third, open an issue and make the case before writing the code.

If you are adding code adapted from Nx (or anywhere else), say so in the PR and
note the upstream file. See [LICENSE-THIRD-PARTY](LICENSE-THIRD-PARTY).

## Getting set up

You need Rust 1.88 or newer (the MSRV, set by `ratatui`):

```sh
git clone https://github.com/sofired/composemux
cd composemux
cargo build
```

Docker is **not** required to build or to run the test suite — the tests use
pure functions, a `TestBackend`, and a fake environment rather than a live
daemon. You will want Docker to exercise the tool for real:

```sh
cargo run -- --project <some-running-compose-project>
```

## Before you open a PR

CI runs these on Linux, macOS and Windows, and they must pass:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --no-default-features   # clipboard fallback is optional
```

## Tests

The suite is the main safety net for a port — it is what stops an innocuous
refactor from silently changing behaviour that is supposed to match Nx. New
behaviour should come with tests, and the existing style is worth matching:

- **Name the scenario and the outcome**, not the function under test
  (`a_running_container_whose_task_died_is_reattached`, not `test_resync`).
- **Prefer pure functions over mocks.** Where logic needs Docker or a terminal,
  the decision is usually extractable — see `event_decision` and
  `plan_attachments` in `src/docker/stream.rs`, or `build_service` in
  `src/docker/client.rs`.
- **Inject time rather than sleeping.** `ScrollMomentum::scroll` and `App::tick`
  both take a `now: Instant` for this reason.
- **Assert on observable behaviour.** A test that would still pass with the
  feature deleted is worse than no test, because it reads as coverage.

Rendering is tested by drawing into a `ratatui::backend::TestBackend` and
asserting on the resulting buffer (`src/tui/render.rs`), and the layout
arithmetic has property tests that sweep every terminal size.

## Where things live

```
src/config.rs          config file + CLI flag merging
src/project.rs         compose project-name resolution
src/docker/            Engine API access, log streaming, event supervision
src/model/             Service/status types, and the vt100-backed log buffer
src/tui/app.rs         state machine: focus, pinning, filter, key dispatch
src/tui/components/    rendering
src/fallback.rs        plain streaming when stdout is not a TTY
```

## Scope

composemux is **read-only** by design. It attaches to containers that something
else started and never starts, stops, restarts or execs into them. It is built
to sit inside a wrapper script that owns `compose up` and `compose down`, so its
exit codes and terminal restoration are part of its contract with that caller.

PRs that add lifecycle control would change what the tool *is*, so please open
an issue first rather than arriving with an implementation.

## Commits and pull requests

- Keep the subject line short and in the imperative ("Fix …", not "Fixed …").
- Explain *why* in the body when the reason isn't obvious from the diff.
- One logical change per PR where you can manage it.
- Update the README if you change user-visible behaviour, and say so in the PR.

## Licensing of contributions

composemux is MIT licensed, and stays MIT: it is derived from Nx, which is MIT,
and keeping a single permissive licence keeps the provenance unambiguous.

By submitting a contribution you agree that it is licensed under the MIT
Licence, the same terms as the project ("inbound = outbound"). You keep the
copyright in your own work; there is no CLA and no copyright assignment.

Please sign off your commits to certify you have the right to submit them under
that licence — this is the [Developer Certificate of Origin](https://developercertificate.org/):

```sh
git commit -s
```

## Reporting bugs

Include your OS, `composemux --version`, `docker version`, and what the terminal
was doing. For streaming or attachment problems, `COMPOSEMUX_DEBUG=1` writes
diagnostics to `composemux.log` in your system temp directory — the UI can't log
to stdout while it owns the screen.
