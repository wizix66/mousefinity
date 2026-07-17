# mousefinity

Share one mouse and keyboard across multiple computers — plus clipboard sync
and file transfer — over secure peer-to-peer connections that work across the
internet, not just your LAN.

Move the cursor off the edge of one screen and it appears on the next machine,
exactly like a multi-monitor setup, except each "monitor" is a different
computer. You decide how the screens are arranged.

## Highlights

- **Standalone** — one static binary, no accounts, no central server you must
  run. (Connections bootstrap through public [iroh](https://iroh.computer)
  relays and fall back to them when hole-punching fails; you can self-host
  those too.)
- **Fast & memory-safe** — written entirely in Rust.
- **Works across the internet** — connectivity via QUIC with NAT
  hole-punching. Peers find each other by public key through relay + DNS
  discovery; when a direct path exists it is used, otherwise traffic falls
  back to an encrypted relay. No port forwarding needed.
- **Secure by construction** — every connection is end-to-end encrypted
  (TLS 1.3 over QUIC). A peer *is* its Ed25519 public key: pairing means
  exchanging ids once, and anything not on your peer list is refused at the
  handshake.
- **Clipboard sync** — the clipboard follows you as you hop between screens
  (text, up to 4 MiB).
- **File transfer** — `mousefinity send laptop big.iso` streams a file
  directly to a paired peer's downloads folder.
- **Seamless hopping** — configurable screen arrangement (left/right/up/down
  of each other), proportional cursor entry between screens of different
  resolutions, chained hops across 3+ machines, and a panic key
  (ScrollLock) that always yanks control back to the machine with the
  physical keyboard.

## Install

Grab a prebuilt binary from the
[releases page](https://github.com/wizix66/mousefinity/releases)
(Windows, Linux, macOS Intel + Apple Silicon), or build from source:

```sh
cargo build --release          # produces target/release/mousefinity(.exe)
```

Windows note: on the GNU toolchain, build with dependency optimization (the
default dev profile here already does) — see `Cargo.toml`.

## Setup (two machines: `desktop` and `laptop`)

On each machine:

```sh
mousefinity init               # creates identity + config, prints pairing id
```

Exchange the printed pairing ids (they are public keys — safe to share):

```sh
# on desktop
mousefinity add-peer laptop  <laptop's id>
# on laptop
mousefinity add-peer desktop <desktop's id>
```

Arrange the screens **on either machine** (the layout syncs to every
connected peer automatically — newest edit wins):

```sh
mousefinity link desktop right laptop     # laptop sits to the right
```

Prefer something friendlier? `mousefinity tui` opens an interactive
configuration UI: add/remove peers (Ctrl-V pastes a pairing id), copy your
own id, and set each screen's neighbours with the arrow keys. Saving pokes a
running daemon so changes apply — and sync — immediately, no restart needed
for layout edits.

Then start the daemon on both:

```sh
mousefinity run
```

Push the cursor off desktop's right edge — it lands on the laptop. Keyboard,
mouse buttons and scrolling follow the cursor. Copy text on one machine,
paste on the other. ScrollLock instantly returns control home.

Send a file to whichever peer you like (daemon must be running):

```sh
mousefinity send laptop path/to/file.pdf   # lands in ~/Downloads/mousefinity
```

## Configuration

`~/.config/mousefinity/config.toml` (Linux/macOS) or
`%APPDATA%\mousefinity\config.toml` (Windows). Everything the CLI does you
can also edit by hand:

```toml
name = "desktop"
# screen = [2560, 1440]        # override auto-detected size if needed
# downloads = "D:/incoming"    # where received files land

[peers.laptop]
id = "3fa9…"                   # from `mousefinity id` on that machine

[layout.desktop]
right = "laptop"

[layout.laptop]
left = "desktop"
```

The layout is a graph, not a grid — chain as many machines as you like
(`desktop → laptop → mac`), including vertical stacking with `up`/`down`.
Hops work between any two machines that are direct neighbours in the layout;
each machine only needs a peering entry for machines it talks to directly.

**Layout syncs itself.** Every edit (via `link` or the TUI) stamps a
revision; peers exchange layouts when they connect and gossip newer
revisions onward, so you only ever edit the arrangement on one machine.
Trust does *not* sync, by design: each machine decides for itself which
public keys it accepts, so `add-peer` stays a per-machine step.

The identity key lives next to the config (`secret.key`). Protect it like an
SSH private key; the pairing id printed by `init`/`id` is the public half.

## Platform support

| Platform | Control others | Be controlled | Clipboard | Files | Notes |
| -------- | -------------- | ------------- | --------- | ----- | ----- |
| Windows  | ✅ | ✅ | ✅ | ✅ | DPI-aware; low-level hooks |
| macOS    | ✅ | ✅ | ✅ | ✅ | grant Accessibility + Input Monitoring to the binary |
| Linux X11 | ✅ | ✅ | ✅ | ✅ | capture uses evdev: add your user to the `input` group |
| Linux Wayland | ⚠️ | ⚠️ | ✅ | ✅ | injection depends on compositor support; capture via evdev |
| Android  | 🚧 | 🚧 | 🚧 | 🚧 | core crates build for `aarch64-linux-android`; needs an AccessibilityService app shell (planned) |
| iOS      | 🚧 | ❌ | 🚧 | 🚧 | Apple provides no API for system-wide input injection; an iOS node can only ever be a controller/clipboard/file peer, not a controlled screen |

The protocol and networking crates (`mousefinity-proto`, and the transport in
the main crate) are pure Rust with no desktop dependencies and compile for
both mobile targets today; what's missing is the platform shell (Kotlin
AccessibilityService / Swift app) that hosts them. This is the documented
path, not a hidden limitation: **no** third-party tool can inject
system-wide input on stock iOS.

## How it works

- **Transport** — [iroh](https://github.com/n0-computer/iroh) QUIC endpoints.
  Each host publishes its (relay, address) info under its public key;
  connections authenticate both ends by key during the TLS handshake and
  hole-punch a direct UDP path, falling back to the encrypted relay.
- **Capture** — a low-level hook (`rdev::grab`) sees every input event. While
  the virtual cursor is on a remote screen all events are swallowed locally
  and forwarded; the physical cursor is parked at the screen centre so each
  hook event yields a clean relative delta.
- **Focus model** — the machine with the physical input is authoritative: it
  tracks the virtual cursor across every screen in the layout (it learns each
  peer's resolution at handshake), decides edge hops, and streams absolute
  positions to whichever peer is focused. Controlled peers never make hop
  decisions, which prevents feedback loops from injected events.
- **Clipboard** — pushed to the peer you hop to; handed back when you leave.
- **Files** — a separate QUIC connection per transfer (`ALPN mousefinity/file/1`),
  streamed with backpressure; receivers only ever write inside their
  downloads directory (path components are stripped).
- **`send` CLI** — talks to the running daemon over token-authenticated
  loopback IPC, so transfers reuse the daemon's identity and connections.

## Security model

- Identity = Ed25519 keypair, generated at `init`, never leaves the machine.
- Peering is explicit and mutual: each side must `add-peer` the other's
  public key. Unknown keys are rejected before any application data flows.
- All traffic (input, clipboard, files) rides TLS-1.3-encrypted QUIC;
  relays only ever see ciphertext.
- Received files: only the file *name* is honoured, never a path; name
  collisions get ` (n)` suffixes; files land only in the configured
  downloads directory.
- Emergency release: ScrollLock returns control to the local machine even if
  the focused peer hangs or vanishes (peer loss also auto-releases).

## Limitations (v0.1)

- Layout edits sync between connected daemons; a machine that was offline
  catches up when it reconnects. If two people edit layouts simultaneously
  on different machines, the newest timestamp wins.
- Clipboard is text-only; images/rich content planned.
- Key forwarding assumes a US-QWERTY *sender* for printable characters
  (named keys — arrows, modifiers, function keys — are layout-independent).
- One screen per host is modelled: with multi-monitor hosts, set `screen` to
  your primary monitor and prefer hop edges not covered by a second monitor.
- Modifier state is not re-synchronized across a hop; release modifiers
  before hopping.
- Android/iOS shells are not implemented yet (see the support matrix).

## License

MIT or Apache-2.0, at your option.
