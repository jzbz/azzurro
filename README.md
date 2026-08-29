# Azzurro

An open-source Linux controller for BluOS players — Bluesound, NAD and PSB.

Rust throughout, Slint for the GUI. No webview, no Electron, no Qt, no C++.

**Status: young, and exercised against real hardware.** Discovery, status, the
long poll and the transport verbs all run against a player rather than a
fixture, and firmware upgrades have been driven end to end on an NAD Powernode.

The window lists the players it finds and drives them:

- the selected player's queue, with cover art — play a row, remove one,
  reorder by dragging, save the lot as a playlist
- browse and search every source the player offers, with Home rearrangeable,
  favourites to hand, and recent searches that survive a restart
- transport, volume, mute and the physical inputs
- ad-hoc grouping, one player at a time or all of them at once
- the player's own settings, served from a second port and rendered from the
  forms the player sends
- alarms: read, write, schedule, and choose what each one plays
- firmware upgrades: an offer on the way in, a confirmation, the install
  itself, and a strip that follows it stage by stage

Each player is also exported over MPRIS, so the desktop's own media controls
work.

Two things are not reimplementable and are out of scope: linking a new music
service, which happens on Lenbrook's cloud control panel, and anything else
behind a BluOS account.

## What is not there yet

Measured against the official controller, in roughly the order a user notices:

| Missing | Where it stands |
| --- | --- |
| **Presets** | The shelf is hidden and the save route is a stub. `bluosctl preset <player> <n>` recalls a slot; nothing enumerates the slots or stores into one. |
| **Per-service track actions** | `/Status`'s `<actions>` are parsed in full — thumbs up and down, the shop link, the fifteen-second nudge — and nothing fires them. |
| **Saved playlists** | The queue can be saved and a track filed into a playlist. Listing, renaming, deleting and reordering are absent. |
| **Stereo pairs and surround zones** | Only ad-hoc groups exist. A bonded pair shows as two unrelated players. |
| **Named group configurations** | "Downstairs" as a one-press recall. No route for it is documented. |
| **Queues past 500 tracks** | The queue is fetched as one window of 500. The header says `500 of 1234`, so nothing is hidden silently, but the tail cannot be reached. |
| **Custom radio by stream URL** | Radio is whatever the player's browse tree offers. No endpoint for pasting a URL is documented. |
| **Upgrading through a master** | The upgrade route takes `slave=` to reach a zone member. Not sent, so such a player cannot be upgraded from here. `upgrade=all` is refused deliberately. |

Most of these are UI work over a protocol layer that already reaches the
endpoint, rather than protocol work waiting to be done.

## Layout

| Crate | Contents |
| --- | --- |
| `crates/bluos` | The protocol: LSDP discovery, the control API, the long poll, the status documents. No GUI types, no UI dependencies. Meant to be published on its own. |
| `crates/bluos-cli` | `bluosctl`, the same crate with a command line on it. The fast way to check something against real hardware. |
| `crates/azzurro-gui` | Slint front end. Binary is `azzurro`. |
| `crates/fake-player` | A test double that answers like a player, so the protocol and the window can be driven with no speaker on the network. Not published. |

The GUI depends only on `bluos`'s own types — no XML parser or HTTP client
reaches a Slint callback — so the protocol layer stays usable by anything else.

## Trying it

```bash
cargo run --bin bluosctl -- discover
```

```bash
cargo run --bin bluosctl -- status 10.0.0.155
```

`bluosctl watch <player>` prints a block every time a player changes and is the
quickest way to see the long poll working. `cargo run --bin azzurro` opens the
window.

Players are addressed as `host` or `host:port`; a bare address means the
standard control port, 11000.

## How it fits together

One tokio runtime, one long-poll task per player, and a channel of commands
going the other way. A player's `/Status` is requested with a timeout and the
last etag, and the player holds the connection open until something changes —
so the UI is driven by the players rather than by a timer, and nothing is
missed between polls.

Discovery is a UDP broadcast on port 11430 that every player answers. The same
socket keeps listening afterwards, because players announce themselves when they
wake, so one switched on an hour later still appears without a rescan.

Neither of those helps a player that was asleep at startup or sits behind
something that eats broadcast traffic, so addresses that have answered are
remembered in `~/.config/azzurro/players` and tried again next time. One can be
pinned there by hand, or typed into the box under the player list.

Every player is also exported on D-Bus as its own MPRIS media player, named
after the speaker rather than after the app — two speakers playing two
different things are two things the desktop should be able to see and drive.
Media keys, the GNOME shell menu, the KDE applet and a lock screen all reach a
player through the same command channel the window's own buttons use.

Browsing is server-driven. `/ui/Configuration` lists the screens a player
offers, each one arrives as XML describing rows and items, and every `browse`
action names the next document to fetch. The vocabulary is closed — about
twenty-five elements and nine action types — so `crates/bluos/src/screen.rs`
is a `match` rather than a general document renderer, and the app needs no
knowledge of any particular music service.

The interface draws its own controls rather than using the platform's — a
music controller is one of the few app classes where looking like itself is
the expectation. `ui/theme.slint` holds the palette, spacing and type in one
place; `ui/widgets.slint` holds the pieces built from them. Icons are
[Lucide](https://lucide.dev), recoloured through their alpha channel so one
copy of each glyph serves every colour the theme has.

The now-playing panel takes a colour from the cover art and washes it behind
everything at low opacity, so a red sleeve and a green one feel different from
across the room. Greys and near-blacks are discarded before averaging, because
the mean of a whole sleeve is always a muddy brown; a cover with no colour in
it gets no tint rather than a dirty one.

Cover art is the one place Slint costs more than a toolkit with a URL-loading
image element: fetching, decoding, scaling and caching are all the app's job.
That work is confined to `crates/azzurro-gui/src/artwork.rs` — an LRU of
decoded pixels in front of a cache of fetched bytes on disk, deduplicated by
URL and bounded to four fetches at once.

`docs/protocol.md` is the reference for all of it, including what has been
confirmed against hardware and what has only been read out of the official
client.

## Installing

There is no published release yet. Linux is the target and the only platform
this is used on; CI builds and tests on macOS and Windows as well, so the code
stays honest about what is platform-specific, but neither is packaged.

To put it in your desktop's menu from a checkout:

```bash
cargo build --release -p azzurro-gui
install -Dm755 target/release/azzurro ~/.local/bin/azzurro
install -Dm644 crates/azzurro-gui/desktop/blue.azzurro.Azzurro.desktop \
    ~/.local/share/applications/blue.azzurro.Azzurro.desktop
install -Dm644 crates/azzurro-gui/desktop/blue.azzurro.Azzurro-256.png \
    ~/.local/share/icons/hicolor/256x256/apps/blue.azzurro.Azzurro.png
```

If `build.target-dir` is set in your Cargo config the binary is under that
directory instead; `cargo build --release -p azzurro-gui --message-format=json`
reports where it actually went.

`packaging/blue.azzurro.Azzurro.yml` is a Flatpak manifest for the same
thing. It needs `packaging/cargo-sources.json`, which is generated from
`Cargo.lock` because Flathub builds offline:

```bash
python3 packaging/cargo-sources.py > packaging/cargo-sources.json
```

The sandbox takes no filesystem permissions. It does need `--share=network`,
and not only for HTTP: discovery is a UDP broadcast, which needs the host's
network namespace rather than a proxied socket.

## Licence

MIT. See `LICENSE`.

Not affiliated with, endorsed by, or supported by Lenbrook Industries. BluOS,
Bluesound and NAD are their trademarks.
