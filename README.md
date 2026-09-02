# Azzurro

An open-source controller for BluOS players — Bluesound, NAD and PSB — for
Linux, macOS and Windows.

Rust throughout, Slint for the GUI. No webview, no Electron, no Qt, no C++.

**Status: young, and exercised against real hardware.** Discovery, status, the
long poll and the transport verbs all run against a player rather than a
fixture, and firmware upgrades have been driven end to end on an NAD Powernode.

The window lists the players it finds and drives them:

- the selected player's queue, with cover art — play a row, remove one,
  reorder by dragging, save the lot as a playlist; long ones arrive five
  hundred tracks at a time as the list is scrolled
- browse and search every source the player offers, with Home rearrangeable,
  favourites to hand, and recent searches that survive a restart
- saved playlists: keep the queue as one, play it, and delete it
- transport, volume, mute and the physical inputs
- ad-hoc grouping, one player at a time or all of them at once
- the player's own settings, served from a second port and rendered from the
  forms the player sends
- alarms: read, write, schedule, and choose what each one plays
- whatever the service offers for the track — Love, Ban, Shop — where it
  offers any
- presets: the shelf, the screen, playing one, saving a station into one from
  its own menu or from what is playing, deleting it from the player's, renaming
  one, and dragging them into a new order
- radio stations typed in by hand, for the ones no directory carries — kept on
  this machine, since the player has nowhere to put them
- firmware upgrades: an offer on the way in, a confirmation, the install
  itself, and a strip that follows it stage by stage
- a keyboard: space plays and pauses, Escape and Backspace go back, Home and
  End reach the ends of a list, and `/` opens search. Typing a letter jumps a
  long list to it, as does the alphabet down the side — the player indexes its
  four long lists, and a list held whole is indexed from its own rows

On Linux each player is also exported over MPRIS, so the desktop's own media
controls work. That is the one feature that is not on all three: MPRIS is
D-Bus, and the other two have nothing to export to.

Two things are not reimplementable and are out of scope: linking a new music
service, which happens on Lenbrook's cloud control panel, and anything else
behind a BluOS account.

## What is not there yet

Measured against the official controller, in roughly the order a user notices:

| Missing | Where it stands |
| --- | --- |
| **The fifteen-second nudge** | A podcast's skip and back carry an `interval`. Left out of the track actions beside them: no player here offers one, so their shape has never been seen. |
| **Renaming a saved playlist** | Saving, listing, playing and deleting all work — the player serves a BluOS Playlists shelf and a context menu with Delete on it. Renaming is not offered by the player at all, in any menu or in `/Services`, so it is not a matter of building it. |
| **Stereo pairs and surround zones** | Only ad-hoc groups exist. A bonded pair shows as two unrelated players. |
| **Named group configurations** | "Downstairs" as a one-press recall. There is no route for it: `/Zones`, `/Groups`, `/ZonePresets`, `/SavedGroups` and `/Rooms` all answer 404, and neither `/Services` nor the settings service mentions grouping. Only ad-hoc `/AddSlave` and `/RemoveSlave` exist, and both are used. So this would be the app's own file of player sets rather than anything the player keeps — buildable, but not testable on one speaker. |

None of these is waiting on interface work. Two need a second speaker, one
needs a service that offers it, and one is not something the player does.

One thing is built but unproven. A zone member that will not answer about its
own firmware is asked about through the player leading it, using the `&slave=`
parameter — which was read out of the official controller rather than observed,
and has never run against a real zone, there being one speaker here to test on.
It is reached only after a direct request has already failed, so it cannot
change what works today. `upgrade=all`, which would upgrade a whole room on one
press, is refused deliberately and stays that way.

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

Three more files sit beside it, all plain text and all the app's own: the
stations typed in by hand, the searches made, and the order Home's shelves were
dragged into. Nothing else is written anywhere, and none of it is anything the
player could hold.

On Linux every player is also exported on D-Bus as its own MPRIS media player,
named after the speaker rather than after the app — two speakers playing two
different things are two things the desktop should be able to see and drive.
Media keys, the GNOME shell menu, the KDE applet and a lock screen all reach a
player through the same command channel the window's own buttons use.

Browsing is server-driven. `/ui/Configuration` lists the screens a player
offers, each one arrives as XML describing rows and items, and every `browse`
action names the next document to fetch. The vocabulary is closed — about
twenty-five elements and nine action types — so `crates/bluos/src/screen.rs`
is a `match` rather than a general document renderer, and the app needs no
knowledge of any particular music service.

One obligation comes with that. A screen can offer a filter or a sort — Radio
Paradise's MQA against CD Quality — and choosing one is **not** state the
player keeps. It answers with an `X-Sovi-Ui-Context` header and expects it back
on every request afterwards; without that, the action succeeds, the screen
refreshes, and nothing changes.

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

Releases carry a Flatpak bundle for `x86_64` and `aarch64`, a universal `.app`
for macOS, and one self-contained `.exe` for Windows. The macOS bundle is signed
with a Developer ID and notarised by Apple, so it opens without being allowed
past Gatekeeper by hand; the Windows executable is not signed, so SmartScreen
warns on first run and then lets you through. Installing it through winget
avoids that warning entirely.

```bash
brew install --cask jzbz/azzurro/azzurro           # macOS, from the tap
flatpak install ./azzurro-*.flatpak                # Linux, from the bundle
winget install Azzurro.Azzurro                     # Windows, once the manifest lands
```

The winget manifest is submitted rather than merged — until a moderator takes
it, the Windows route is the `.exe` from the release.

Every release ships a `SHA256SUMS` signed with the maintainer's PGP key. It is
the one artefact that survives a compromise of GitHub, winget or the Homebrew
tap, so it is worth checking before trusting any of them:

```bash
gpg --verify SHA256SUMS.asc SHA256SUMS && sha256sum -c SHA256SUMS
```

Linux is where this is developed and used daily. The other two have each had
their artifact run on a real machine — the window drawn, a player found on the
network and its playback shown — rather than only compiled. That is worth doing
rather than trusting: the Windows build passed every test on every push while
being unable to start on any machine that was not the one that built it.

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

`packaging/macos-bundle.sh` lays out `Azzurro.app` around a universal binary;
nothing in it needs Xcode. On Windows `cargo build --release -p azzurro-gui`
is the whole of it — the binary sets `windows_subsystem = "windows"` in release
so it does not open a console behind the window, and `.cargo/config.toml` links
the Visual C++ runtime into it. Without that the exe exits on any machine
without the redistributable, which is most of them: `0xC0000135`, no window and
nothing in the event log.

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
