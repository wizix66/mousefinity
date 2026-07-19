# rdev fork: macOS drag events

`rdev-macos-drag-events.patch` is the change mousefinity needs in
[rdev](https://github.com/Narsil/rdev) so that click-and-drag works when a mac
is the machine doing the controlling.

## What it fixes

macOS stops sending `MouseMoved` the moment a mouse button goes down and sends
`LeftMouseDragged` (or the right/other variants) instead. rdev's event tap
subscribes to those, but `convert()` has no arm for them, so they fall through
to `_ => None`. In `grab`, an event that fails to convert never reaches the
callback *and* is returned unmodified — so during a drag mousefinity sees no
motion to forward, and cannot suppress the motion either, which leaks the drag
onto the local screen.

Measured on macOS 26.5 with rdev 0.5.3, synthesising a drag and counting the
motion events a `grab` callback receives:

| | plain motion | button held |
| --- | --- | --- |
| rdev 0.5.3 | 10 | **0** |
| with this patch | 10 | **10** |

Upstream rdev's last release was 0.5.3 in 2023. The one published fork,
`openloaf-rdev`, has the same gap.

## Applying it

```sh
git clone https://github.com/Narsil/rdev && cd rdev
git checkout -b macos-drag-events v0.5.3
git apply /path/to/rdev-macos-drag-events.patch
git commit -am "macOS: report motion while a mouse button is held"
git push <your-fork> macos-drag-events
```

Then point the workspace at it, in the root `Cargo.toml`:

```toml
[patch.crates-io]
rdev = { git = "https://github.com/<you>/rdev", branch = "macos-drag-events" }
```

`[patch]` keeps the dependency named `rdev` at its usual version, so nothing
else in the tree changes. Re-run `cargo test --workspace` after applying; the
suite covers the delta arithmetic that drag motion feeds into, not the tap
itself, so the useful check is a real drag between two hosts.
