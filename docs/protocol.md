# The BluOS protocol, as far as it has been worked out

BluOS publishes no protocol specification. Everything here was established one
of three ways, and each claim below says which:

* **Observed** — a request this project made to a real player, whose response
  was read. The player was an NAD Powernode N330 on BluOS 4.16.6. Addresses and
  MACs in the examples have been replaced with documentation values.
* **Declared** — the player itself describes the request, in the XML it serves
  from `/Services` or `/ui/*`. As good as observed, because the device is the
  one making the claim.
* **Transcribed** — read out of the official controller's own code and not
  exercised here, because exercising it would have meant changing the state of
  somebody's stereo. Treat as likely but unconfirmed.

Nothing in this document came from Lenbrook, and none of their code is
reproduced in this repository.

## Discovery — LSDP

**Observed.** UDP port 11430, IPv4 broadcast. The official controller also
browses mDNS, which this project does not yet do.

A packet is a six-byte header followed by one or more self-delimiting messages:

```text
byte 0      header length, and the offset the first message starts at (6)
bytes 1-4   the ASCII magic word "LSDP"
byte 5      protocol version (1)
```

Each message begins with its own total length and one ASCII type byte, so an
unknown type is always skippable. Type `A`, announce, is the only one carrying
anything useful:

```text
u8   length     (of the whole message, including this byte)
u8   type       ('A')
u8   node-id length, then that many bytes    — a MAC address, in practice
u8   address length, then that many bytes    — four bytes of IPv4, in practice
u8   record count, then that many records:
       u8 class major, u8 class minor
       u8 TXT count, then that many pairs of
          u8 key length,   key bytes   (UTF-8)
          u8 value length, value bytes (UTF-8)
```

### The query

Sending these eleven bytes to each interface's broadcast address makes every
player answer at once:

```text
06 4C 53 44 50 01   header
05 51 01 FF FF      one message, type 'Q', one class filter, FF:FF wildcard
```

The official controller repeats it at 0, 1, 2, 3, 5, 7 and 10 seconds with up to
250 ms of jitter — front-loaded, then thinning out. Players also announce
unprompted when they come up, so a controller that keeps listening does not have
to sweep again.

A controller hears its own broadcast come back on the socket it sent from. That
is normal; a `Q` packet simply carries no announcements.

### Classes

`(major, minor)`, major 0 on everything seen. The official controller treats
minor 1, 3, 6 and 8 as controllable players and ignores the rest — the class
check is what separates the control API from the other services a player
advertises on the same address:

| Class | Port | What it is |
| --- | --- | --- |
| 0:1 | 11000 | The control API described below |
| 0:4 | 11431 | Something else; does not answer HTTP |

### TXT keys

Observed on a player's 0:1 record: `name`, `port`, `model`, `version`, `zs`.
`port` is authoritative; where it is absent the official controller assumes
11000.

## Control — HTTP on port 11000

**Observed.** Plain HTTP, no TLS, XML responses, and **no authentication of any
kind**. Anything that can route to the player can drive it. A player's own idea
of its identity is `host:port`, which is what `/SyncStatus` reports as `id`.

### Reading state

| Endpoint | Status | Returns |
| --- | --- | --- |
| `/SyncStatus` | Observed | The player: name, model, brand, firmware, volume, MAC, zone options, grouping |
| `/Status` | Observed | What it is doing: transport state, volume, titles, artwork, service, queue position |
| `/Volume` | Observed | Current volume, with `db`, `mute` and an etag of its own |
| `/Presets` | Observed | Stored presets |
| `/Playlist` | Observed | The play queue — see below. Note the singular |
| `/Playlists` | Observed | Saved playlists. Note the plural |
| `/Alarms` | Observed | Alarms |
| `/Services` | Observed | The browse grammar — see below |
| `/RadioBrowse` | Observed | TuneIn's menu, proxied through the player |

`/Songs`, `/Albums`, `/Artists` and `/Genres` answered 302 without parameters;
they are service-scoped and want at least `service=`.

### The long poll

**Observed, and timed.** `/Status` accepts two parameters:

```
GET /Status?timeout=100&etag=8a52c91ed3074a395d457626b80b20c2
```

The player holds the connection open until something changes, or until
`timeout` seconds have elapsed — measured at 5.016 s for a 5 s poll, so the
deadline is honoured precisely — and then answers with the current document
either way. Every reply carries a fresh `etag` to hand back on the next call.

This is the whole reactive core of a controller: one long poll per player, no
polling loop, and no missed transitions. Set the HTTP timeout above the poll
timeout so the player's deadline is always the one that fires.

`/SyncStatus`'s etag also appears inside `/Status` as `syncStat`, so a change in
grouping is visible from a status poll without a second request.

### What the current source can do

**Observed, and the rule transcribed.** `/Status` carries an `<actions>` list:

```xml
<actions><action name="back" state="0"></action></actions>
```

Beyond `skip` and `back` it covers the per-service extras — `love` and `ban` for
thumbs up and down, `shop`, and skip/back variants carrying an `interval` for
the fifteen-second nudges a podcast wants. `state` is the *toggle* state of
`love` and `ban`, not an enabled flag.

The rule the official controller uses to decide whether an action is live is not
obvious, and reads:

```js
status.actions.some(a => a.name === wanted && a.url) ? true : !status.streamUrl
```

So an action counts only when it is listed **with a URL** — a listing without one
is a declaration that it is *unavailable*, which is why the example above means
"you cannot go back" rather than "back takes no parameters". When nothing
matches, the fallback is `!streamUrl`, because `streamUrl` is set when the player
is playing a URL directly rather than working through its queue: a radio stream,
or `Capture:hw:...` for a physical input. Neither has a next track.

That fallback is also the best available signal for "is this player working
through its queue right now", which is how Azzurro uses it. It has been checked
against an input and against a stream, but **not** against library playback — if
a player turns out to set `streamUrl` while playing from its queue, this is the
one inference in this document that would need revisiting.

### The play queue

**Observed.** Two endpoints describe the same thing, at different levels.

`/Playlist` is the structured one, and the better base for a client:

```xml
<playlist length="42" id="692" shuffle="0" repeat="0">
  <song service="LocalMusic" id="0">
    <art>The Rolling Stones</art>          <!-- artist -->
    <alb>Forty Licks CD2</alb>             <!-- album -->
    <title>You Got Me Rocking</title>
    <track>8</track>  <discno>2</discno>  <date>2002</date>
    <time>214</time>                       <!-- seconds -->
    <fn>/var/mnt/…/file.flac</fn>
    <quality>cd</quality>
    <image>/Artwork?service=LocalMusic&amp;artist=…</image>
  </song>
```

The whole queue comes in one document. `start=` and `end=` take a window, both
ends included — `?start=5&end=7` returns three songs. `length` is always the
whole queue, not the size of the window. A song's `id` is its queue position and
is what `/Play?id=` takes.

`id` on the root is the queue's identity and matches `pid` in `/Status`; when it
changes the player has replaced the queue. The device says as much itself, via
`<refreshOnStatusChange key="pid" value="692"/>` on its own queue screen.

`/ui/Queue` is the server-driven version: paginated twenty at a time via
`?offset=`, durations pre-formatted as `3:58`, and each item carrying the
device's own context menu plus a `<nowPlayingMatch key="song" value="0"/>`
telling the client how to decide which row is playing. Its Save, Edit and Clear
buttons and its per-item context menus (`/ui/queueItemCM?id=N`) are where
removing and reordering live — there is no plain endpoint for either, so queue
editing has to go through the server-driven UI.

Note that the queue keeps its position while a player is on an input: `pid` and
`song` both stay put. The right reading is "where playback would resume", not
"what you are hearing".

### Changing state

**Transcribed.** None of these were exercised against hardware.

| Endpoint | Parameters |
| --- | --- |
| `/Play` | none; `seek=<secs>`; `id=<queue index>` |
| `/Pause` | none; `toggle=1` for play/pause in one round trip |
| `/Stop`, `/Skip`, `/Back` | none |
| `/Volume` | `level=0..100`, or `db=`, or `mute=0|1` |
| `/Shuffle` | `state=0|1` |
| `/Repeat` | `state=0|1|2` — **0 is all, 1 is one, 2 is off**, confirmed below |
| `/Clear` | none; empties the queue |
| `/Preset` | `id=<n>` |
| `/AddSlave`, `/RemoveSlave` | `slave=<host>&port=<port>`, called on the master |
| `/Sleep` | none observed; cycles the sleep timer |

The repeat numbering is worth stating twice, because the order is not the one
anyone guesses and getting it backwards silently swaps "repeat everything" for
"repeat nothing". The official controller renders `repeat === 0` as "Repeat
All", `1` as "Repeat One" and `2` as "Off", and treats `repeat !== 2` as the
condition for the button being lit.

**Declared.** These appear in the request descriptions the player serves from
`/Services`, with their parameters spelled out there: `/Add`, `/Info`,
`/AddFavourite`, `/DeleteFavourite`, `/SetPreset`, `/AddToPlaylistOptions`,
`/Action`, `/Delete`.

### Artwork

**Observed.** Art references come in two shapes in the same field, and a client
has to handle both:

* a path relative to the player — `/images/capture/ic_tvNP.png` for the icon of
  a physical input, or `/Artwork?service=…&artist=…&album=…` for a library
  cover;
* an absolute `https://` URL at a streaming service's CDN, which is the only
  reason a controller needs a TLS stack at all.

`/Artwork` on port 11000 does not serve the image itself. It answers **301** to
a second service on the same host at **port 11004**:

```
GET  http://player:11000/Artwork?service=LocalMusic&artist=…&album=…
301  http://player:11004/library/v1/Artwork?album=…&artist=…&service=LocalMusic
200  image/jpeg, 600×600
```

So any client has to follow redirects, and port 11000 is not the only one worth
allowing through a firewall. The `<image>` on a queue song and the `image`
attribute on a `/ui/Queue` item are the same URL.

Covers are per *album*, not per track: every track from one album shares an
artwork URL. A 42-track queue on the test player needed 22 fetches, so
deduplicating by URL before fetching roughly halves the work on a queue built
from albums and does far better on one built from a single record.

## The server-driven UI

**Observed.** This is the part that most changes how a third-party client has to
be built, and it is easy to miss.

The official controller does not hardcode its screens. It asks the player what
screens exist:

```
GET /ui/Configuration
  → home, recentlyPlayed, news, favourites, sources, search,
    nowPlayingContextMenu, queueItemContextMenu, resolveSoviURL, queue, presets
```

and then fetches each one as a document describing what to draw:

```xml
<screen screenTitle="Home" id="screen-home" refreshOnPlayerChange="true">
  <refreshOnStatusChange key="prid" value="0"/>
  <row id="mostUsed" title="Most Used" scrollable="true">
    <source icon="/images/capture/ic_tv.png" title="HDMI ARC">
      <button text="Play" backgroundColor="#00a4cb">
        <action type="player-link" URI="/Play?url=..."/>
```

`/Services` is a second grammar covering browse: `menuEntry`, `menuGroup`,
`browseRequest`, `requestItemParameter`, `disableOnAttribute`, `sort`, `filter`.
It is how a service the client has never heard of becomes browsable — the player
describes the requests, and the client issues them.

**A client that implements these two grammars gets every music service for
free. A client that does not cannot browse at all.**

The good news is that the vocabulary is small and closed. Counted across every
screen the test player serves:

* **~25 screen elements** — `screen`, `row`, `item`, `list`, `source`, `input`,
  `teaser`, `button`, `search`, `queue`, `footer`, `infoPanel`, `contextMenu`,
  `selectorMenu`, `menuAction`, `playAction`, `largeThumbnail`,
  `smallThumbnail`, `nowPlayingMatch`, `refreshOnStatusChange`,
  `customiseScreen`, `configuration`, `service`, `action`
* **9 action types** — `browse`, `player-link`, `context-browse`, `deep-link`,
  `webpage`, `setting`, `settings`, `add`, `reorder`
* **~20 elements** in the `/Services` menu grammar

That is a `match`, not an open-ended document renderer, which is why a toolkit
without runtime widget-tree construction can still render it.

## What a third-party client cannot do

* **Link a new music service.** `/redirectToCp?href=...` sends the user to
  Lenbrook's own cloud control panel. A client can open it in a browser; it
  cannot reimplement it.
* **Anything account-backed.** The official app bundles Firebase and has its own
  account flow.
* **Player settings pages.** `/Settings?id=...` are web pages served by the
  player, not structured documents. They can be shown in a browser.

## Method

The unofficial Linux controller ships as an AppImage containing an unpacked
Electron app; its `package.json` says `UNLICENSED`. It was read to understand the
wire format — in particular the LSDP decoder, which is the only complete
description of that format that exists anywhere — and nothing was copied from
it. Every byte layout here was re-derived and then checked against a real
player's traffic, and the test vectors in `crates/bluos` are captures, not
transcriptions.
