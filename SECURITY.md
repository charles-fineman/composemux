# Security Policy

## Supported versions

composemux is pre-1.0. Fixes land on the latest release; there are no
maintained back-branches yet.

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Use GitHub's private vulnerability reporting instead: go to the
[Security tab](https://github.com/sofired/composemux/security) and choose
*Report a vulnerability*. That opens a private channel with the maintainers and
avoids disclosing the issue before there is a fix.

Please include what you were running (`composemux --version`, `docker version`,
OS), what happened, and how to reproduce it.

## Scope

composemux is a read-only log viewer. It connects to the Docker daemon using
your existing credentials and configuration, renders container output to your
terminal, and never mutates container state.

Things worth reporting:

- Container output being interpreted in a way that escapes the log pane —
  writing outside its bounds, corrupting the surrounding UI, or driving the host
  terminal through emitted escape sequences. Container logs are untrusted input;
  they are parsed by a `vt100` emulator specifically so they cannot be passed
  through verbatim.
- The terminal being left in a broken state (raw mode, alternate screen) after
  exit, which is a real problem when a wrapper script keeps running afterwards.
- Anything that causes credentials, environment, or Docker socket access to be
  exposed or misused.

Out of scope: the security of the containers you point it at, and the security
of the Docker daemon itself.
