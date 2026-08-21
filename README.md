# Azzurro

An open-source Linux controller for BluOS players — Bluesound, NAD and PSB.

Rust throughout, Slint for the GUI. No webview, no Electron, no Qt, no C++.

**Status: a working skeleton, and young.** Discovery, status, the long poll and
the transport verbs are implemented and exercised against real hardware; the
window lists the players it finds, shows the selected one's play queue with
cover art, browses every source the player offers, and drives all of it. Each
player is exported over MPRIS so the desktop's own media controls work. See the
roadmap for what is still missing.

## Layout

| Crate | Contents |
| --- | --- |
| `crates/bluos` | The protocol: LSDP discovery, the control API, the long poll, the status documents. No GUI types, no UI dependencies. Meant to be published on its own. |
| `crates/bluos-cli` | `bluosctl`, the same crate with a command line on it. The fast way to check something against real hardware. |
| `crates/azzurro-gui` | Slint front end. Binary is `azzurro`. |

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

Cover art is the one place Slint costs more than a toolkit with a URL-loading
image element: fetching, decoding, scaling and caching are all the app's job.
That work is confined to `crates/azzurro-gui/src/artwork.rs` — an LRU of
decoded pixels in front of a cache of fetched bytes on disk, deduplicated by
URL and bounded to four fetches at once.

`docs/protocol.md` is the reference for all of it, including what has been
confirmed against hardware and what has only been read out of the official
client.

## Roadmap

**v0.1 — a daily driver.** Discovery and manual addressing. Per-player volume
and mute. Now playing with artwork, transport, seek, shuffle and repeat.
Presets. The play queue, cover art and MPRIS are done; seek and presets are not.

**v0.2 — browsing.** The engine is in: the player describes each screen as XML,
`/ui/BrowseObjects` reaches every service through one code path, and a service
this app has never heard of browses and plays without a line of code about it.
Still to come on top of it: search, favourites, and the per-item context menus
(add to queue, favourite, go to artist) — which are also the only route to
editing the queue, since removing and reordering exist solely as the device's
own menus. Then grouping and saving a queue as a playlist.

**v1.0.** Alarms, sleep timer, inputs. Tray icon and notifications. Flatpak.

Out of scope, because they are not reimplementable: linking a new music service,
which happens on Lenbrook's cloud control panel, and anything else behind a
BluOS account.

## Licence

MIT. See `LICENSE`.

Not affiliated with, endorsed by, or supported by Lenbrook Industries. BluOS,
Bluesound and NAD are their trademarks.
