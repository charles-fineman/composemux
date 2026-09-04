# composemux

A terminal UI for streaming Docker Compose service logs, modelled closely on the
[Nx terminal UI](https://nx.dev/blog/nx-21-terminal-ui).

`docker compose logs -f` interleaves every service into one stream, so the error
you care about scrolls away and there is no way to keep one service in front of
you while you browse the others. `composemux` gives you a navigable list of
services with live status, an output pane for the selected one, and the ability
to **pin** one or two services so they stay on screen.

If you already use the Nx TUI, the keys and the look are the same on purpose.

## Install

```sh
cargo install composemux
```

Prebuilt binaries are attached to each [release](https://github.com/sofired/composemux/releases)
for Linux (x86-64 gnu and musl, arm64), macOS (Apple Silicon) and Windows
(x86-64). On Intel Macs, build from source with `cargo install` — GitHub has
retired its Intel macOS runners, so there is no longer a machine to build that
binary on.

## Use

Run it inside a directory with a running Compose project:

```sh
composemux
```

Or name the project explicitly — which is what you want when a script launches it:

```sh
composemux --project my-stack --pin api --pin worker
```

`composemux` is **read-only**. It attaches to containers that are already
running and never starts, stops or execs into anything, so it is safe to drop
into a script that owns the lifecycle itself.

### Options

| Flag | Meaning |
|---|---|
| `-p, --project <NAME>` | Compose project to attach to. Defaults to `$COMPOSE_PROJECT_NAME`, else the directory name. |
| `-c, --config <PATH>` | Config file. Defaults to the nearest `.composemux.yaml`. |
| `--pin <SERVICE>` | Pin a service to an output pane at startup. Repeatable, max two. |
| `--tail <N>` | Lines of history to load per service before following. |
| `--scrollback <N>` | Rows of output retained per service (default 1000, ~7 MB each). |
| `--no-tui` | Stream plain prefixed lines instead of the full-screen UI. |

## Keys

**Service list**

| Key | Action |
|---|---|
| `↑` `↓` / `k` `j` | Move the selection |
| `1` / `2` | Pin the selected service to output pane 1 or 2 |
| `0` | Close every pane |
| `space` | Open a single pane that follows the selection |
| `enter` | Open the selected service's pane and focus it |
| `tab` / `shift+tab` | Move focus between the list and the panes |
| `b` | Hide or show the service list |
| `m` | Switch between stacked and side-by-side layouts |
| `/` | Filter services; `enter` confirms, `esc` clears |

Pressing `1` or `2` again on a service that already occupies that pane unpins
it. Pinning a service that sits in the *other* pane moves it across rather than
opening a second copy.

**Output pane**

| Key | Action |
|---|---|
| `↑` `↓` / `k` `j` | Scroll (accelerates while held) |
| `ctrl+u` / `ctrl+d` | Scroll half a page |
| `Home` / `End` | Jump to the start or end |
| `c` | Copy the buffer to the clipboard |
| `esc` | Return to the service list |

**Anywhere:** `?` help · `q` quit · `ctrl+c` interrupt · `F10` toggle mouse capture.

## Configuration

Optional. Put `.composemux.yaml` beside your compose file:

```yaml
project: my-stack       # usually passed as --project instead
include: [api, worker]  # empty means every service
exclude: [migrate]
pinned:  [api, db]      # pane 1 and pane 2 at startup
tail: 200               # lines of history per service
scrollback: 1000        # rows retained per service (~7 MB each)
auto_exit: 3            # seconds to wait once every service has exited; false disables
```

Unknown keys are rejected rather than ignored, so a typo is a loud error instead
of a silently missing pin.

## Using it from a script

`composemux` is built to sit inside a wrapper that owns the compose lifecycle:
bring the project up, block on the TUI, then tear down when it exits.

- **Exit codes:** `0` when the user quits with `q` or the stack exits on its own,
  `130` on `ctrl+c`, non-zero on error. That lets a caller tell a deliberate quit
  from an interrupt.
- **Terminal restoration** runs on every exit path, including panics and
  `SIGTERM`/`SIGHUP`, so the calling script never inherits a raw-mode terminal.
- **Non-TTY output** (piped, or in CI) automatically falls back to plain prefixed
  line streaming, so nothing writes escape sequences into a log file.
- **Auto-exit:** once every service has exited *cleanly*, a countdown appears and
  the tool closes so the wrapper can clean up. Any keypress cancels it. If any
  service exited non-zero the countdown does **not** run — closing would let the
  wrapper tear the project down before anyone read the crash.

Invoke the binary directly rather than through a task runner that captures child
output — a TUI nested inside another TUI renders neither.

### Known limitations

- If a container's log stream drops while the container keeps running (a daemon
  restart, say), the reconnect resumes from a one-second boundary, so a couple of
  already-visible lines can appear twice. The Engine API takes no finer
  resolution here, and a duplicated line is preferable to a missing one.
- Scroll anchoring stops once a service's buffer saturates; see above.

## How it works

Logs come from the Docker Engine API rather than by parsing `docker compose logs`
output, which means per-service streams, real container status, and exit codes.
A supervisor watches Docker events, so a container that restarts (and therefore
gets a new ID) is reattached, and services created after startup appear on their
own. Output is fed through a `vt100` terminal emulator, so ANSI colour, progress
bars that rewrite with `\r`, and Java stack traces all render the way they would
in a real terminal.

## Deliberate differences from the Nx TUI

The keys, layout and colours match Nx. Two behaviours intentionally do not,
both because compose services are long-running where Nx tasks are short:

- **Auto-exit only on a clean shutdown.** Nx auto-exits when a run finishes,
  because finishing is success. Here, "everything exited" usually means the
  stack fell over, so a non-zero exit anywhere keeps the UI open.
- **Scrolling anchors to content, not to a row offset.** Nx preserves the raw
  scroll offset, so a scrolled-up view drifts as new output arrives. With
  unbounded container logs that drags you away from the stack trace you are
  reading, so a scrolled-up pane holds its position instead.

  This holds until the service's buffer fills (`scrollback`, 1000 rows by
  default). Past that, every new row evicts the oldest and there is nowhere left
  to move the anchor, so the view drifts as nx's does. Raise `scrollback` to
  widen the window, at roughly 7 MB per service per 1000 rows.

## Contributing

Contributions are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md) — it
covers setup, the test conventions, and one constraint that is easy to trip
over: composemux deliberately mirrors the Nx TUI's keybindings and layout, so a
change that improves on Nx in isolation may still be declined.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). For
security issues, see [SECURITY.md](SECURITY.md) — please report privately rather
than opening an issue.

## Attribution

This is a port of the Nx terminal UI, which is MIT licensed and copyright
2017-2026 Narwhal Technologies Inc. See [LICENSE-THIRD-PARTY](LICENSE-THIRD-PARTY).
Modules derived from Nx name their upstream file in a header comment.

It is an independent project, not affiliated with or endorsed by Nrwl / Nx.

## License

MIT. See [LICENSE](LICENSE).

Contributions are accepted under the same licence (inbound = outbound).
Contributors keep the copyright in their own work; there is no CLA.
