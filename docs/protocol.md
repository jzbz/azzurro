# The BluOS protocol, as far as it has been worked out

BluOS publishes no protocol specification. Everything here was established one
of three ways, and each claim below says which:

* **Observed** — a request this project made to a real player, whose response
  was read. The player was an NAD Powernode N330, on BluOS 4.16.6 and latterly
  4.16.22 — the upgrade section below is an account of it going from one to the
  other. Addresses and MACs in the examples have been replaced with
  documentation values.
* **Declared** — the player itself describes the request, in the XML it serves
  from `/Services` or `/ui/*`. As good as observed, because the device is the
  one making the claim.
* **Transcribed** — read out of the official controller's own code and not
  exercised here, because exercising it would have meant changing the state of
  somebody's stereo. Treat as likely but unconfirmed.

Nothing in this document came from Lenbrook, and none of their code is
reproduced in this repository.

A player is four services on four ports, and only the first is discoverable:

| Port | What answers there |
| --- | --- |
| 11430/udp | LSDP discovery. Broadcast, unauthenticated. |
| 11000 | The control API. Everything below unless it says otherwise. |
| 11001 | Settings, read only — writes go back to 11000. |
| 11004 | The library service, where `/Artwork` redirects to. |

Only the first is broadcast. 11000 is named in the discovery packet's TXT
`port` key; 11001 and 11004 are never announced at all and are found only by
following a redirect out of 11000. So a client that does not follow redirects
sees no artwork and no settings, and a firewall that opens only the control
port breaks both.

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
through its queue right now", which is how Azzurro uses it. **Confirmed on
hardware**: a player working through its queue omits `<streamUrl>` entirely, and
reports `canSeek` 1, a `totlen`, and separate `artist`/`album` fields that an
input never has. The same player on HDMI ARC sets `streamUrl` to
`Capture:hw:…`. So "no streamUrl" really does mean "playing the queue".

Library playback also adds fields an input does not carry: `serviceName` and
`serviceIcon` for display, `isFavourite`, `canMovePlayback`, `streamFormat`
(`FLAC 24/44.1`), and `fn` for the file on disk.

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
telling the client how to decide which row is playing. Unlike every other
server-driven document its root is `<queue>` rather than `<screen>`, and it
carries `offset`, `total` and `modified` — the last of which is the player
saying the queue has been changed away from the playlist that filled it.

Its `<button>` children are the row under the list. They sit directly under the
root with no section around them, which is the only place in any of these
documents that happens.

Editing the queue does **not** have to go through the server-driven UI, despite
appearances. Both verbs are plain endpoints, and the official controller uses
them directly:

| Endpoint | Parameters |
| --- | --- |
| `/Move` | `new=<destination>&old=<source>`, both absolute queue positions |
| `/Delete` | `id=<queue position>` |
| `/Clear` | none, or `nextlist=1` to drop only the autofill tail |
| `/Save` | `name=<playlist name>` |

`/Move` reads backwards and the player gives no hint which way round it is, so
it was settled on hardware: with a thirteen-track queue, `Move?new=10&old=12`
put the last track at position ten and left the rest in order. Everything after
a `/Delete` shifts down, so removing several means working from the end or
re-reading in between.

Neither is announced. The queue's `id` does not change for a reorder or a
delete, so `/Status` looks identical and a client has to re-read the queue
itself.

Note that the queue keeps its position while a player is on an input: `pid` and
`song` both stay put. The right reading is "where playback would resume", not
"what you are hearing".

### Changing state

**Transcribed.** None of these were exercised against hardware.

| Endpoint | Parameters |
| --- | --- |
| `/Play` | none; `seek=<secs>`; `id=<queue index>`; `url=<stream>` |
| `/Pause` | none; `toggle=1` for play/pause in one round trip |
| `/Stop`, `/Skip`, `/Back` | none |
| `/Volume` | `level=0..100`, or `db=`, or `mute=0|1` |
| `/Shuffle` | `state=0|1` |
| `/Repeat` | `state=0|1|2` — **0 is all, 1 is one, 2 is off**, confirmed below |
| `/Clear` | none; empties the queue. `nextlist=1` drops only the autofill tail |
| `/Move` | `new=<destination>&old=<source>` — see the queue section |
| `/Delete` | `id=<queue position>` |
| `/Save` | `name=<playlist name>`; saves the queue as a playlist |
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
`/Action`.

### Saying which schema you understand

The player serves different documents to clients that declare different schema
versions, and the difference is not cosmetic. Two request headers do it:

```
x-sovi-schema-version: 35
x-sovi-ui-schema-version: 7
```

Those are the numbers the official controller sends. Fetching every screen with
and without them, against a Powernode on BluOS 4.16.6:

- The play queue gains a fourth button, **Queue builder mode**, whose action is
  `player-link` to `/ui/action?CBQ=true`. A client that declares nothing is
  never offered it.
- The queue's Clear button changes from `type="player-link"` to
  `type="confirmation"`, carrying `title="Clear queue?"` — the player asking to
  be asked.
- The Sources screen hands out plain `browse` actions to
  `/ui/browseMenuGroup?service=<name>` for the music services, where an older
  client is given a `deep-link` to `/music-service/<name>` that it has to know
  how to translate for itself.
- Home gains a `<fetch url="/ui/myPlaylistsRow?service=LocalMusic"
  itemType="largeThumbnail"/>` — a shelf whose contents are fetched separately —
  along with another row, teaser and menu action.

Toggling Queue Builder Mode with `/ui/action?CBQ=true` returns 200 with an empty
body, but on this firmware nothing observable follows: no `<modeIndicator>`
appears on any screen and no `<addAction>` replaces a `<playAction>`. The button
is served; whatever it drives is not. Since the button and its action both come
from the player, a client that simply draws the buttons it is given gets the
feature for free on firmware where it does work.

### Grouping and zones

**Shapes transcribed from the official controller's own parser; not exercised
against hardware**, because only one player has ever been available here.

BluOS has two distinct notions and they are easy to conflate:

* **Groups** are ad-hoc multi-room sync. A master leads, slaves follow its
  transport and content, and each keeps its own volume.
* **Zones** are a persistent hardware pairing — a stereo pair, or a surround
  set — described by `zoneMaster`, `zoneSlave`, `channelMode` and `channelName`,
  and configured rather than assembled on the fly.

Only groups are implemented. `/SyncStatus` reports them from both sides, and the
two sides do not look alike:

```xml
<!-- on the master -->
<slave id="10.0.0.9" port="11000"></slave>

<!-- on the slave: the address is the element's TEXT, not an attribute -->
<master port="11000" reconnecting="false">10.0.0.155</master>
```

Note that `id` on a `<slave>` is a bare host, whereas `id` on `<SyncStatus>`
itself may be either `host:port` or a bare host with a separate `port`
attribute. `reconnecting="true"` means the follower has lost its leader and is
trying to get it back — worth showing rather than drawing a healthy group.

Both `/AddSlave?slave=<host>&port=<port>` and `/RemoveSlave?…` are sent **to the
master**: it owns the group, and the slave finds out afterwards.

A player may report its own `id` as `127.0.0.1`, which is true where it is
standing and useless from anywhere else. The official controller substitutes the
address it actually reached the player on, and so does this crate.

`syncStat` in `/Status` mirrors `/SyncStatus`'s own etag, so a controller
already long-polling `/Status` learns that grouping changed without a second
request.

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

### Settings — a third service, on port 11001

**Observed.** `/Settings` on the control port is not the settings service. It
answers **301** to a third port:

```
GET  http://player:11000/Settings
301  http://player:11001/Settings
200  text/xml — <settings schemaVersion="35"> <setting id="alarms" …/> …
```

So settings are structured documents, not web pages, and a client can draw them
itself rather than opening a browser. Each `<setting>` carries an `id`, a
`displayName`, an `icon`, a `class` that says what kind of control it is, and
the URL a write goes to.

The trap is that reads and writes are on **different ports**. A settings
document is read from 11001, and a write goes back to **11000** — posting it to
where the document came from answers 404. Confirmed by writing a setting the
value it already held, then a different one, and watching it change and revert.

### Saved playlists have no id

**Confirmed.** `/Save?name=…` keeps the current queue and answers
`<saved><entries>42</entries></saved>`. `/Playlists` then lists them:

```xml
<playlists service="BluOS">
  <name image="/Artwork?service=LocalMusic&amp;fn=…">Azzurro test playlist</name>
</playlists>
```

The name is the **element text**, and there is no id anywhere — a playlist is
addressed by its name, which is why deleting one is
`/Delete?name=<name>&service=BluOS` and answers `<deleted>…</deleted>`.

A player with none answers `<playlists service="BluOS"></playlists>`, and its
`/ui/Sources` does not mention the BluOS service at all. The moment one exists,
a **BluOS Playlists** shelf appears on `/ui/Home` and the playlist's own
context menu carries Play now, Shuffle, Add next, Add last and Delete — the
last as a confirmation. **Renaming is offered nowhere**, in no menu and in no
request description.

`/ui/browseMenuGroup?service=BluOS` browses them, with a sort selector using
`Csort=BluOS-0~alpha` — the same client-carried context as `Cfilter`, and it
must be echoed back the same way.

### Reordering presets, and how it deletes them

**Observed, and documented nowhere at all.** The Presets screen's footer offers
`/reorder-presets?url=%2FPresets%2Fedit%3Fprid%3D34`, a route into the client.
`/Presets/edit` answers **405** to a GET with no `Allow` header, and
`must be application/json` to a POST with no body. Nothing in `/Services`
describes it.

The shape came out of the player's own type errors, which name its internals:

```
POST /Presets/edit?prid=34    Content-Type: application/json
{"prid": 34, "ordering": [{"from": 4, "to": 1}, …]}
```

`BulkEdit { prid: int, ordering: []preset.Move }`, `Move { from: int, to: int }`.
Send an array rather than an object and it says so by name — *cannot unmarshal
array into Go value of type preset.BulkEdit* — and a wrong type on a field
names the field and its type, which is how the whole thing was read.

**Each move overwrites its destination, and the list must be a complete
permutation.** A `to` that is nobody's `from` is a preset destroyed, silently,
with an ordinary success in reply — `{"from":1,"to":3}` on four presets deleted
the one in slot 3. Measured on a Powernode running 4.16.22, twice: once by
accident on a real preset, once deliberately on throwaways.

Sent whole it is atomic and behaves: reversing four presets in one request
reverses them rather than walking into itself, and `4→1, 1→2, 2→3, 3→4` moves
the last to the front and shifts the rest down.

Slots are not necessarily contiguous — deleting a preset leaves a gap — so the
permutation is over whatever slots are occupied, not over `1..n`.

`prid` identifies the list and changes on **every** edit, including saves and
deletes; the Presets screen carries `refreshOnStatusChange key="prid"` so a
client knows to redraw.

### A grouped list continues as a document of its own

**Confirmed.** A long library list — Artists, 448 of them — is served behind an
alphabet index, and its `<nextLink>` points at `/ui/browseGrouped?…&
listContinuation=30&…`. What comes back has **no `<screen>` around it**:

```xml
<list offset="30" total="448">
  <index revision="223">
    <item key="#" offset="0" length="6"></item>
    <item key="B" offset="28" length="35"></item>
  </index>
  <item title="Bach">…</item>
  <nextLink>/ui/browseGrouped?…&amp;listContinuation=60&amp;…</nextLink>
</list>
```

So `<list>` is a root element alongside `<screen>`, `<contextMenu>` and
`<queue>`. A parser that insists on one of those three reads every continuation
as malformed, and such a list can never be paged at all — thirty of four
hundred and forty-eight, with a cursor pointing at a page nothing will read.

Two details worth having. The `<index>` items are somewhere to scroll to rather
than rows to draw — no title, no action — so they must not become rows. And
`total` is here, which most screens do not give: a grouped list is the one place
paging has an absolute end rather than only an opaque cursor.

`listContinuation` advances by the page size, and a Powernode on 4.16.22 pages
this list in 30s: fourteen requests, the last bringing 28.

**And some lists count instead of pointing.** A library's Songs answers

```xml
<list offset="0" total="2062">
```

with thirty rows and **no `<nextLink>` at all**. It is not unpaged — asking for
the same URL with `&listContinuation=30` returns `<list offset="30"
total="2062">` and the next thirty. The client is expected to do the
arithmetic: `offset` plus the rows it got, until `total`.

So there are two paging protocols on the same routes, and a client needs both:
follow the cursor where there is one, count where there is not. Which a list
uses is not something to infer from its name — Artists points and Songs counts,
on the same `/ui/browseGrouped` endpoint.

Lists the player does not page at all give neither: Genres answers all 175 in
one `<list>` with no `offset`, no `total` and no cursor. Absence of a total is
the signal that a list is whole, and must not be turned into a request.

### The UI context is the client's to carry

**Confirmed.** A screen can offer a `selectorMenu` — Radio Paradise's is
"Filter by quality", MQA against CD Quality — whose items are ordinary
`player-link` actions:

```xml
<selectorMenu menuTitle="Filter by quality" replaceScreen="false">
  <item text="MQA" selected="true">
    <action type="player-link" URI="/ui/action?Cfilter=RadioParadise-~20" refreshScreen="true"/>
  </item>
  <item text="CD Quality">
    <action type="player-link" URI="/ui/action?Cfilter=RadioParadise-~4" refreshScreen="true"/>
  </item>
</selectorMenu>
```

Running one answers **200 with an empty body** and this header:

```
X-Sovi-Ui-Context: eyJ2IjoxLCJmaWx0ZXJzIjp7IlJhZGlvUGFyYWRpc2UtIjoiNCJ9fQo=
```

which is base64 for `{"v":1,"filters":{"RadioParadise-":"4"}}`.

**The filter is not state the player keeps.** It is state the player hands to
the client, and the client must send back on subsequent requests. Fetch the
screen again without that header and it comes back exactly as it was —
`MQA selected="true"` and every item still at `:20`. Send it and the same URL
answers with `CD Quality selected="true"` and items at `:4`. Confirmed both
ways on a Powernode running 4.16.22.

This is why a picker can look broken while everything about it works: the
action runs, `refreshScreen` is honoured, the screen is re-fetched — and the
re-fetch says "no filter" because the context went nowhere.

Treat it as opaque and round-trip it. Note also that the syntax is validated:
`Cfilter=RadioParadise-4`, without the `~`, answers 400 `Invalid filter
parameter`.

**The key belongs to the screen, not to the service.** The example above is
from `/ui/BrowseObjects?service=RadioParadise&url=%2FRadioBrowse`, whose filter
is `RadioParadise-`. The screen the app actually browses to reach the same
service is `/ui/browseMenuGroup?service=RadioParadise`, and its filter is
`RadioParadise-0`. Two screens onto one service, two keys, and a context
carrying one says nothing about the other — which is a good reason to carry the
header whole and never to build one.

A related trap for anyone poking at this with `curl`: the player answers a
*different, older-shaped* document when the `x-sovi-schema-version` and
`x-sovi-ui-schema-version` headers are absent — two plain `<list>` sections
rather than a `selectorMenu` and one list. A hand-made request is not what the
app sees.

### Playing something the player did not offer

**Confirmed.** `/Play?url=` is how a `player-link` row starts what it names,
and the value is normally the player's own scheme — `RadioParadise:/5:20/…`,
`Capture:bluez:bluetooth`. It also takes an **ordinary URL**:

```
GET  http://player:11000/Play?url=http%3A%2F%2Fice1.somafm.com%2Fgroovesalad-128-mp3
200  <state>stream</state>
```

and `/Status` then reports `service` as `http`, the address back as
`streamUrl`, the station's description as `title1` and its live track as
`title2` — so an arbitrary stream behaves like any other source, metadata
included. Confirmed on a Powernode running 4.16.22, both ways: an unreachable
`http://` address answers `<state>stream</state>`, fails to connect and falls
back to `stop` with the queue untouched; a real one plays.

Nothing in the official controller offers this — there is no box anywhere in it
for a stream address — so a station missing from TuneIn is unreachable there.

Like any play action it answers a `<dialog>` **instead of acting** when
starting it would discard the play queue, so the reply has to be read rather
than dropped.

### Firmware upgrades

**Observed.** Not on the page a browser sees. `GET /upgrade?noheader=1` on
**port 80** is a jQuery Mobile page meant for a person, and the official
controller does not parse it — it loads it in an embedded webview and lets the
user click whatever the player drew. Anything read out of that page is a guess
at someone else's HTML. The machine-readable route is on the control port:

```
GET  http://player:11000/upgrade?upgrade=check
200  <?xml version="1.0" encoding="UTF-8" standalone="yes"?>
     <upgrade inProgress="false" version="4.16.22" available="true">check</upgrade>
```

captured from a Powernode running 4.16.6. Three values of `upgrade=` exist:

* `check` — non-destructive, and treated that way: the official app polls it
  every 15 seconds while waiting for a player to come up;
* `this` — starts an upgrade on the addressed player;
* `all` — the official app uses this only for players too old to talk to and
  for its retry button. Nothing states what it covers; that it means the master
  plus its zone members is an **inference** from those two call sites.

`&slave=<host>&port=<port>` addresses a zone member through its master.

Note `version`: the player names the release it is offering, and the official
controller reads straight past it — its parser takes `inProgress` and
`available` and discards the rest. A client that wants to say *which* update is
waiting can, and the app people compare against does not.

The one precondition worth enforcing is the one that app enforces: send
`this` only when a check has answered `available="true"` and
`inProgress="false"`. Starting a second upgrade over a running one is the way
to turn a working speaker into a brick.

**Observed.** While an upgrade runs, `/SyncStatus` **stops answering with
`<SyncStatus>`** and answers with a different root element:

```
<UpgradeStatusStage1 name="Powernode" model="N330" .../>
<UpgradeStatusStage2 name="Powernode" model="N330" .../>
```

A parser that insists on the usual root reads this as a malformed player at
exactly the moment it most needs watching. The elements can carry `step`,
`total`, `percent`, `error` and `abortable` — but **a Powernode sends none of
those, in either stage**, for the whole of an upgrade. Working the stage out
from `step` alone, which is what the official app does, therefore reads every
moment of the install as the first one; take the stage from *which element*
arrived and let the counters refine it only where they exist.

`abortable` is reported and there is nothing to do with it. The official app
parses the field and never reads it again, and no route for stopping an
upgrade appears anywhere in its bundle. Treat an upgrade as uninterruptible.

Timing, from one run on a Powernode going 4.16.6 → 4.16.22:

```
   0s  upgrade=this sent
  72s  player stops answering
  88s  answering again, UpgradeStatusStage1
 104s  UpgradeStatusStage2
 265s  answering as SyncStatus again, version="4.16.22"
```

Two things follow for a client. Do not hold a long poll open across this: the
player reboots partway through and an etag poll waits for a reply that is never
coming — the official app drops to a bare five-second `/SyncStatus` while
upgrading, and that is the right shape. And do not treat the first failed
request as a player that has gone: silence is the normal middle of an upgrade.

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
* **Sign-in forms behind a service.** The page a service's sign-in lives on is
  HTML meant for a browser, asks for a password and sometimes a captcha, and is
  the player's own. It can be opened; it should not be reimplemented.

## Method

The unofficial Linux controller ships as an AppImage containing an unpacked
Electron app; its `package.json` says `UNLICENSED`. It was read to understand the
wire format — in particular the LSDP decoder, which is the only complete
description of that format that exists anywhere — and nothing was copied from
it. Every byte layout here was re-derived and then checked against a real
player's traffic, and the test vectors in `crates/bluos` are captures, not
transcriptions.
