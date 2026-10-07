//! A household of BluOS players that is not there, for screenshots.
//!
//!     cargo run -p fake-player --example demo-player
//!
//! Three players, each on the control port a real one uses:
//!
//!     127.0.0.2:11000   Living Room   leads a group, playing an album
//!     127.0.0.3:11000   Kitchen       follows Living Room
//!     127.0.0.4:11000   Studio        playing something else, groupable
//!
//! Type `127.0.0.2` (then .3 and .4) into "Add a player by address". All of
//! 127.0.0.0/8 is the loopback on Linux; macOS answers on 127.0.0.1 alone
//! unless more are aliased, and there `--at` names addresses that are.
//!
//! Announcing a loopback address over LSDP would come to nothing — the app
//! refuses an announcement for an address on no local network — so in a
//! network namespace of its own, with a TEST-NET address on its loopback, the
//! demo can sit on that network instead and announce itself to be found
//! unasked:
//!
//!     cargo run -p fake-player --example demo-player -- --at 192.0.2.10 --announce 192.0.2.255
//!
//! The app has to run in that same namespace, to hear the announcements and to
//! reach the rooms at all. Bringing `lo` up there with 192.0.2.10/24 on it is
//! enough: that puts .10, .11 and .12 on a network the app counts as local.
//!
//! Options:
//!
//!     --at ADDR         where the next room goes; rooms not placed follow on
//!                       from the last address given (default 127.0.0.2)
//!     --port N          the control port, the same for every room (11000)
//!     --announce ADDR   send an LSDP announcement for every room to ADDR
//!                       every three seconds
//!     --still           hold every clock where it starts, so the position on
//!                       screen reads the same from one frame to the next
//!     --covers DIR      write the cover art into DIR as PNG files and exit
//!
//! A search for "weather" finds an album and songs from four records; one for
//! "cirrus" finds an artist, their album and its songs.
//!
//! Everything it serves is invented: the artists, albums and tracks below, the
//! rooms, the MAC addresses (locally administered, 02:…), and the cover art,
//! which is painted here from each album's three colors rather than read from
//! anywhere. Nothing on the screen can say anything about anybody's music.
//! Real content would not do: a household's library would show whose records
//! they are, and the covers of real albums are not this project's to publish.

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_player::{Player, Request};

#[derive(Clone, Copy)]
enum Motif {
    Bands,
    Moon,
    Rings,
    Orbit,
    Sunburst,
    Dots,
    Diagonals,
    Blobs,
    Crescents,
    Ridges,
    Quarters,
    Stripes,
}

struct Album {
    artist: &'static str,
    title: &'static str,
    year: u16,
    genre: &'static str,
    /// The library's word for it, which is what a badge shows.
    quality: &'static str,
    /// The decoder's own phrase, shown under the badge on Now Playing.
    format: &'static str,
    motif: Motif,
    colors: [[u8; 3]; 3],
    tracks: &'static [(&'static str, u32)],
}

/// The library, twelve records with a cover each.
///
/// Measured against the window at the 980px width screenshots are taken at:
/// every track title fits the queue column whole, and every album title fits
/// the caption of a shelf tile but the first, whose title runs about ten
/// pixels past the tile's 116 and is elided there. It is read whole everywhere
/// else it appears — on Now Playing, the album's own page and the list of
/// albums.
const LIBRARY: &[Album] = &[
    Album {
        artist: "Cirrus Fold",
        title: "Low Pressure Systems",
        year: 2024,
        genre: "Electronic",
        quality: "hd",
        format: "FLAC 24/96",
        motif: Motif::Bands,
        colors: [[0x12, 0x4e, 0x78], [0x2a, 0x9d, 0x8f], [0xe9, 0xc4, 0x6a]],
        tracks: &[
            ("Isobar", 252),
            ("Cold Front", 303),
            ("Dew Point", 227),
            ("Barometer Song", 271),
            ("Altostratus", 362),
            ("Rain Shadow", 258),
            ("Clearing", 326),
            ("Fair Weather", 174),
        ],
    },
    Album {
        artist: "Tidewater Static",
        title: "Ferrylight",
        year: 2022,
        genre: "Indie",
        quality: "cd",
        format: "FLAC 16/44.1",
        motif: Motif::Moon,
        colors: [[0x0b, 0x13, 0x2b], [0x2c, 0x3e, 0x6b], [0xf4, 0xa2, 0x61]],
        tracks: &[
            ("Lamps on the Water", 214),
            ("Ferrylight", 247),
            ("Ropes and Anchors", 233),
            ("Deck Chairs", 198),
            ("Last Crossing", 289),
            ("Port of Call", 205),
        ],
    },
    Album {
        artist: "The Basalt Choir",
        title: "Oxbow Hymnal",
        year: 2019,
        genre: "Folk",
        quality: "cd",
        format: "FLAC 16/44.1",
        motif: Motif::Rings,
        colors: [[0x3a, 0x5a, 0x40], [0xa3, 0xb1, 0x8a], [0xf1, 0xe9, 0xd2]],
        tracks: &[
            ("Oxbow", 241),
            ("Flood Plain Hymn", 312),
            ("Willow Root", 198),
            ("Estuary", 276),
            ("Slow River", 334),
            ("Ferryman's Psalm", 259),
        ],
    },
    Album {
        artist: "Quiet Meridian",
        title: "Sextant Hours",
        year: 2021,
        genre: "Jazz",
        quality: "hd",
        format: "FLAC 24/192",
        motif: Motif::Orbit,
        colors: [[0x14, 0x1b, 0x33], [0x8c, 0x2f, 0x39], [0xe0, 0xb0, 0x4a]],
        tracks: &[
            ("Prime Meridian", 388),
            ("Dead Reckoning", 421),
            ("Sextant", 296),
            ("Twelve Bells", 344),
            ("Trade Winds", 402),
            ("Equator Blues", 377),
        ],
    },
    Album {
        artist: "Saffron Engine",
        title: "Brass Weather",
        year: 2023,
        genre: "Funk",
        quality: "cd",
        format: "FLAC 16/44.1",
        motif: Motif::Sunburst,
        colors: [[0xe7, 0x6f, 0x24], [0xc2, 0x18, 0x5b], [0xff, 0xd1, 0x66]],
        tracks: &[
            ("Heat Haze", 233),
            ("Thunderhead Strut", 264),
            ("Brass Weather", 251),
            ("Monsoon Shuffle", 289),
            ("Humidity", 216),
        ],
    },
    Album {
        artist: "Pale Lantern Orchestra",
        title: "Attic Nocturnes",
        year: 2020,
        genre: "Classical",
        quality: "hd",
        format: "FLAC 24/96",
        motif: Motif::Dots,
        colors: [[0x2b, 0x2d, 0x42], [0x4a, 0x4e, 0x69], [0xf2, 0xe8, 0x8f]],
        tracks: &[
            ("Kitchen Nocturne", 312),
            ("Lamp at the Window", 287),
            ("Skylight", 341),
            ("Hallway Waltz", 226),
            ("Attic Light", 298),
        ],
    },
    Album {
        artist: "Copper Moth",
        title: "Signal Box Diaries",
        year: 2025,
        genre: "Electronic",
        quality: "cd",
        format: "FLAC 16/44.1",
        motif: Motif::Diagonals,
        colors: [[0xb8, 0x73, 0x33], [0x1f, 0x6f, 0x78], [0xf5, 0xf0, 0xe1]],
        tracks: &[
            ("Platform Four", 244),
            ("Signal Box", 218),
            ("Overhead Lines", 297),
            ("Sleeper Car", 331),
            ("Terminus", 276),
        ],
    },
    Album {
        artist: "Glass Orchard",
        title: "Hothouse Petals",
        year: 2018,
        genre: "Pop",
        quality: "cd",
        format: "FLAC 16/44.1",
        motif: Motif::Blobs,
        colors: [[0xf4, 0xac, 0xb7], [0x9d, 0x81, 0xba], [0xff, 0xf1, 0xe6]],
        tracks: &[
            ("Greenhouse", 229),
            ("Petal Static", 254),
            ("Second Bloom", 268),
            ("Pollen Count", 201),
            ("Orchard at Dusk", 312),
        ],
    },
    Album {
        artist: "Velvet Almanac",
        title: "Crescent Postcards",
        year: 2017,
        genre: "Indie",
        quality: "cd",
        format: "FLAC 16/44.1",
        motif: Motif::Crescents,
        colors: [[0x1d, 0x35, 0x57], [0x45, 0x7b, 0x9d], [0xf1, 0xfa, 0xee]],
        tracks: &[
            ("Paper Moons", 236),
            ("Midnight Garden", 281),
            ("Lantern Parade", 247),
            ("Postcard Weather", 265),
            ("Morning Anyway", 302),
        ],
    },
    Album {
        artist: "Marram Grass Union",
        title: "Dune Machinery",
        year: 2016,
        genre: "Post-Rock",
        quality: "cd",
        format: "FLAC 16/44.1",
        motif: Motif::Ridges,
        colors: [[0x6b, 0x3e, 0x2e], [0xd9, 0x8c, 0x4a], [0xf6, 0xe3, 0xb4]],
        tracks: &[
            ("Windbreak", 288),
            ("Sand Engine", 342),
            ("Marram", 251),
            ("Tideline Motor", 307),
            ("Shifting Ground", 365),
        ],
    },
    Album {
        artist: "Kiln & Kettle",
        title: "Terracotta Mornings",
        year: 2023,
        genre: "Soul",
        quality: "cd",
        format: "FLAC 16/44.1",
        motif: Motif::Quarters,
        colors: [[0xb5, 0x4a, 0x2c], [0x2f, 0x4f, 0x4a], [0xf2, 0xd7, 0xb6]],
        tracks: &[
            ("Glaze", 219),
            ("Slow Firing", 263),
            ("Clay Hands", 241),
            ("Kitchen Weather", 198),
            ("Second Cup", 230),
            ("Terracotta", 276),
        ],
    },
    Album {
        artist: "Juniper Relay",
        title: "Stairwell Echoes",
        year: 2024,
        genre: "Ambient",
        quality: "hd",
        format: "FLAC 24/48",
        motif: Motif::Stripes,
        colors: [[0x23, 0x3d, 0x2f], [0x5f, 0x8f, 0x6e], [0xd8, 0xe8, 0xc8]],
        tracks: &[
            ("Ground Floor", 301),
            ("Landing", 412),
            ("Handrail", 267),
            ("Up the Well", 355),
            ("Rooftop Door", 389),
        ],
    },
];

/// The shelves on Home, by position in [`LIBRARY`].
const RECENTLY_PLAYED: &[usize] = &[0, 3, 1, 4, 2, 5, 9, 7];
const RECENTLY_ADDED: &[usize] = &[11, 10, 6, 8, 7, 9];

struct RoomDef {
    name: &'static str,
    /// Generic words, not a product: the card shows this only while idle.
    model: &'static str,
    volume: u8,
    album: usize,
    track: usize,
    /// Seconds into the track at start.
    at: u32,
    playing: bool,
    /// Index of the room this one follows at start, if any.
    follows: Option<usize>,
}

const ROOMS: &[RoomDef] = &[
    RoomDef {
        name: "Living Room",
        model: "Streaming amplifier",
        volume: 34,
        album: 0,
        track: 1,
        at: 96,
        playing: true,
        follows: None,
    },
    RoomDef {
        name: "Kitchen",
        model: "Wireless speaker",
        volume: 22,
        album: 0,
        track: 1,
        at: 96,
        playing: true,
        follows: Some(0),
    },
    RoomDef {
        name: "Studio",
        model: "Streamer",
        volume: 41,
        album: 3,
        track: 2,
        at: 40,
        playing: true,
        follows: None,
    },
];

struct Room {
    album: usize,
    track: usize,
    /// Position at `since`.
    at: u32,
    since: Instant,
    playing: bool,
    volume: u8,
    muted: bool,
    leader: Option<usize>,
}

struct World {
    rooms: Vec<Room>,
    addrs: Vec<Ipv4Addr>,
    port: u16,
    /// Bumped on anything that changes a `/Status` or `/SyncStatus`.
    generation: u32,
    /// Whether every clock stands where it is. See [`still_status`].
    still: bool,
    /// How many statuses have been answered, which a still demo writes into
    /// each etag.
    answered: u32,
}

impl World {
    /// Whose transport and content this room shows: its leader's if it has one.
    fn source(&self, i: usize) -> usize {
        self.rooms[i].leader.unwrap_or(i)
    }

    /// Where room `i`'s source is now: album, track, seconds in. Plays through
    /// the album and round again, so a demo left running stays plausible.
    fn position(&self, i: usize) -> (usize, usize, u32) {
        let room = &self.rooms[self.source(i)];
        let album = &LIBRARY[room.album];
        let mut track = room.track;
        let mut secs = room.at
            + if room.playing && !self.still {
                room.since.elapsed().as_secs() as u32
            } else {
                0
            };
        while secs >= album.tracks[track].1 {
            secs -= album.tracks[track].1;
            track = (track + 1) % album.tracks.len();
        }
        (room.album, track, secs)
    }

    fn settle(&mut self, i: usize) {
        let (_, track, secs) = self.position(i);
        let room = &mut self.rooms[i];
        room.track = track;
        room.at = secs;
        room.since = Instant::now();
        self.generation += 1;
    }

    fn room_at(&self, host: &str) -> Option<usize> {
        let host: Ipv4Addr = host.parse().ok()?;
        self.addrs.iter().position(|a| *a == host)
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// A query value as a player writes one: spaces as `+`, the rest escaped.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn dec(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn clock(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Where an album's cover is, in the shape a library cover has.
fn art(album: &Album) -> String {
    format!(
        "/Artwork?service=LocalMusic&amp;artist={}&amp;album={}",
        enc(album.artist),
        enc(album.title)
    )
}

fn album_link(i: usize) -> String {
    format!("/ui/browseContext?service=LocalMusic&amp;type=Album&amp;album={i}")
}

fn artist_link(i: usize) -> String {
    format!("/ui/browseContext?service=LocalMusic&amp;type=Artist&amp;album={i}")
}

/// The player's own play wrapper around `/Add`, which is what a track row
/// carries on a real player.
fn play_link(album: usize, track: Option<usize>, shuffle: bool) -> String {
    let mut inner = format!("/Add?playnow=1&album={album}");
    if let Some(t) = track {
        let _ = write!(inner, "&track={t}");
    }
    if shuffle {
        inner.push_str("&shuffle=1");
    }
    format!("/ui/prf?u={}", enc(&inner))
}

fn album_tile(i: usize, kind: &str) -> String {
    let a = &LIBRARY[i];
    format!(
        r#"    <{kind} title="{}" subTitle="{}" image="{}" objectType="Album">
      <action type="browse" URI="{}"/>
    </{kind}>
"#,
        esc(a.title),
        esc(a.artist),
        art(a),
        album_link(i)
    )
}

fn artist_tile(i: usize) -> String {
    let a = &LIBRARY[i];
    format!(
        r#"    <item title="{}" image="{}" objectType="Artist">
      <action type="browse" URI="{}"/>
    </item>
"#,
        esc(a.artist),
        art(a),
        artist_link(i)
    )
}

fn sync_status(w: &World, i: usize) -> String {
    let def = &ROOMS[i];
    let room = &w.rooms[i];
    let addr = w.addrs[i];
    let mut inner = String::new();
    for (j, other) in w.rooms.iter().enumerate() {
        if other.leader == Some(i) {
            let _ = writeln!(inner, r#"  <slave id="{}" port="{}"/>"#, w.addrs[j], w.port);
        }
    }
    if let Some(l) = room.leader {
        let _ = writeln!(
            inner,
            r#"  <master port="{}" reconnecting="false">{}</master>"#,
            w.port, w.addrs[l]
        );
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<SyncStatus etag="s{generation}" id="{addr}:{port}" name="{name}" model="DEMO" modelName="{model}" brand="Demo" mac="02:00:00:00:00:{last:02X}" icon="/images/players/demo.png" volume="{vol}" db="-30" schemaVersion="35" version="4.16.22" initialized="true">
{inner}</SyncStatus>"#,
        generation = w.generation,
        port = w.port,
        name = esc(def.name),
        model = esc(def.model),
        last = addr.octets()[3],
        vol = room.volume,
    )
}

fn status(w: &World, i: usize) -> String {
    let (album_i, track, secs) = w.position(i);
    let album = &LIBRARY[album_i];
    let (title, length) = album.tracks[track];
    let source = &w.rooms[w.source(i)];
    let room = &w.rooms[i];
    let state = if source.playing { "play" } else { "pause" };
    // A still demo names every answer differently. See `still_status`.
    let etag = if w.still {
        format!("p{}-{album_i}-{track}-{}", w.generation, w.answered)
    } else {
        format!("p{}-{album_i}-{track}", w.generation)
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<status etag="{etag}"><album>{alb}</album><artist>{art}</artist><canMovePlayback>true</canMovePlayback><canSeek>1</canSeek><cursor>{track}</cursor><db>-30</db><image>{image}</image><indexing>0</indexing><isFavourite>0</isFavourite><mode>1</mode><mute>{mute}</mute><name>{title}</name><pid>{pid}</pid><prid>1</prid><quality>{quality}</quality><repeat>2</repeat><secs>{secs}</secs><service>LocalMusic</service><serviceIcon>/images/LibraryIcon.png</serviceIcon><serviceName>Library</serviceName><shuffle>0</shuffle><sid>5</sid><sleep></sleep><song>{track}</song><state>{state}</state><streamFormat>{format}</streamFormat><syncStat>s{generation}</syncStat><title1>{title}</title1><title2>{art}</title2><title3>{alb}</title3><totlen>{length}</totlen><twoline_title1>{title}</twoline_title1><twoline_title2>{art} • {alb}</twoline_title2><volume>{vol}</volume></status>"#,
        generation = w.generation,
        alb = esc(album.title),
        art = esc(album.artist),
        image = art(album),
        mute = u8::from(room.muted),
        title = esc(title),
        pid = 100 + album_i,
        quality = album.quality,
        format = album.format,
        vol = room.volume,
    )
}

/// How long a still demo sits on each `/Status` before answering it.
///
/// The app carries the clock forward itself between statuses — the position
/// it draws is the one last reported plus the whole seconds since it arrived —
/// so a player that only stops counting is not enough to hold the bar still:
/// the app would count for it. What holds it is a status arriving more often
/// than once a second, so that the seconds since the last one never reach
/// one. An unchanged etag would have the app wait a second between polls,
/// which is exactly the gap to avoid, so each answer carries a new one; and
/// each is held this long first, so the polls come a couple of times a second
/// rather than as fast as the loopback allows.
fn still_status() -> Duration {
    Duration::from_millis(400)
}

/// The queue: the album playing, then the start of the next one, so the list
/// runs past one cover.
fn queue_songs(album_i: usize) -> Vec<(usize, usize)> {
    let next = (album_i + 1) % LIBRARY.len();
    let mut songs: Vec<(usize, usize)> = (0..LIBRARY[album_i].tracks.len())
        .map(|t| (album_i, t))
        .collect();
    songs.extend((0..3.min(LIBRARY[next].tracks.len())).map(|t| (next, t)));
    songs
}

fn playlist(w: &World, i: usize, request: &Request) -> String {
    let (album_i, _, _) = w.position(i);
    let songs = queue_songs(album_i);
    let number = |key| request.param(key).and_then(|v| v.parse::<usize>().ok());
    let start = number("start").unwrap_or(0);
    let end = number("end").unwrap_or(usize::MAX);
    let mut body = String::new();
    for (at, (a, t)) in songs.iter().enumerate() {
        if at < start || at > end {
            continue;
        }
        let album = &LIBRARY[*a];
        let (title, length) = album.tracks[*t];
        let _ = write!(
            body,
            "\n  <song id=\"{at}\" service=\"LocalMusic\"><art>{}</art><alb>{}</alb><title>{}</title><track>{}</track><time>{length}</time><quality>{}</quality><image>{}</image></song>",
            esc(album.artist),
            esc(album.title),
            esc(title),
            t + 1,
            album.quality,
            art(album),
        );
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<playlist name=\"{}\" modified=\"0\" length=\"{}\" id=\"{}\" shuffle=\"0\" repeat=\"2\">{body}\n</playlist>",
        esc(LIBRARY[album_i].title),
        songs.len(),
        100 + album_i
    )
}

fn queue_screen(w: &World, i: usize) -> String {
    let (album_i, _, _) = w.position(i);
    format!(
        r##"<?xml version="1.0" encoding="UTF-8"?>
<queue offset="0" total="{}" id="{}" modified="false">
  <refreshOnStatusChange key="pid" value="{}"/>
  <button text="Save" icon="/images/ui/btn_save_queue.png">
    <action type="browse" URI="/AddToPlaylistOptions?saveQueue=1" resultType="SaveQueueOptions" title="Save playlist"/>
  </button>
  <button text="Edit" icon="/images/ui/btn_edit_queue.png">
    <action type="deep-link" URI="/edit-queue"/>
  </button>
  <button text="Clear" icon="/images/ui/btn_clear_queue.png">
    <action type="confirmation" URI="/Clear" title="Clear queue?" refreshScreen="true" notification="Play Queue cleared"/>
  </button>
</queue>"##,
        queue_songs(album_i).len(),
        100 + album_i,
        100 + album_i
    )
}

const CONFIGURATION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<configuration>
  <item id="home" URI="/ui/Home"/>
  <item id="search" URI="/ui/Search"/>
  <item id="queue" URI="/ui/Queue"/>
  <item id="sources" URI="/ui/Sources"/>
</configuration>"#;

const SOURCES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<screen screenTitle="Sources" id="screen-sources">
  <row id="music" title="Music">
    <service text="Library" image="/images/LibraryIcon.png">
      <action type="browse" URI="/ui/browseMenuGroup?service=LocalMusic"/>
    </service>
  </row>
  <row id="inputs" title="Inputs">
    <input text="HDMI ARC" image="/images/capture/ic_tv.png">
      <action type="player-link" URI="/Play?url=Capture%3Ahw%3Aarc"/>
      <nowPlayingMatch key="inputId" value="arc"/>
    </input>
    <input text="Optical In" image="/images/capture/ic_optical.png">
      <action type="player-link" URI="/Play?url=Capture%3Ahw%3Aspdif"/>
      <nowPlayingMatch key="inputId" value="spdif"/>
    </input>
    <input text="Bluetooth" image="/images/BluetoothIcon.png">
      <action type="player-link" URI="/Play?url=Capture%3Abluez%3Abluetooth"/>
      <nowPlayingMatch key="inputId" value="bluetooth"/>
    </input>
  </row>
</screen>"#;

const SEARCH_BOX: &str = r#"  <search prompt="Search Library" parameterName="q" type="browse" URI="/ui/Search?forService=LocalMusic" resultType="screen" title="Search" service="LocalMusic"/>
"#;

fn home() -> String {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<screen screenTitle="Home" id="screen-home" refreshOnPlayerChange="true">
  <row id="recentlyPlayed" title="Recently Played" scrollable="true">
"#,
    );
    for &i in RECENTLY_PLAYED {
        s.push_str(&album_tile(i, "item"));
    }
    s.push_str(
        r#"  </row>
  <row id="recentlyAdded" title="Recently Added" scrollable="true">
"#,
    );
    for &i in RECENTLY_ADDED {
        s.push_str(&album_tile(i, "item"));
    }
    s.push_str(
        r#"  </row>
  <row id="mostUsed" title="Most Used" scrollable="true">
    <source text="Library" image="/images/LibraryIcon.png">
      <action type="browse" URI="/ui/browseMenuGroup?service=LocalMusic"/>
    </source>
    <source text="HDMI ARC" image="/images/capture/ic_tv.png">
      <action type="player-link" URI="/Play?url=Capture%3Ahw%3Aarc"/>
    </source>
    <source text="Bluetooth" image="/images/BluetoothIcon.png">
      <action type="player-link" URI="/Play?url=Capture%3Abluez%3Abluetooth"/>
    </source>
  </row>
</screen>"#,
    );
    s
}

fn library() -> String {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<screen screenTitle="Library" id="screen-library" service="LocalMusic">
"#,
    );
    s.push_str(SEARCH_BOX);
    for (title, kind) in [
        ("Artists", "Artist"),
        ("Albums", "Album"),
        ("Songs", "Song"),
        ("Genres", "Genre"),
    ] {
        let _ = writeln!(
            s,
            r#"  <row title="{title}"><action type="browse" URI="/ui/browseGrouped?service=LocalMusic&amp;type={kind}"/></row>"#
        );
    }
    s.push_str("  <row id=\"recentlyAdded\" title=\"Recently Added\" scrollable=\"true\">\n");
    for &i in RECENTLY_ADDED {
        s.push_str(&album_tile(i, "item"));
    }
    s.push_str("  </row>\n</screen>");
    s
}

fn song_row(album_i: usize, t: usize, numbered: bool, playing: Option<usize>) -> String {
    let album = &LIBRARY[album_i];
    let (title, length) = album.tracks[t];
    let track = if numbered {
        format!(r#" track="{}""#, t + 1)
    } else {
        String::new()
    };
    let image = if numbered {
        String::new()
    } else {
        format!(r#" image="{}""#, art(album))
    };
    let matching = match playing {
        Some(p) if p == album_i => format!(r#"<nowPlayingMatch key="song" value="{t}"/>"#),
        _ => String::new(),
    };
    format!(
        r#"    <item title="{}" subTitle="{}"{track}{image} duration="{}" quality="{}" objectType="Song">
      <action type="player-link" URI="{}"/>{matching}
    </item>
"#,
        esc(title),
        esc(album.artist),
        clock(length),
        album.quality,
        play_link(album_i, Some(t), false),
    )
}

fn album_screen(album_i: usize, playing: Option<usize>) -> String {
    let a = &LIBRARY[album_i];
    let mut s = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<screen screenTitle="{title}" id="screen-album" service="LocalMusic">
  <header image="{image}" title="{title}" subTitle="{artist}" subSubTitle="{year} • {genre} • {n} Tracks">
    <button text="Play all"><action type="player-link" URI="{play}"/></button>
    <button text="Shuffle"><action type="player-link" URI="{shuffle}"/></button>
  </header>
  <list>
"#,
        title = esc(a.title),
        image = art(a),
        artist = esc(a.artist),
        year = a.year,
        genre = a.genre,
        n = a.tracks.len(),
        play = play_link(album_i, None, false),
        shuffle = play_link(album_i, None, true),
    );
    for t in 0..a.tracks.len() {
        s.push_str(&song_row(album_i, t, true, playing));
    }
    s.push_str("  </list>\n</screen>");
    s
}

fn artist_screen(album_i: usize) -> String {
    let a = &LIBRARY[album_i];
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<screen screenTitle="{}" id="screen-artist" service="LocalMusic">
  <list title="Albums">
{}  </list>
</screen>"#,
        esc(a.artist),
        album_tile(album_i, "item")
    )
}

fn grouped(kind: &str) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let mut order: Vec<usize> = (0..LIBRARY.len()).collect();
    match kind {
        "Artist" => {
            order.sort_by_key(|&i| LIBRARY[i].artist.trim_start_matches("The "));
            s.push_str("<screen screenTitle=\"Artists\" service=\"LocalMusic\">\n  <list>\n");
            for i in order {
                s.push_str(&artist_tile(i));
            }
        }
        "Song" => {
            s.push_str("<screen screenTitle=\"Songs\" service=\"LocalMusic\">\n  <list>\n");
            let mut all: Vec<(usize, usize)> = (0..LIBRARY.len())
                .flat_map(|a| (0..LIBRARY[a].tracks.len()).map(move |t| (a, t)))
                .collect();
            all.sort_by_key(|&(a, t)| LIBRARY[a].tracks[t].0);
            for (a, t) in all {
                s.push_str(&song_row(a, t, false, None));
            }
        }
        "Genre" => {
            s.push_str("<screen screenTitle=\"Genres\" service=\"LocalMusic\">\n  <list>\n");
            let mut genres: Vec<&str> = LIBRARY.iter().map(|a| a.genre).collect();
            genres.sort_unstable();
            genres.dedup();
            for g in genres {
                let _ = writeln!(s, "    <item title=\"{g}\"/>");
            }
        }
        _ => {
            order.sort_by_key(|&i| LIBRARY[i].title);
            s.push_str("<screen screenTitle=\"Albums\" service=\"LocalMusic\">\n  <list>\n");
            for i in order {
                s.push_str(&album_tile(i, "item"));
            }
        }
    }
    s.push_str("  </list>\n</screen>");
    s
}

/// A library search, the way one reads: an artist who matches brings their
/// records and songs along, and a record or a song can match by its own name.
fn search(request: &Request) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<screen screenTitle=\"Search\" id=\"screen-search\" service=\"LocalMusic\">\n",
    );
    s.push_str(SEARCH_BOX);
    let query = request
        .param("q")
        .map(dec)
        .unwrap_or_default()
        .to_lowercase();
    let query = query.trim();
    if !query.is_empty() {
        let hit = |text: &str| text.to_lowercase().contains(query);
        let artists: Vec<usize> = (0..LIBRARY.len())
            .filter(|&i| hit(LIBRARY[i].artist))
            .collect();
        let albums: Vec<usize> = (0..LIBRARY.len())
            .filter(|&i| hit(LIBRARY[i].title) || hit(LIBRARY[i].artist))
            .collect();
        let songs: Vec<(usize, usize)> = (0..LIBRARY.len())
            .flat_map(|a| (0..LIBRARY[a].tracks.len()).map(move |t| (a, t)))
            .filter(|&(a, t)| hit(LIBRARY[a].tracks[t].0) || hit(LIBRARY[a].artist))
            .collect();
        if !artists.is_empty() {
            s.push_str("  <row id=\"artists\" title=\"Artists\" scrollable=\"true\">\n");
            for i in artists {
                s.push_str(&artist_tile(i));
            }
            s.push_str("  </row>\n");
        }
        if !albums.is_empty() {
            s.push_str("  <row id=\"albums\" title=\"Albums\" scrollable=\"true\">\n");
            for i in albums {
                s.push_str(&album_tile(i, "item"));
            }
            s.push_str("  </row>\n");
        }
        if !songs.is_empty() {
            s.push_str("  <list id=\"songs\" title=\"Songs\">\n");
            for (a, t) in songs {
                s.push_str(&song_row(a, t, false, None));
            }
            s.push_str("  </list>\n");
        }
    }
    s.push_str("</screen>");
    s
}

// ---------------------------------------------------------------------------
// Cover art, painted. Abstract on purpose: a picture of nothing in particular
// cannot be mistaken for any real record's sleeve.

const SIDE: u32 = 600;

fn mix(p: [f32; 3], q: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        p[0] + (q[0] - p[0]) * t,
        p[1] + (q[1] - p[1]) * t,
        p[2] + (q[2] - p[2]) * t,
    ]
}

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// How much of a disc of radius `r` covers a pixel `d` from its center.
fn disc(d: f32, r: f32) -> f32 {
    let aa = 1.5 / SIDE as f32;
    1.0 - smooth(r - aa, r + aa, d)
}

fn shade(motif: Motif, u: f32, v: f32, a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    use std::f32::consts::TAU;
    let aa = 1.5 / SIDE as f32;
    let dist = |x: f32, y: f32| ((u - x).powi(2) + (v - y).powi(2)).sqrt();
    match motif {
        Motif::Bands => {
            let mut t = 0.0;
            for k in 0..6 {
                let edge = 0.12 + 0.15 * k as f32 + 0.035 * (u * TAU * 1.3 + k as f32 * 1.7).sin();
                t += smooth(edge - aa, edge + aa, v);
            }
            let t = t / 6.0;
            if t < 0.5 {
                mix(a, b, t * 2.0)
            } else {
                mix(b, c, (t - 0.5) * 2.0)
            }
        }
        Motif::Moon => {
            let horizon = 0.64;
            if v < horizon {
                let sky = mix(a, b, v / horizon);
                mix(sky, c, disc(dist(0.62, 0.36), 0.14))
            } else {
                let depth = (v - horizon) / (1.0 - horizon);
                let water = mix(b, a, depth);
                let wide = 0.16 * (1.0 - 0.6 * depth);
                let glint = smooth(0.2, 0.6, (v * 90.0).sin())
                    * (1.0 - smooth(wide - 0.02, wide, (u - 0.62).abs()));
                mix(water, c, glint * 0.7)
            }
        }
        Motif::Rings => {
            let d = dist(0.38, 0.6);
            let s = smooth(-0.12, 0.12, (d * TAU * 6.0).sin());
            mix(mix(a, b, s), c, disc(d, 0.07))
        }
        Motif::Orbit => {
            // A planet in a tilted ring, against a sky of a few stars.
            let sky = mix(a, mix(a, b, 0.35), v);
            let stars: f32 = [
                (0.12, 0.14),
                (0.86, 0.1),
                (0.2, 0.82),
                (0.9, 0.7),
                (0.7, 0.9),
                (0.08, 0.5),
            ]
            .iter()
            .map(|(x, y)| disc(dist(*x, *y), 0.007))
            .sum();
            let sky = mix(sky, c, stars.min(1.0));
            let (x, y) = (u - 0.5, v - 0.52);
            let (cos, sin) = (0.42f32.cos(), 0.42f32.sin());
            let (rx, ry) = (x * cos + y * sin, -x * sin + y * cos);
            let ellipse = ((rx / 0.4).powi(2) + (ry / 0.11).powi(2)).sqrt();
            let ring = smooth(0.86, 0.9, ellipse) * (1.0 - smooth(0.98, 1.02, ellipse));
            let body = disc(dist(0.5, 0.52), 0.21);
            let lit = mix(b, c, smooth(0.25, -0.2, x + y) * 0.55);
            let planet = mix(sky, lit, body);
            // The far half of the ring passes behind the planet.
            if ry < 0.0 && body > 0.5 {
                planet
            } else {
                mix(planet, c, ring)
            }
        }
        Motif::Sunburst => {
            let (x, y) = (u - 0.08, v - 1.04);
            let theta = y.atan2(x);
            let rays = mix(a, b, smooth(-0.1, 0.1, (theta * 22.0).sin()));
            mix(rays, c, disc((x * x + y * y).sqrt(), 0.3))
        }
        Motif::Dots => {
            let base = mix(a, b, (u + v) / 2.0);
            let (cx, cy) = ((u * 7.0).floor(), (v * 7.0).floor());
            let (fx, fy) = ((cx + 0.5) / 7.0, (cy + 0.5) / 7.0);
            let r = 0.008 + 0.034 * (cy / 6.0) * (0.5 + 0.5 * ((cx * 1.7).sin()).abs());
            mix(base, c, disc(dist(fx, fy), r))
        }
        Motif::Diagonals => {
            let s = ((u + v) * TAU * 5.0).sin();
            let base = mix(a, b, smooth(-0.1, 0.1, s));
            let d = dist(0.5, 0.5);
            let ring = disc(d, 0.27) * (1.0 - disc(d, 0.24));
            mix(base, c, ring)
        }
        Motif::Blobs => {
            let base = mix(a, b, v);
            let field: f32 = [
                (0.3, 0.35, 0.16),
                (0.62, 0.42, 0.13),
                (0.48, 0.7, 0.18),
                (0.78, 0.76, 0.1),
            ]
            .iter()
            .map(|(x, y, r)| r * r / (dist(*x, *y).powi(2) + 1e-4))
            .sum();
            mix(base, c, smooth(0.95, 1.05, field) * 0.85)
        }
        Motif::Crescents => {
            let base = mix(a, b, v);
            let moon = disc(dist(0.5, 0.46), 0.24) * (1.0 - disc(dist(0.6, 0.4), 0.21));
            let stars: f32 = [
                (0.18, 0.2),
                (0.82, 0.16),
                (0.24, 0.78),
                (0.8, 0.72),
                (0.12, 0.5),
            ]
            .iter()
            .map(|(x, y)| disc(dist(*x, *y), 0.012))
            .sum();
            mix(base, c, (moon + stars).min(1.0))
        }
        Motif::Ridges => {
            // Dunes, one in front of another, under a low sun: each ridge is
            // nearer and darker than the one behind it.
            let mut px = mix(c, mix(b, c, 0.5), v * 1.4);
            px = mix(px, mix(c, [255.0; 3], 0.4), disc(dist(0.7, 0.3), 0.09));
            for k in 0..4 {
                let k = k as f32;
                let crest = 0.42
                    + 0.14 * k
                    + 0.05 * (u * TAU * (0.7 + 0.2 * k) + k * 2.1).sin()
                    + 0.02 * (u * TAU * 2.3 + k).sin();
                px = mix(px, mix(b, a, k / 3.0), smooth(crest - aa, crest + aa, v));
            }
            px
        }
        Motif::Quarters => {
            // Tiles of quarter circles, each turned its own way, the way a
            // floor of them is laid.
            let n = 4.0;
            let (cx, cy) = ((u * n).floor(), (v * n).floor());
            let (fx, fy) = (u * n - cx, v * n - cy);
            let (ox, oy) = match (cx as u32 * 3 + cy as u32 * 5) % 4 {
                0 => (0.0, 0.0),
                1 => (1.0, 0.0),
                2 => (1.0, 1.0),
                _ => (0.0, 1.0),
            };
            let d = ((fx - ox).powi(2) + (fy - oy).powi(2)).sqrt();
            let ground = if ((cx + cy) as u32).is_multiple_of(2) {
                a
            } else {
                b
            };
            let edge = n * aa;
            let quarter = 1.0 - smooth(0.92 - edge, 0.92 + edge, d);
            let inner = 1.0 - smooth(0.46 - edge, 0.46 + edge, d);
            mix(mix(ground, c, quarter), ground, inner)
        }
        Motif::Stripes => {
            let s = (u * TAU * 6.0 + 2.2 * (u * TAU * 1.1).sin()).sin();
            let base = mix(a, b, smooth(-0.08, 0.08, s));
            let bar = smooth(0.68 - aa, 0.68 + aa, v) * (1.0 - smooth(0.72 - aa, 0.72 + aa, v));
            mix(base, c, bar)
        }
    }
}

fn paint(album: &Album) -> Vec<u8> {
    let [a, b, c] = album
        .colors
        .map(|c| [c[0] as f32, c[1] as f32, c[2] as f32]);
    let img = image::RgbImage::from_fn(SIDE, SIDE, |x, y| {
        let px = shade(
            album.motif,
            x as f32 / SIDE as f32,
            y as f32 / SIDE as f32,
            a,
            b,
            c,
        );
        image::Rgb(px.map(|f| f.round().clamp(0.0, 255.0) as u8))
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .expect("a PNG");
    out.into_inner()
}

/// A name for a cover on disk: its place in the library and its title.
fn file_name(i: usize, album: &Album) -> String {
    let slug: String = album
        .title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("{:02}-{slug}.png", i + 1)
}

// ---------------------------------------------------------------------------
// LSDP, for a demo on a TEST-NET address that wants to be found rather than
// typed in. See the note at the top.

fn announcement(name: &str, model: &str, addr: Ipv4Addr, port: u16) -> Vec<u8> {
    let mut msg = vec![0u8, b'A', 6, 0x02, 0, 0, 0, 0, addr.octets()[3], 4];
    msg.extend(addr.octets());
    msg.extend([1, 0, 1]);
    let port = port.to_string();
    let txt = [
        ("name", name),
        ("port", port.as_str()),
        ("model", model),
        ("version", "4.16.22"),
    ];
    msg.push(txt.len() as u8);
    for (k, v) in txt {
        msg.push(k.len() as u8);
        msg.extend(k.bytes());
        msg.push(v.len() as u8);
        msg.extend(v.bytes());
    }
    msg[0] = msg.len() as u8;
    let mut packet = vec![6, b'L', b'S', b'D', b'P', 1];
    packet.extend(msg);
    packet
}

// ---------------------------------------------------------------------------

struct Options {
    at: Vec<Ipv4Addr>,
    port: u16,
    announce: Option<Ipv4Addr>,
    still: bool,
    covers: Option<PathBuf>,
}

const USAGE: &str = "usage: demo-player [--at 127.0.0.2]... [--port 11000] [--announce 192.0.2.255] [--still] [--covers DIR]";

fn refuse(why: &str) -> ! {
    eprintln!("demo-player: {why}\n{USAGE}");
    std::process::exit(2);
}

fn options() -> Options {
    let mut options = Options {
        at: Vec::new(),
        port: 11000,
        announce: None,
        still: false,
        covers: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .unwrap_or_else(|| refuse(&format!("{arg} takes a value")))
        };
        let address = |v: String| -> Ipv4Addr {
            v.parse()
                .unwrap_or_else(|_| refuse(&format!("{arg} takes an IPv4 address, not {v}")))
        };
        match arg.as_str() {
            "--at" => options.at.push(address(value())),
            "--announce" => options.announce = Some(address(value())),
            // Not zero, which would have each room bound to a port of the
            // kernel's choosing while every document and announcement still
            // named the zero asked for: the group and the rooms found by LSDP
            // would point at a port nothing listens on.
            "--port" => {
                let v = value();
                options.port = match v.parse() {
                    Ok(0) | Err(_) => {
                        refuse(&format!("--port takes a port from 1 to 65535, not {v}"))
                    }
                    Ok(port) => port,
                };
            }
            "--covers" => options.covers = Some(PathBuf::from(value())),
            "--still" => options.still = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => refuse(&format!("unknown argument {other}")),
        }
    }
    options
}

/// One address per room: those given, then on from the last of them.
fn addresses(given: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    if given.len() > ROOMS.len() {
        refuse(&format!(
            "there are {} rooms and {} addresses",
            ROOMS.len(),
            given.len()
        ));
    }
    let mut addrs = given.to_vec();
    if addrs.is_empty() {
        addrs.push(Ipv4Addr::new(127, 0, 0, 2));
    }
    while addrs.len() < ROOMS.len() {
        let last = u32::from(addrs[addrs.len() - 1]);
        addrs.push(Ipv4Addr::from(last + 1));
    }
    addrs
}

fn main() {
    let options = options();

    let started = Instant::now();
    let covers: Vec<Vec<u8>> = LIBRARY.iter().map(paint).collect();
    eprintln!("painted {} covers in {:?}", covers.len(), started.elapsed());

    if let Some(dir) = &options.covers {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("creating {}: {e}", dir.display()));
        for (i, (album, png)) in LIBRARY.iter().zip(&covers).enumerate() {
            let path = dir.join(file_name(i, album));
            std::fs::write(&path, png)
                .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
            println!("{}\t{} — {}", path.display(), album.artist, album.title);
        }
        return;
    }

    let addrs = addresses(&options.at);
    let port = options.port;
    let covers = Arc::new(covers);
    let world = Arc::new(Mutex::new(World {
        rooms: ROOMS
            .iter()
            .map(|r| Room {
                album: r.album,
                track: r.track,
                at: r.at,
                since: Instant::now(),
                playing: r.playing,
                volume: r.volume,
                muted: false,
                leader: r.follows,
            })
            .collect(),
        addrs: addrs.clone(),
        port,
        generation: 1,
        still: options.still,
        answered: 0,
    }));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async move {
        let mut players = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            // Taken, most likely, or not an address this machine has: worth a
            // line saying which, not a panic and a backtrace.
            let at = SocketAddr::new(IpAddr::V4(*addr), port);
            let player = match Player::bind(at, Vec::new()).await {
                Ok(player) => player,
                Err(e) => {
                    eprintln!("demo-player: binding {addr}:{port}: {e}");
                    std::process::exit(1);
                }
            };
            serve(&player, i, world.clone(), covers.clone());
            if options.still {
                player.delay("/Status", still_status());
            }
            eprintln!("{} at {}", ROOMS[i].name, player.address());
            players.push(player);
        }
        if options.still {
            eprintln!("still: every clock stands where it started");
        }

        if let Some(target) = options.announce {
            let addrs = addrs.clone();
            std::thread::spawn(move || {
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("a UDP socket");
                socket.set_broadcast(true).expect("broadcast");
                loop {
                    for (i, addr) in addrs.iter().enumerate() {
                        let packet = announcement(ROOMS[i].name, "DEMO", *addr, port);
                        if let Err(e) = socket.send_to(&packet, (target, bluos::lsdp::PORT)) {
                            eprintln!("announce to {target}: {e}");
                        }
                    }
                    std::thread::sleep(Duration::from_secs(3));
                }
            });
            eprintln!("announcing to {target}:{}", bluos::lsdp::PORT);
        }

        // What a script waiting on the demo looks for: every room is bound
        // and answering by the time this is written.
        eprintln!("ready");
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

fn serve(player: &Player, i: usize, world: Arc<Mutex<World>>, covers: Arc<Vec<Vec<u8>>>) {
    let w = world.clone();
    player.handle("/SyncStatus", move |_| {
        Some(sync_status(&w.lock().unwrap(), i))
    });
    let w = world.clone();
    player.handle("/Status", move |_| {
        let mut w = w.lock().unwrap();
        w.answered = w.answered.wrapping_add(1);
        Some(status(&w, i))
    });
    let w = world.clone();
    player.handle("/Playlist", move |r| {
        Some(playlist(&w.lock().unwrap(), i, r))
    });
    let w = world.clone();
    player.handle("/ui/Queue", move |_| {
        Some(queue_screen(&w.lock().unwrap(), i))
    });
    player.serve("/ui/Configuration", CONFIGURATION);
    player.serve("/ui/Sources", SOURCES);
    player.serve("/ui/Home", home());
    player.serve("/ui/browseMenuGroup", library());
    player.handle("/ui/browseGrouped", |r| {
        Some(grouped(r.param("type").unwrap_or("Album")))
    });
    player.handle("/ui/Search", |r| Some(search(r)));
    let w = world.clone();
    player.handle("/ui/browseContext", move |r| {
        let album: usize = r
            .param("album")?
            .parse()
            .ok()
            .filter(|a| *a < LIBRARY.len())?;
        match r.param("type") {
            Some("Artist") => Some(artist_screen(album)),
            _ => {
                let w = w.lock().unwrap();
                let (playing, _, _) = w.position(i);
                Some(album_screen(album, Some(playing)))
            }
        }
    });
    player.handle_bytes("/Artwork", move |r| {
        let (artist, title) = (dec(r.param("artist")?), dec(r.param("album")?));
        let at = LIBRARY
            .iter()
            .position(|a| a.artist == artist && a.title == title)?;
        Some(("image/png".to_owned(), covers[at].clone()))
    });
    player.handle("/upgrade", |_| {
        Some(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><upgrade inProgress="false" version="4.16.22" available="false">check</upgrade>"#.to_owned())
    });

    // Commands. Enough of each to look right afterwards; none is thorough.
    let w = world.clone();
    player.handle("/Pause", move |r| {
        let mut w = w.lock().unwrap();
        let s = w.source(i);
        w.settle(s);
        let room = &mut w.rooms[s];
        room.playing = if r.param("toggle") == Some("1") {
            !room.playing
        } else {
            false
        };
        Some(format!(
            "<state>{}</state>",
            if room.playing { "play" } else { "pause" }
        ))
    });
    let w = world.clone();
    player.handle("/Play", move |r| {
        let mut w = w.lock().unwrap();
        let s = w.source(i);
        w.settle(s);
        let (album, _, _) = w.position(s);
        let room = &mut w.rooms[s];
        if let Some(secs) = r.param("seek").and_then(|v| v.parse().ok()) {
            room.at = secs;
        }
        if let Some(id) = r.param("id").and_then(|v| v.parse::<usize>().ok()) {
            let songs = queue_songs(album);
            if let Some((a, t)) = songs.get(id) {
                room.album = *a;
                room.track = *t;
                room.at = 0;
            }
        }
        room.playing = true;
        Some("<state>play</state>".to_owned())
    });
    for (path, step) in [("/Skip", 1isize), ("/Back", -1)] {
        let w = world.clone();
        player.handle(path, move |_| {
            let mut w = w.lock().unwrap();
            let s = w.source(i);
            w.settle(s);
            let room = &mut w.rooms[s];
            let n = LIBRARY[room.album].tracks.len() as isize;
            room.track = ((room.track as isize + step).rem_euclid(n)) as usize;
            room.at = 0;
            Some(format!("<id>{}</id>", room.track))
        });
    }
    let w = world.clone();
    player.handle("/ui/prf", move |r| {
        let inner = dec(r.param("u")?);
        let query = inner.split_once('?')?.1;
        let get = |k: &str| {
            query
                .split('&')
                .filter_map(|p| p.split_once('='))
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_owned())
        };
        let album: usize = get("album")?.parse().ok().filter(|a| *a < LIBRARY.len())?;
        let track: usize = get("track").and_then(|t| t.parse().ok()).unwrap_or(0);
        let mut w = w.lock().unwrap();
        let s = w.source(i);
        let room = &mut w.rooms[s];
        room.album = album;
        room.track = track.min(LIBRARY[album].tracks.len() - 1);
        room.at = 0;
        room.since = Instant::now();
        room.playing = true;
        w.generation += 1;
        Some(format!(
            "<addsong length=\"{}\"/>",
            LIBRARY[album].tracks.len()
        ))
    });
    let w = world.clone();
    player.handle("/Volume", move |r| {
        let mut w = w.lock().unwrap();
        let room = &mut w.rooms[i];
        if let Some(level) = r.param("level").and_then(|v| v.parse::<u8>().ok()) {
            room.volume = level.min(100);
        }
        if let Some(m) = r.param("mute") {
            room.muted = m == "1";
        }
        let (vol, mute) = (room.volume, u8::from(room.muted));
        w.generation += 1;
        Some(format!(
            "<volume db=\"-30\" mute=\"{mute}\" etag=\"v{}\">{vol}</volume>",
            w.generation
        ))
    });
    for (path, join) in [("/AddSlave", true), ("/RemoveSlave", false)] {
        let w = world.clone();
        player.handle(path, move |r| {
            let mut w = w.lock().unwrap();
            let j = w.room_at(r.param("slave")?)?;
            if j != i {
                w.rooms[j].leader = join.then_some(i);
                w.generation += 1;
            }
            Some("<addSlave/>".to_owned())
        });
    }
    for path in [
        "/Shuffle", "/Repeat", "/Clear", "/Delete", "/Move", "/Save", "/Sleep", "/Stop",
    ] {
        player.handle(path, |_| Some("<ok/>".to_owned()));
    }
}
