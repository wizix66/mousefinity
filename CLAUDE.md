# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`mousefinity` shares one mouse/keyboard/clipboard across machines over P2P QUIC
(iroh). Every host runs the same daemon — there is no server role. Cargo
workspace, Rust 2021, two crates.

## Commands

```sh
cargo build --release --workspace      # what CI builds
cargo test --workspace                 # all tests (unit tests only, no integration dir)
cargo test -p mousefinity engine::     # one module's tests
cargo test -p mousefinity-proto edge_crossing   # one test by name
cargo clippy --workspace --all-targets # 3 pre-existing warnings; keep it at 3
cargo build -p mousefinity-proto --target aarch64-linux-android   # mobile-core guarantee
```

Requires rustc **1.91+** (iroh's MSRV). Linux build deps:
`libx11-dev libxtst-dev libxi-dev libevdev-dev libxdo-dev`.

The repo is not rustfmt-clean and never has been — run `rustfmt` on individual
new files rather than `cargo fmt` across the tree, which would bury a change in
unrelated reformatting.

`tshark` is useful for confirming whether a path is direct or relayed, but the
QUIC payload is end-to-end encrypted, so it shows path shape only — `doctor`
and `report` are the real protocol-debugging tools.

### Checking the other targets from a Mac

```sh
# Windows: compiles the cfg(windows) code. Needs `cargo install cargo-xwin`
# and `brew install llvm` (ring's build script shells out to llvm-lib).
PATH="/opt/homebrew/opt/llvm/bin:$PATH" cargo xwin build --target x86_64-pc-windows-msvc -p mousefinity

# arm64 Linux (the Ampere/Graviton target): native under podman on Apple
# Silicon, so the tests actually run rather than just building.
podman run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/tmp/target docker.io/library/rust:1 \
  bash -c "apt-get update -qq && apt-get install -y -qq libx11-dev libxtst-dev libxi-dev libevdev-dev libxdo-dev && cargo test --workspace"

cargo build -p mousefinity-proto --target aarch64-linux-android   # no NDK needed: rlib, nothing links
```

Nothing here *runs* a Windows binary. Wine's cask wants an interactive sudo
password and a Windows VM wants an ISO and a licence, so Windows-specific
runtime behaviour is still unverified — prefer writing platform logic so it is
selected by data (archive magic bytes, `target.exists()`) rather than by
`cfg(windows)`, which makes it testable everywhere.

Running the daemon needs OS permissions (macOS: Accessibility + Input
Monitoring; Linux: user in `input` group), so `cargo run -- run` will log
"input capture unavailable" and park as a controlled-only host without them.

**`MOUSEFINITY_CONFIG_DIR`** overrides the config directory — use it to run
several instances on one machine, or to keep tests off the real config.

`mousefinity doctor` is the first thing to reach for on any connectivity
question; it reports what the network blocks, relay health, and per-peer
direct-vs-relayed reachability.

## Architecture

`crates/proto` — wire protocol + layout model. **Deliberately free of any
platform input/GUI dependency** so it builds for Android/iOS; CI enforces this
with the `mobile-core` job. Don't add desktop deps here. `Msg` is the control
enum, framed as `u32-le length ++ postcard`. `PROTO_VERSION` must be bumped on
incompatible changes (peers refuse mismatches at `Hello`).

`crates/mousefinity` — the daemon and CLI. Thread topology set up in
`cmd_run` ([main.rs:310](crates/mousefinity/src/main.rs:310)):

- **capture** ([capture.rs](crates/mousefinity/src/capture.rs)) — rdev grab
  hook. Must own the main thread (hard macOS requirement). Communicates focus
  state to the engine only through `CaptureShared` atomics.
- **engine** ([engine.rs](crates/mousefinity/src/engine.rs)) — own thread,
  blocking receive over `EngineIn`. The focus state machine: virtual cursor
  position, hop decisions, clipboard handoff, layout adoption.
- **net** ([net.rs](crates/mousefinity/src/net.rs)) — tokio runtime on its own
  thread; iroh endpoint, dialing, accept loop, file transfer, mesh join.
- **inject** ([inject.rs](crates/mousefinity/src/inject.rs)) — own thread that
  owns the single `enigo` handle; everyone else sends `InjectCmd`.
- **ipc** ([ipc.rs](crates/mousefinity/src/ipc.rs)) — loopback TCP + random
  token in `ipc.json`, so `send`/`link`/`tui` reuse the daemon's identity and
  live connections rather than opening their own endpoint.

Out-of-band commands: [doctor.rs](crates/mousefinity/src/doctor.rs) probes the
network into a `Report` sink (echoed live by `doctor`, captured by `report`);
[diag.rs](crates/mousefinity/src/diag.rs) wraps that into a bug-report bundle;
[upgrade.rs](crates/mousefinity/src/upgrade.rs) self-updates from GitHub
releases.

### Invariants worth knowing before changing behavior

- **The host with the physical mouse is authoritative.** It tracks the virtual
  cursor across every screen and streams absolute positions; controlled peers
  never make hop decisions. Breaking this reintroduces feedback loops from
  injected events.
- **While remote, motion is the difference between consecutive hook events**,
  never the offset from screen centre — suppressing an event does not pin the
  pointer, so a fixed reference re-reports accumulated drift and the remote
  cursor accelerates away.
- **Which event was our own warp is not decided from coordinates.** A warp
  landing slightly off is indistinguishable from a fast flick; two attempts at
  telling them apart both leaked the teleport distance to the far screen as a
  jump. Instead any step above `MAX_STEP` is discarded, and `RECENTRE_AT` is
  kept a smaller divisor so a warp is always longer than the largest allowed
  step — `a_warp_is_always_longer_than_the_largest_allowed_step` in
  [capture.rs](crates/mousefinity/src/capture.rs) pins that relationship.
  Preserve it if you touch either constant. The inject thread reports where it
  put the pointer, which keeps discards near zero, but correctness does not
  depend on that arriving.
- **Trust is per-machine and never gossiped by manual pairing.** A peer *is*
  its iroh `EndpointId`; anything not in `peers` is refused at accept. Only
  hosts sharing a mesh token accept `Roster` gossip.
- **Layout syncs, trust doesn't.** `layout_rev` is unix-ms of the last local
  edit; strictly-newer revisions are adopted, persisted, and re-gossiped.
  Any edit path must bump `layout_rev` via `config::now_ms()`.
- **Names are local; ids are the identity on the wire.** Config, engine and
  TUI are name-keyed; `Msg::Layout` is id-keyed. `layout_to_wire` /
  `layout_from_wire` in [net.rs](crates/mousefinity/src/net.rs) translate at
  the control link's read/write loops, and `Hello.name` is deliberately
  ignored in favour of a `names_by_id` lookup. Anything that puts a local
  name on the wire reintroduces phantom screens when two hosts use different
  aliases for one machine.
- **Peer links are epoch-tagged** so a stale link's teardown can't evict its
  successor — preserve the epoch check when touching `PeerUp`/`PeerDown` or
  `control_txs`.
- **Config read-modify-write goes through `config::FILE_LOCK`** (engine thread
  and net runtime both write it).
- **ScrollLock is the panic key** and is intercepted in capture, never
  forwarded, so it works even when the focused peer is wedged.
- Mesh membership is proven by a blake3 keyed hash bound to *both* endpoint
  ids of the connection ([mesh.rs](crates/mousefinity/src/mesh.rs)) — the
  secret never crosses the wire and proofs can't be replayed to another pair.

- **Nothing leaves the machine unasked.** There is no telemetry; `report`
  writes a local file and says so. The bundle redacts `mesh_secret` and never
  reads `secret.key` — [diag.rs](crates/mousefinity/src/diag.rs) has a test
  pinning that, written so it cannot pass vacuously if the field is renamed.
- **`upgrade` verifies before it installs.** It hashes the download against
  `SHA256SUMS` (published by `release.yml`) or GitHub's asset digest, and
  refuses if neither exists. `install_at` stages then renames, so a failed
  upgrade leaves the working binary intact. Archive format is chosen by magic
  bytes, not `cfg(windows)`, so the zip path is testable off Windows — keep it
  that way.
- **Clipboard construction is serialized process-wide**
  ([clipboard.rs](crates/mousefinity/src/clipboard.rs)). Production only ever
  has one `Clip`, but macOS aborts the process if several threads touch
  NSPasteboard at once, which parallel tests do.

### The rdev patch

`[patch.crates-io]` points rdev at a fork
([contrib/](contrib/README.md) has the patch and the measurements). macOS sends
`LeftMouseDragged` instead of `MouseMoved` while a button is held, and stock
rdev converts neither, so a grab callback sees nothing for the duration of a
drag and cannot suppress it either — click-and-drag from a mac did nothing
remotely and leaked onto the local screen.

Consequences worth knowing: the build needs to reach github.com for that git
dependency, not just crates.io, and `cargo install mousefinity` from a registry
would ignore the patch (`[patch]` does not survive publishing) and silently
lose drag support. Distribution is via release binaries, so this is fine today.
The fork branches from upstream's `0e2a1c8`, the commit 0.5.3 was published
from; rebase onto that if it ever needs regenerating.

### Cargo profile note

`[profile.dev.package."*"] opt-level = 2` is load-bearing, not just for speed:
unoptimized, the iroh-relay cdylib's export table overflows the windows-gnu
linker's 64k limit.

## Conventions

Module-level `//!` docs explain the *why* of each file; comments in this
codebase justify non-obvious decisions rather than restate code. Follow that.
User-facing CLI output is lowercase and conversational, and errors suggest the
next command to run.

Release is tag-driven (`v*`): builds five targets plus a packaged `iroh-relay`
server binary for self-hosting, a `SHA256SUMS` that `upgrade` verifies against,
and a CycloneDX SBOM per crate.

**Never put the CI-skip token in a commit you intend to tag.** It suppresses
every workflow triggered by a push referencing that commit, and a tag push is
a push — so the release silently does not build. There is no failed run to
notice, just a tag with no release behind it.

The token is matched as a literal substring anywhere in the message, including
in prose *about* it — a commit explaining this trap, with the token quoted in
its own message, suppressed the release a second time. Write it as "the
CI-skip token" in commit messages; spelling it out is only safe in files.

```sh
cargo install cargo-cyclonedx
cargo cyclonedx --format json --spec-version 1.5 --all-features  # writes <crate>.cdx.json beside each manifest
```

Dependabot (`.github/dependabot.yml`) groups iroh patch bumps into one PR and
ignores iroh majors — those change `PROTO_VERSION` compatibility, so both ends
of every pair must upgrade together and a human should choose when.
