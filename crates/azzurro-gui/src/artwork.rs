//! Cover art: fetch, decode, scale, cache.
//!
//! Slint has no image element that loads a URL, so all of this is the app's
//! job. That is the one real cost of the toolkit for an app like this, and it
//! is bounded — this module is the whole of it.
//!
//! Three layers, cheapest first: an in-memory cache of already-decoded pixels
//! at the size they will be drawn, a cache on disk of the bytes as fetched, and
//! finally the network. Players serve their own art over plain HTTP; anything
//! that came from a streaming service is an absolute https URL at that
//! service's CDN, which is the only reason this app links a TLS stack at all.
//!
//! Note the return type. [`slint::Image`] holds a non-atomic refcount and is
//! therefore **not `Send`**, so it cannot be built here and carried to the UI
//! thread. `SharedPixelBuffer` is `Send`, so decoded pixels travel and the
//! `Image` is made on the far side, inside the event loop.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use tokio::sync::Semaphore;

/// Decoded art, ready for `slint::Image::from_rgba8`.
pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// How many decoded images to keep. A screenful of queue thumbnails plus the
/// covers of everything recently selected, with room to spare.
///
/// Counted in entries rather than bytes, so what it costs depends on which
/// sizes are in it. Three tiers are asked for and no more: `THUMB_SIZE` 80,
/// `TILE_SIZE` 232 and `COVER_SIZE` 720, and one LRU holds all of them. A
/// fourth crept in for a while — the sidebar card fetched its own 80×80 beside
/// the queue's 72, so the selected player's cover was decoded twice and held
/// twice for a difference nothing can see. The two are one tier at 80 now,
/// which is what both of them want at 2x: keeping this table true is what
/// makes a fourth tier visible if one is ever added deliberately. At four
/// bytes a pixel the worst case for a full cache of each is
///
/// | tier | one entry | 256 entries |
/// |------|-----------|-------------|
/// | 80   | 25 KiB    | 6 MiB       |
/// | 232  | 210 KiB   | 53 MiB      |
/// | 720  | 2 MiB     | 506 MiB     |
///
/// so a realistic mix — mostly thumbnails and shelf tiles, a few covers — sits
/// in the tens of megabytes. The 720 row needs 256 *different albums* opened
/// at full size to be reached and is by far the least likely of the three; the
/// 232 row is the one a few minutes of browsing actually approaches.
///
/// That top row grew fourfold when the now-playing sleeve went from 360 to
/// 720, which is the honest cost of the hero being sharp rather than upscaled.
/// If it ever bites, the fix is a small separate cache for the cover tier
/// rather than shrinking the picture the app is arranged around.
///
/// Nothing here is unbounded and no player can push past it: the entry count
/// is fixed and each entry is capped by the size that was asked for. Whether
/// tens of megabytes of decoded pixels is the right price for not re-decoding
/// is a separate question from recording what the price is.
const MEMORY_CACHE: usize = 256;

/// Fetches allowed at once. Enough to fill a list quickly, few enough that
/// opening a long queue does not put thirty connections into a speaker that is
/// also trying to play music.
const CONCURRENT_FETCHES: usize = 4;

/// Decodes allowed at once.
///
/// [`CONCURRENT_FETCHES`] bounds connections and nothing else. A cover already
/// on disk never touches that permit — it is read and handed straight to
/// `spawn_blocking` — so the work this actually costs was bounded by how many
/// covers something asked for at once, which is a number a queue chooses and
/// not one this app does. A window of a hundred rows, all cached, was a hundred
/// simultaneous decodes, each allowed [`MAX_DECODED`] to itself.
///
/// Three, because a decode is tens of milliseconds and the list still fills
/// faster than the eye follows, and because the worst case is then three
/// decoder allocations rather than as many as the queue is long.
const CONCURRENT_DECODES: usize = 3;

/// Files kept on disk. Pruned to this on startup and every [`PRUNE_EVERY`]
/// covers after that, oldest first.
///
/// Startup alone was not enough: a session left open for a week fetches art
/// the whole time and nothing between one launch and the next ever looks at
/// the directory, so the limit held only for as long as the app had just
/// started.
const DISK_CACHE_FILES: usize = 512;

/// And how much they may come to, whatever the count.
///
/// The file limit alone bounds the directory only if the files are the size a
/// cover is. Each one may be up to [`MAX_BYTES`], so 512 of them is nearly
/// five gigabytes — a ceiling set by what a player chose to serve rather than
/// by anything this app decided. Real covers are tens of kilobytes, so this is
/// far above a full cache of them and far below what the count alone allows.
const DISK_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// How many covers may be written before the directory is checked again.
///
/// Often enough that it cannot run far past the limit, rarely enough that the
/// cost — one directory listing and a sort — is nothing against the hundred
/// fetches that earned it.
const PRUNE_EVERY: usize = 100;

/// Refuse anything implausible for a piece of cover art rather than decoding
/// it. Guards against a redirect to something that is not an image at all.
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Whether these bytes begin like an image this build can read.
///
/// A player answers an artwork request it cannot satisfy with `200 OK` and an
/// XML error document — observed on a Powernode, where every `/Artwork` for a
/// capture input comes back as `application/xml`. Those bytes were written
/// into the disk cache, and the read path accepts any file that is not empty,
/// so one such reply meant that cover was permanently undecodable: the cache
/// answered before the network could ever be tried again.
///
/// Only the three formats the workspace compiles are recognized; anything else
/// would be undecodable regardless.
fn looks_like_an_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G'])
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

/// How many redirects a cover may take before it is not worth following.
///
/// The same figure the protocol client uses, and for the same reason: a chain
/// longer than this is not a CDN doing its job.
pub const MAX_HOPS: usize = 5;

/// The player addresses art may be fetched from, shared with the HTTP client.
///
/// Shared because the redirect policy needs the same answer this does, and it
/// is built with the client before there is an [`Artwork`] to ask. A player
/// answers `/Artwork` with a 301 to its own library service on another port —
/// 11004 on a Powernode — so a policy that only knew the public-address rule
/// stopped every cover the player served.
pub type Players = std::sync::Arc<Mutex<std::collections::HashSet<std::net::IpAddr>>>;

/// How many addresses may be allowed at once. See [`Artwork::remember_player`].
///
/// The same figure the registry caps itself at, and for the same reason: far
/// above any real system — the largest BluOS install is a few dozen zones — so
/// the only thing this can turn away is a flood.
const MAX_ART_HOSTS: usize = 256;

/// Whether art may be fetched from `url`, given the players adopted so far.
///
/// [`permitted`] alone is the rule for somewhere this app has never spoken to;
/// this adds the players themselves, whose own addresses are exactly the
/// private space that rule refuses.
pub fn allowed_by(players: &Players, url: &reqwest::Url) -> bool {
    if permitted(url) {
        return true;
    }
    match url.host_str().and_then(literal_address) {
        Some(addr) => players.lock().unwrap().contains(&addr),
        None => false,
    }
}

/// Whether art may be fetched from this URL.
///
/// A player names the address cover art is fetched from, and an adopted player
/// is only ever as trustworthy as the subnet it announced itself on. Naming
/// `http://192.168.1.1/…` or a loopback port would have this app issue that
/// request from inside the network, where the caller cannot reach — a request
/// whose body it never sees, but whose side effects still happen.
///
/// So: the player's own art is fine, a CDN's is fine, and an address in
/// private, loopback or link-local space that is *not* a player is not. The
/// host is only inspected when it is written as an address; a name is left to
/// the resolver, which is the gap in this — a name that resolves into private
/// space still passes. Closing that needs a connect-time hook rather than a
/// check here, and the hop limit above bounds how far a redirect can walk
/// toward one.
pub fn permitted(url: &reqwest::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }

    let Some(host) = url.host_str() else {
        return false;
    };

    // A name, which this does not resolve. See the note above.
    let Some(addr) = literal_address(host) else {
        return true;
    };

    !(addr.is_loopback() || addr.is_unspecified() || is_private(addr))
}

/// The host as an address, if it is written as one rather than as a name.
///
/// A URL writes IPv6 in brackets — `http://[::1]/x` — and those are not part
/// of the address.
///
/// Canonical, so an IPv4 address written as IPv6 is judged as the IPv4 address
/// it is. `http://[::ffff:127.0.0.1]:631/` dials the loopback on a dual-stack
/// socket, but as an `Ipv6Addr` it is neither `::1` nor in either private
/// prefix, so every check below passed it.
fn literal_address(host: &str) -> Option<std::net::IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .ok()
        .map(|addr| addr.to_canonical())
}

/// Address space that is reachable from this machine but not from the internet.
///
/// 100.64.0.0/10 is carrier-grade NAT's shared space, and it is also where a
/// tailnet puts every machine on it — so an address there can be someone's
/// other computer, reached through this one. A player on a tailnet is still
/// allowed its own art, by being adopted.
fn is_private(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || (a == 100 && (b & 0xc0) == 64)
        }
        // `is_unique_local` and `is_unicast_link_local` are still unstable, so
        // the prefixes are written out: fc00::/7 and fe80::/10.
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80
        }
    }
}

/// How long one cover may take to arrive.
///
/// Without it a CDN that accepts the connection and then says nothing holds
/// one of the [`CONCURRENT_FETCHES`] permits forever, and four such covers
/// stop artwork loading altogether for the rest of the session. Generous,
/// because this is a picture over someone else's network and not something
/// the interface waits on.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// How many URLs are remembered as undecodable. See [`Artwork::undecodable`].
///
/// Small on purpose: a cover that cannot be read is a rarity, and this only
/// has to outlive the redraws of the list it is in.
const UNDECODABLE_REMEMBERED: usize = 64;

/// And for how long. See [`Artwork::undecodable`].
///
/// Not for the session. The bytes that fail are not always the bytes the
/// player will always serve: a player whose library service is still coming up
/// answers `/Artwork` with its XML error document — the same reply
/// [`looks_like_an_image`] was written for — and one of those used to cost
/// that album its cover until the app was restarted, because the entry is
/// consulted before anything else and 64 slots are not turned over in a normal
/// session. Long enough that a cover which genuinely cannot be read is not
/// re-fetched on every queue read, short enough that a service coming up is
/// waited out rather than outlived.
const UNDECODABLE_FOR: Duration = Duration::from_secs(5 * 60);

/// How many trips one call will start for a URL.
///
/// More than one because the first may find a trip already running and be
/// left with nothing when that trip's owner is cancelled; see
/// [`Artwork::bytes`]. Bounded because a URL whose owner keeps being
/// cancelled is a queue somebody is scrolling, and the cover can wait for the
/// next read of it rather than being chased here.
const TRIPS_PER_CALL: usize = 2;

/// The largest cover this will decode, per side, and the most memory it may
/// take to do it.
///
/// [`MAX_BYTES`] bounds the bytes that arrive, which says nothing about what
/// they claim: a couple of hundred bytes of header can declare enormous
/// dimensions. The `image` crate's own default is not nothing — it caps a
/// single allocation at 512 MB — but it sets no bound on width or height at
/// all, so a long thin cover can pass it while still being absurd. These are
/// tighter on both counts.
///
/// Well above any real sleeve: the largest a streaming service serves is
/// around 3000 a side, and everything here is scaled to 360 or less
/// immediately afterwards.
const MAX_SIDE: u32 = 8192;
const MAX_DECODED: u64 = 128 * 1024 * 1024;

/// A decoded image is identified by where it came from and how big it was
/// wanted, since the same cover is drawn at three sizes — a queue thumbnail, a
/// shelf tile and a full cover — and each is scaled and stored separately.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    url: String,
    size: u32,
}

/// Where the bytes a decode is about to run on came from.
///
/// A decode that fails on bytes read from disk is worth one more try: the file
/// is dropped and the network asked. The same failure on bytes just fetched
/// means the cover itself cannot be read, and asking again would only fail
/// again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Disk,
    Network,
}

/// What one trip for a URL's bytes ends with: the bytes and where they came
/// from, or nothing.
///
/// Behind an `Arc` because every caller waiting on the same trip gets a copy
/// of the answer and none of them needs to own it.
type Fetched = Option<(Arc<Vec<u8>>, Source)>;

/// A trip already under way, for the callers that arrive while it runs.
///
/// `None` while it is still going, `Some` once it has ended — whether or not
/// it produced anything. A `watch` rather than a lock held across the request:
/// a waiter parks on the channel, and the one doing the work is not slowed by
/// how many of them there are.
type Running = tokio::sync::watch::Receiver<Option<Fetched>>;

/// Takes a URL off the in-flight map however its fetch ends.
///
/// A plain removal at the end is not enough. This task can be dropped
/// mid-request — the queue loader aborts its whole set when the user moves on
/// — and an entry left behind is one that every later caller joins and then
/// waits on for an answer nobody will ever send.
struct Fetching<'a> {
    inflight: &'a Mutex<HashMap<String, Running>>,
    url: &'a str,
}

impl Drop for Fetching<'_> {
    fn drop(&mut self) {
        self.inflight.lock().unwrap().remove(self.url);
    }
}

pub struct Artwork {
    http: reqwest::Client,
    memory: Mutex<LruCache<Key, Pixels>>,
    /// One color per image, computed once when it is decoded. Keyed by URL
    /// alone: the same cover gives the same color at any size.
    tints: Mutex<LruCache<String, [u8; 3]>>,
    /// URLs whose bytes would not decode, so they are not asked for again.
    ///
    /// The one re-fetch in [`Self::get`] bounds itself within a single call
    /// and leaves the disk exactly as it found it — the fresh copy passes the
    /// magic-byte check and is written back over the file that was just
    /// dropped. So the next call read, deleted, fetched and rewrote the same
    /// undecodable cover, and `load_thumbnails` runs on every queue read: one
    /// such cover was a request to the player and two decodes per track
    /// change, for ever. Remembering the failure is what makes the re-fetch
    /// the one extra attempt it was meant to be.
    ///
    /// Keyed by URL alone, like `tints`: whether bytes decode is a property of
    /// the bytes, not of the size they were wanted at — the size is only the
    /// box the decoded image is scaled into afterwards.
    ///
    /// The value is when the entry stops counting; see [`UNDECODABLE_FOR`] for
    /// why one reply is not allowed to settle a URL for the whole run.
    undecodable: Mutex<LruCache<String, Instant>>,
    /// `None` when there is nowhere to write, which is not fatal: the memory
    /// cache and the network still work.
    disk: Option<PathBuf>,
    /// Covers written since the process started. Every [`PRUNE_EVERY`] of them
    /// buys a look at the directory; see [`DISK_CACHE_FILES`].
    written: std::sync::atomic::AtomicUsize,
    /// Decoded images that have landed in `memory` since the process started.
    ///
    /// A number that only goes up, so anything drawing from the cache can ask
    /// "has anything at all decoded since I last looked?" for the price of one
    /// atomic load. The queue's memo needs that as a gate: what it used to hash
    /// was [`Self::cached`] per row, which is the memory lock taken once a
    /// track and answers a narrower question at five hundred times the cost.
    ///
    /// It is only a gate, because it counts every decode in the process — a
    /// browse thumbnail, another player's sidebar card — and a queue watching
    /// it alone is a queue rebuilt several times a second for covers that are
    /// not its own. What arrived is asked separately, of the URLs the asker
    /// actually named, in [`Self::any_decoded`].
    arrivals: std::sync::atomic::AtomicU64,
    /// And how many rows have asked [`Self::cached`] what is decoded.
    ///
    /// The other side of the same story: every one of those is a turn of the
    /// memory lock, taken while whoever is drawing holds a lock of its own.
    /// The queue's memo is written so that this grows by the length of the
    /// window when the window has changed and by nothing when it has not, and
    /// a test watches the number to say so.
    peeks: std::sync::atomic::AtomicU64,
    limit: Semaphore,
    /// And the same for decoding; see [`CONCURRENT_DECODES`].
    ///
    /// Shared rather than owned by the struct so a permit can outlive the
    /// future that took it: the decode runs on the blocking pool and a
    /// blocking task cannot be cancelled, so a permit released when its caller
    /// is aborted is a permit released while the work it bounds is still
    /// running. See where it is taken.
    decoding: Arc<Semaphore>,
    /// URLs being read or fetched right now, so the same cover wanted at two
    /// sizes costs one trip rather than two.
    ///
    /// The queue asks for a 72px thumbnail and the sleeve for a 720px cover of
    /// the same track at the same instant, and each size is a separate entry in
    /// the memory cache — so both missed, both opened a connection, both read
    /// the same bytes and both wrote the same file. The sizes still decode
    /// separately, which is what they are for; only the bytes are shared.
    inflight: Mutex<HashMap<String, Running>>,
    /// The addresses of players this app has adopted.
    ///
    /// A player serves its own cover art from its own address, which is on the
    /// user's subnet and so is exactly the private space [`permitted`] refuses
    /// by default. Recording the players separates "this speaker's own art"
    /// from "some other machine on the LAN a player pointed us at", which is
    /// the whole distinction that check exists to make.
    players: Players,
}

impl Artwork {
    pub fn new(http: reqwest::Client, players: Players) -> Self {
        let disk = dirs::cache_dir().map(|d| d.join("azzurro").join("artwork"));

        // `new` is called once, before the window opens, so this one is
        // deliberately synchronous: there is nothing yet to block.
        if let Some(dir) = &disk {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::debug!("no artwork cache on disk: {e}");
            } else {
                prune(dir);
            }
        }

        Self {
            http,
            written: std::sync::atomic::AtomicUsize::new(0),
            arrivals: std::sync::atomic::AtomicU64::new(0),
            peeks: std::sync::atomic::AtomicU64::new(0),
            players,
            memory: Mutex::new(LruCache::new(
                NonZeroUsize::new(MEMORY_CACHE).expect("MEMORY_CACHE is not zero"),
            )),
            tints: Mutex::new(LruCache::new(
                NonZeroUsize::new(MEMORY_CACHE).expect("MEMORY_CACHE is not zero"),
            )),
            undecodable: Mutex::new(LruCache::new(
                NonZeroUsize::new(UNDECODABLE_REMEMBERED)
                    .expect("UNDECODABLE_REMEMBERED is not zero"),
            )),
            disk: disk.filter(|d| d.is_dir()),
            limit: Semaphore::new(CONCURRENT_FETCHES),
            decoding: Arc::new(Semaphore::new(CONCURRENT_DECODES)),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// One that keeps nothing on disk, for a test.
    ///
    /// `new` creates and prunes the user's own cache directory, which a test
    /// has no business touching.
    #[cfg(test)]
    pub fn in_memory(http: reqwest::Client, players: Players) -> Self {
        Self {
            http,
            written: std::sync::atomic::AtomicUsize::new(0),
            arrivals: std::sync::atomic::AtomicU64::new(0),
            peeks: std::sync::atomic::AtomicU64::new(0),
            players,
            memory: Mutex::new(LruCache::new(
                NonZeroUsize::new(MEMORY_CACHE).expect("MEMORY_CACHE is not zero"),
            )),
            tints: Mutex::new(LruCache::new(
                NonZeroUsize::new(MEMORY_CACHE).expect("MEMORY_CACHE is not zero"),
            )),
            undecodable: Mutex::new(LruCache::new(
                NonZeroUsize::new(UNDECODABLE_REMEMBERED)
                    .expect("UNDECODABLE_REMEMBERED is not zero"),
            )),
            disk: None,
            limit: Semaphore::new(CONCURRENT_FETCHES),
            decoding: Arc::new(Semaphore::new(CONCURRENT_DECODES)),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// What is already decoded, without touching the disk or the network — and
    /// without counting as a use.
    ///
    /// This is what the queue view calls while building rows: it fills in the
    /// covers it has and leaves the rest blank, and a later republish picks up
    /// whatever arrived in the meantime.
    ///
    /// `peek` and not `get`: this is called while drawing, and drawing every
    /// row four hundred milliseconds apart would otherwise walk the whole
    /// visible list to the front of the LRU on each pass. What is on screen
    /// then looks more recently used than what was actually asked for, and the
    /// eviction order stops meaning anything. Wanting a cover is `get`, in
    /// [`Self::get`]; drawing one is this.
    pub fn cached(&self, url: &str, size: u32) -> Option<Pixels> {
        self.peeks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = Key {
            url: url.to_owned(),
            size,
        };
        self.memory.lock().unwrap().peek(&key).cloned()
    }

    /// Whether any of `urls` is decoded at `size`, in one turn of the lock.
    ///
    /// The question [`Self::arrivals`] cannot answer: that counter says
    /// something decoded, this says whether it was one of the covers the
    /// caller is waiting on. Asked only once a rebuild is otherwise in
    /// prospect, so the walk is over the URLs a list still lacks rather than
    /// over the whole of it.
    ///
    /// Deliberately not counted in [`Self::peeks`], which measures what the
    /// rows cost: this is one turn of the lock for the whole list and clones
    /// nothing. `peek` rather than `get`, for the reason given on
    /// [`Self::cached`] — asking is not using.
    pub fn any_decoded(&self, urls: &[String], size: u32) -> bool {
        // One key, refilled, rather than one allocated per URL: a queue that
        // is still waiting asks this every time anything anywhere decodes.
        let mut key = Key {
            url: String::new(),
            size,
        };
        let memory = self.memory.lock().unwrap();
        urls.iter().any(|url| {
            key.url.clear();
            key.url.push_str(url);
            memory.contains(&key)
        })
    }

    /// Put a decoded image in as though one had been fetched and decoded.
    ///
    /// For a test of a memo that watches the cache. Nothing in this build can
    /// stage a real arrival: the fake player answers `/Artwork` with a 404,
    /// and a picture it could answer with is bytes rather than the text its
    /// routes are written in. The pixels are a single blank one, because what
    /// is under test is which URLs moved and not what they look like.
    #[cfg(test)]
    pub fn note_arrival(&self, url: &str, size: u32) {
        self.memory.lock().unwrap().put(
            Key {
                url: url.to_owned(),
                size,
            },
            Pixels::new(1, 1),
        );
        // After the entry is in, exactly as in `get`: a memo that sees the new
        // number goes looking for the picture it is announcing.
        self.arrivals
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// How many times that has been asked. See [`Self::peeks`].
    ///
    /// Only a test reads it, the same as [`Self::in_memory`]: the count is
    /// kept either way, because an atomic add beside a mutex is not worth a
    /// `cfg` of its own.
    #[cfg(test)]
    pub fn peeks(&self) -> u64 {
        self.peeks.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many decoded images have landed since the process started.
    ///
    /// For a memo that draws from the cache: the number standing still is a
    /// one-load proof that a list of rows built out of [`Self::cached`] would
    /// come out byte for byte the same, so it is the gate in front of the
    /// narrower question. The number moving proves nothing on its own — see
    /// [`Self::arrivals`] and [`Self::any_decoded`].
    pub fn arrivals(&self) -> u64 {
        self.arrivals.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Note that this player's own address serves art that is allowed.
    ///
    /// Called as players are adopted, and bounded here rather than by the
    /// caller. The registry's own cap used to be given as the bound, which
    /// stopped being true the moment a player was allowed to move: retiring
    /// the address a player has left is what makes room for the one it is
    /// announcing from, so an announcement replaying one node id from a fresh
    /// address each time kept the registry at its old size while adding an
    /// entry here every time. LSDP is an unauthenticated broadcast, which
    /// makes that a set grown by whoever is on the segment — and every address
    /// in it is permanently exempt from [`permitted`]'s refusal to dial
    /// private space, the one check that stops a player aiming this app's
    /// fetches at the LAN. `Backend::moved_here` drops the address it
    /// retires; this is the ceiling for whatever no retirement covers.
    ///
    /// Full is refused rather than evicted. An eviction would quietly take the
    /// art away from a player the user is looking at, where a refusal only
    /// declines the address that arrived last — and a household past this many
    /// players is a flood rather than a house.
    pub fn remember_player(&self, host: std::net::IpAddr) {
        // Canonical for the same reason [`literal_address`] is: the two are
        // compared, and a player discovered as `::ffff:10.0.0.155` is the
        // player whose art is at `10.0.0.155`.
        let host = host.to_canonical();
        let mut players = self.players.lock().unwrap();
        if players.len() >= MAX_ART_HOSTS && !players.contains(&host) {
            tracing::warn!(
                %host,
                "not allowing art from another address: already allowing {MAX_ART_HOSTS}"
            );
            return;
        }
        players.insert(host);
    }

    /// And forget it, once nothing is tracked at that address any more.
    ///
    /// The other half of the bound above: an address a player has left is not
    /// a player's own address, and leaving it in the set kept it fetchable for
    /// the rest of the run. Called only where the registry says no entry
    /// remains there, since one device can present two zones on one host.
    pub fn forget_player(&self, host: std::net::IpAddr) {
        self.players.lock().unwrap().remove(&host.to_canonical());
    }

    /// The cover for `url` as the desktop should be told of it, if at all.
    ///
    /// MPRIS hands `mpris:artUrl` to the shell, which fetches it itself — with
    /// none of the checks this module makes, and without the redirect policy
    /// on this module's client. So the copy already on disk is preferred: a
    /// `file://` URL costs the shell no request and gives a player nothing to
    /// aim. Failing that, the URL is offered only where this app would fetch it
    /// too, and a cover this app would refuse is not exported at all.
    ///
    /// Only from inside the tokio runtime: the disk is read on its blocking
    /// pool. The D-Bus methods run on zbus's own executor thread, where there
    /// is no runtime and asking for one panics — so they use
    /// [`Self::offered`] instead.
    ///
    /// Compiled on Linux and under test only. MPRIS is the one thing that hands
    /// a URL to a desktop and it is Linux-only, so elsewhere this is a method
    /// nobody calls — which CI, running clippy with `-D warnings`, refuses. The
    /// policy it applies is worth checking on every platform, so the tests keep
    /// it.
    #[cfg(any(target_os = "linux", test))]
    pub async fn for_desktop(&self, url: &str) -> Option<String> {
        if let Some(dir) = &self.disk {
            let path = dir.join(file_name(url));
            let found = tokio::task::spawn_blocking(move || {
                // The first bytes, not just the name: a reply written before
                // the image check existed is still an XML error document, and
                // pointing the shell at that would lose it a URL that works.
                use std::io::Read;
                let mut head = [0u8; 12];
                let read = std::fs::File::open(&path)
                    .and_then(|mut f| f.read(&mut head))
                    .ok()?;
                looks_like_an_image(&head[..read]).then_some(path)
            })
            .await
            .ok()
            .flatten();
            if let Some(url) = found.and_then(|path| reqwest::Url::from_file_path(path).ok()) {
                return Some(url.to_string());
            }
        }
        self.offered(url)
    }

    /// The URL itself, if this app would fetch it — the half of
    /// [`Self::for_desktop`] that needs no disk and no runtime.
    ///
    /// Linux and test only, for the reason above: its only caller is the MPRIS
    /// bridge.
    #[cfg(any(target_os = "linux", test))]
    pub fn offered(&self, url: &str) -> Option<String> {
        self.may_fetch(url).then(|| url.to_owned())
    }

    /// Whether art may be fetched from here, given the players adopted so far.
    fn may_fetch(&self, url: &str) -> bool {
        match reqwest::Url::parse(url) {
            Ok(url) => allowed_by(&self.players, &url),
            Err(_) => false,
        }
    }

    /// The color to tint a panel with behind this artwork, if it has been
    /// decoded. See [`dominant`].
    pub fn tint(&self, url: &str) -> Option<[u8; 3]> {
        self.tints.lock().unwrap().get(url).copied()
    }

    /// Decoded art at `size`, fetching and decoding if it is not already held.
    ///
    /// `size` bounds both dimensions; the aspect ratio is kept, so art that is
    /// not square comes back smaller in one axis rather than distorted.
    pub async fn get(&self, url: &str, size: u32) -> Option<Pixels> {
        if url.is_empty() {
            return None;
        }
        if let Some(hit) = self.cached(url, size) {
            return Some(hit);
        }
        // Asked once and answered for a while. `get` is not the rare call it
        // reads like — a queue read calls it for every distinct cover, and a
        // browse page for every row — so a cover this build cannot read has to
        // stop costing anything after the one attempt below. `get` rather than
        // `peek`, unlike [`Self::cached`]: this is a cover somebody still
        // wants drawn, so it should stay remembered for as long as it is
        // being asked for.
        //
        // For a while and not for good: the entry is the memory of one reply,
        // and a reply can be a player having a bad minute rather than a cover
        // that is not a picture. See [`UNDECODABLE_FOR`].
        if self
            .undecodable
            .lock()
            .unwrap()
            .get(url)
            .is_some_and(|until| *until > Instant::now())
        {
            return None;
        }

        // At most one second attempt, and only for bytes that came off the
        // disk. A file that begins like an image and then will not decode —
        // truncated by a crash before the write became a rename, or a format
        // this build was not compiled for — passed the magic-byte check and
        // failed the decode silently, every time, for as long as the file
        // survived pruning. That cover was blank until then. Dropping it and
        // asking the network once costs one request and ends it.
        let mut refetched = false;
        let decoded = loop {
            let (bytes, from) = self.bytes(url).await?;

            // Decoding a large JPEG is tens of milliseconds and would otherwise
            // occupy an async worker doing nothing else. The permit is held
            // across it, unlike the fetch permit: see [`CONCURRENT_DECODES`].
            //
            // Owned, and moved into the closure rather than kept in a local
            // here, so that it is dropped when the decode returns and not when
            // this future does. The two are not the same moment: a walk being
            // aborted drops this future at the await below, and the blocking
            // task carries on because a blocking task has no cancellation
            // point. Held in a local, the permit went back at the abort and
            // the walk that replaced this one took a fresh one straight away,
            // so the covers actually being decoded at once were three plus
            // however many orphans were still going — which is no bound at
            // all, and worst on exactly the player whose covers are large
            // enough for the limit to matter.
            let permit = Arc::clone(&self.decoding).acquire_owned().await.ok()?;
            let out = tokio::task::spawn_blocking(move || {
                let decoded = decode(&bytes, size);
                drop(permit);
                decoded
            })
            .await
            .ok()
            .flatten();

            match out {
                Some(decoded) => break decoded,
                None if from == Source::Disk && !refetched => {
                    refetched = true;
                    self.discard(url).await;
                }
                // Nothing more to try: these are the bytes the player serves
                // and they are not a picture. Written down rather than only
                // dropped, because dropping the file is undone by the fetch
                // that follows it — see [`Self::undecodable`].
                None => {
                    self.undecodable
                        .lock()
                        .unwrap()
                        .put(url.to_owned(), Instant::now() + UNDECODABLE_FOR);
                    return None;
                }
            }
        };

        if let Some(tint) = dominant(&decoded) {
            self.tints.lock().unwrap().put(url.to_owned(), tint);
        }
        self.memory.lock().unwrap().put(
            Key {
                url: url.to_owned(),
                size,
            },
            decoded.clone(),
        );
        // After the entry is in, so a memo that sees the new number and
        // rebuilds finds the picture the number is announcing.
        self.arrivals
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        Some(decoded)
    }

    /// Forget the cached file for `url`, so the next call fetches it again.
    async fn discard(&self, url: &str) {
        let Some(dir) = &self.disk else { return };
        let path = dir.join(file_name(url));
        tracing::debug!(url, "dropping a cached cover that would not decode");
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
    }

    /// The bytes as fetched, from disk if they are there — and shared, so one
    /// URL wanted at two sizes costs one trip rather than two.
    async fn bytes(&self, url: &str) -> Fetched {
        // Twice at most, because a trip can end without an answer: the task
        // that owned it was dropped. That is routine rather than exotic — the
        // queue loader aborts its whole set the moment a newer one takes the
        // generation — and the caller that had joined it is not a caller who
        // learned there is no art. Read as a miss, one aborted loader blanked
        // the sleeve and the sidebar card until the next track. So a waiter
        // left with nothing goes round and tries to become the one doing the
        // work; see [`TRIPS_PER_CALL`].
        for _ in 0..TRIPS_PER_CALL {
            // Either join the trip already running for this URL or become it,
            // decided under one lock so that two callers arriving together
            // cannot both decide they are the one doing the work.
            let turn = {
                let mut inflight = self.inflight.lock().unwrap();
                match inflight.get(url) {
                    Some(running) => Err(running.clone()),
                    None => {
                        let (tx, rx) = tokio::sync::watch::channel(None);
                        inflight.insert(url.to_owned(), rx);
                        Ok(tx)
                    }
                }
            };

            let answer = match turn {
                Ok(answer) => answer,
                Err(mut running) => {
                    // `None` is "still going", and a sender dropped while it
                    // still says so is a trip that was cancelled. The guard
                    // below has already taken the URL off the map by then, so
                    // going round makes this caller the new owner rather than
                    // a second waiter on a trip nobody is running.
                    while running.borrow().is_none() {
                        if running.changed().await.is_err() {
                            break;
                        }
                    }
                    let ended = running.borrow().clone();
                    match ended {
                        Some(out) => return out,
                        None => continue,
                    }
                }
            };

            // Off the map however this ends, cancellation included.
            let _slot = Fetching {
                inflight: &self.inflight,
                url,
            };

            let out = self.read_or_fetch(url).await;
            let _ = answer.send(Some(out.clone()));
            return out;
        }
        None
    }

    /// One trip for a URL's bytes: the disk first, then the network.
    async fn read_or_fetch(&self, url: &str) -> Fetched {
        let path = self.disk.as_ref().map(|dir| dir.join(file_name(url)));

        // Off the async workers. Reading a cover is a millisecond on a local
        // disk and rather more when the cache directory turns out to live on a
        // network mount, which is exactly the case that would otherwise stall a
        // runtime thread.
        if let Some(cache) = path.clone() {
            let reading = cache.clone();
            let cached = tokio::task::spawn_blocking(move || std::fs::read(reading).ok())
                .await
                .ok()
                .flatten();
            match cached.filter(|b| !b.is_empty()) {
                Some(bytes) if looks_like_an_image(&bytes) => {
                    return Some((Arc::new(bytes), Source::Disk));
                }
                // Written before the check above existed, or truncated by a
                // crash mid-write. Dropped rather than returned, so the fetch
                // below gets its turn instead of the cache answering wrongly
                // forever.
                Some(_) => {
                    tracing::debug!("dropping a cached cover that is not an image");
                    let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(cache)).await;
                }
                None => {}
            }
        }

        // The player chose this address, so it is checked before it is dialed
        // rather than trusted because a player said it. Redirects are checked
        // per hop by the policy on the client itself.
        if !self.may_fetch(url) {
            tracing::debug!(
                url,
                "artwork refused: not the player's own host, and not public"
            );
            return None;
        }

        // Held across the request, not the decode: the point is to bound
        // connections, and a permit kept through decoding would idle it.
        let _permit = self.limit.acquire().await.ok()?;

        let mut response = match self.http.get(url).timeout(FETCH_TIMEOUT).send().await {
            Ok(response) => response.error_for_status().ok()?,
            Err(e) => {
                tracing::debug!(url, "artwork fetch failed: {e}");
                return None;
            }
        };

        // Trust the declared length where there is one, so an implausible
        // cover costs nothing to refuse.
        if response
            .content_length()
            .is_some_and(|n| n as usize > MAX_BYTES)
        {
            tracing::debug!(url, "artwork is implausibly large; skipping");
            return None;
        }

        // Then read it a chunk at a time, checking as it goes. A chunked
        // response declares no length at all, so this is the check that
        // actually holds: asking for the whole body first and measuring it
        // afterwards means the oversized body is already in memory, which is
        // the thing being guarded against.
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_BYTES {
                        tracing::debug!(url, "artwork kept growing past the limit; dropping it");
                        return None;
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(url, "artwork fetch stopped short: {e}");
                    return None;
                }
            }
        }
        if !looks_like_an_image(&bytes) {
            tracing::debug!(url, "not caching a reply that is not an image");
            return Some((Arc::new(bytes), Source::Network));
        }

        let bytes = Arc::new(bytes);
        if let Some(path) = path {
            let copy = Arc::clone(&bytes);
            // Every so often, check the directory has not run away. Counted
            // here rather than timed, because writing is what fills it.
            let due = self
                .written
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let sweep = due
                .is_multiple_of(PRUNE_EVERY)
                .then(|| self.disk.clone())
                .flatten();

            tokio::spawn(async move {
                let written = tokio::task::spawn_blocking(move || {
                    // Written beside the target and renamed onto it. A rename
                    // within one filesystem is atomic, so a reader sees either
                    // the old file or the whole new one — where writing in
                    // place could leave a truncated file that every later read
                    // accepts, since the read only checks the file is not
                    // empty. `panic = "abort"` makes dying mid-write an
                    // ordinary event rather than an exotic one.
                    let temp = temp_name(&path);
                    let out = std::fs::write(&temp, &*copy)
                        .and_then(|()| std::fs::rename(&temp, &path))
                        .inspect_err(|_| {
                            let _ = std::fs::remove_file(&temp);
                        });
                    if let Some(dir) = sweep {
                        prune(&dir);
                    }
                    out
                })
                .await;
                if let Ok(Err(e)) = written {
                    tracing::debug!("could not cache artwork: {e}");
                }
            });
        }
        Some((bytes, Source::Network))
    }
}

/// A name nothing else is writing, beside the file it will become.
///
/// The target is named for the URL alone, so two writes of one cover — the
/// queue's thumbnail and the sleeve's, or this app running twice — were
/// writing the same `.part`: each truncated what the other had put there, and
/// each then renamed whatever was in it onto the cover. Both write the same
/// bytes, so the result was usually right and occasionally a short file that
/// every later read accepted as an image. A counter makes the collision
/// impossible rather than unlikely, and the process id keeps a second instance
/// out of this one's way.
///
/// Pruning sweeps up anything a crash leaves behind, the way it does the
/// covers themselves.
fn temp_name(path: &std::path::Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("{}-{n}.part", std::process::id()))
}

/// Decode and scale to fit a `size` box.
fn decode(bytes: &[u8], size: u32) -> Option<Pixels> {
    // Told what it may spend before it is given anything to read.
    // `load_from_memory`, which this replaced, decodes under the crate's
    // default limits — a 512 MB allocation ceiling and no dimension bound
    // whatever. That was survivable while a panic in here cost one cover: the
    // decode runs in `spawn_blocking` and a failed join was discarded. The
    // release profile now aborts on panic, so this path has to fail by
    // returning rather than by dying, and it should fail sooner than 512 MB.
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_DECODED);

    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .inspect_err(|e| tracing::debug!("unrecognizable artwork: {e}"))
        .ok()?;
    reader.limits(limits);

    let image = reader
        .decode()
        .inspect_err(|e| tracing::debug!("undecodable artwork: {e}"))
        .ok()?;

    // `thumbnail` rather than `resize`: a good deal faster, and the difference
    // is invisible at the sizes anything here is drawn at. It scales *up* as
    // readily as down, though, and enlarging a small cover to fill the box
    // would cost memory and buy nothing — Slint scales it for display anyway.
    let scaled = if image.width() > size || image.height() > size {
        image.thumbnail(size, size)
    } else {
        image
    }
    .to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        scaled.as_raw(),
        scaled.width(),
        scaled.height(),
    ))
}

/// The cover again, as a wash of color rather than a picture.
///
/// Slint has no blur filter, so the blur has to arrive already in the pixels.
///
/// The temptation is to shrink the cover to almost nothing and let the GPU
/// stretch it back, since bilinear upscaling is itself a kind of blur. It is
/// the wrong kind. Magnifying twenty-odd pixels across a whole window makes
/// the interpolation visible as facets and contour rings — the picture is
/// gone, but so is the smoothness that was the point.
///
/// So the blur is done properly, at a size where a real Gaussian is still
/// cheap: a few hundred microseconds on a 144px square, once per track. What
/// gets stretched afterwards is already smooth, and stretching something
/// smooth stays smooth.
pub fn frosted(pixels: &Pixels) -> Option<Pixels> {
    /// Enough pixels that upscaling has something to interpolate between.
    const SMALL: u32 = 144;
    /// A radius large relative to `SMALL`, which is what makes it a wash
    /// rather than a soft-focus photograph.
    const SIGMA: f32 = 20.0;

    let source =
        image::RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec())?;
    // `resize` with a proper filter rather than `thumbnail`: nearest-ish
    // downsampling aliases the sleeve's hard edges into the small image, and
    // the blur then spreads the aliasing around instead of removing it.
    let small =
        image::imageops::resize(&source, SMALL, SMALL, image::imageops::FilterType::Triangle);
    let blurred = image::imageops::blur(&small, SIGMA);

    Some(SharedPixelBuffer::clone_from_slice(
        blurred.as_raw(),
        blurred.width(),
        blurred.height(),
    ))
}

/// One color standing for a whole cover, for tinting the panel behind it.
///
/// Not the average, which on almost any sleeve is a muddy brown: grays, the
/// near-black and the near-white are all thrown away first, so what is left is
/// whatever the cover is actually *colored*. A sleeve that really is
/// monochrome yields nothing, and the caller draws no tint rather than a
/// dirty one.
///
/// The result is then pulled up to a consistent brightness, because the point
/// is a wash of the right hue and not a faithful reproduction — a dark cover
/// should not produce a tint indistinguishable from the background.
fn dominant(pixels: &Pixels) -> Option<[u8; 3]> {
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);

    for px in pixels.as_slice() {
        let (pr, pg, pb) = (px.r as u32, px.g as u32, px.b as u32);
        let high = pr.max(pg).max(pb);
        let low = pr.min(pg).min(pb);

        // Too dark or too bright to have a usable hue, or too close to gray
        // to have one at all.
        if high < 40 || low > 232 || high - low < 28 {
            continue;
        }
        r += pr as u64;
        g += pg as u64;
        b += pb as u64;
        n += 1;
    }

    // A handful of stray colored pixels on a black-and-white sleeve is noise,
    // not a color.
    if n < 64 {
        return None;
    }

    let (r, g, b) = ((r / n) as f32, (g / n) as f32, (b / n) as f32);
    let peak = r.max(g).max(b).max(1.0);
    let lift = 190.0 / peak;

    Some([
        (r * lift).min(255.0) as u8,
        (g * lift).min(255.0) as u8,
        (b * lift).min(255.0) as u8,
    ])
}

/// A stable file name for a URL.
///
/// Hashed rather than escaped because these URLs carry query strings with
/// artist and album names in them, and some of them are longer than a file name
/// is allowed to be.
fn file_name(url: &str) -> String {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Keep the cache directory from growing without end.
///
/// Runs once at startup and drops the oldest files past the limit. Crude —
/// modification time is not access time — but it bounds the directory without
/// tracking anything, and re-fetching a cover that was wrongly dropped costs
/// one request.
fn prune(dir: &PathBuf) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut files: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((meta.modified().ok()?, meta.len(), e.path()))
        })
        .collect();

    let total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if files.len() <= DISK_CACHE_FILES && total <= DISK_CACHE_BYTES {
        return;
    }

    // Oldest first, dropping until both limits are met. The byte total is what
    // stops a run of maximum-size responses filling the disk inside a count
    // that looks generous for real covers.
    files.sort_unstable_by_key(|(modified, _, _)| *modified);

    let mut count = files.len();
    let mut bytes = total;
    let mut dropped = 0usize;
    for (_, len, path) in &files {
        if count <= DISK_CACHE_FILES && bytes <= DISK_CACHE_BYTES {
            break;
        }
        let _ = std::fs::remove_file(path);
        count -= 1;
        bytes = bytes.saturating_sub(*len);
        dropped += 1;
    }

    if dropped > 0 {
        tracing::debug!("pruning {dropped} cached artwork files");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid 24-bit BMP header declaring `w` by `h`, with almost no pixel
    /// data behind it — the shape a header bomb takes.
    /// A real PNG of the given size, encoded.
    ///
    /// PNG rather than the BMP this used to build: the workspace compiles the
    /// `image` crate with `default-features = false` and only jpeg, png and
    /// webp, so a BMP was refused for being a format nobody here can read.
    /// The assertion below passed for that reason and would have passed with
    /// no dimension limit at all.
    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbaImage::new(w, h)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encodes");
        out
    }

    #[test]
    fn a_cover_wider_than_any_cover_is_refused_on_its_dimensions() {
        // 9000 by 100 is the shape that tells this apart from the crate's
        // default limits: it is far under the 512 MB those allow, so without
        // a width bound the decoder starts reading it. Deliberately not a
        // square — a square large enough to breach 512 MB would be refused
        // either way and would prove nothing about this change.
        const { assert!(MAX_SIDE < 9000, "the test no longer exceeds the limit") };

        // The control comes first, and is the point of it: it proves this
        // build can decode the format at all, so the refusal below is the
        // dimension limit doing its job rather than an unreadable format.
        assert!(
            decode(&png_of(100, 100), 360).is_some(),
            "the control must decode, or the refusal below proves nothing"
        );
        assert!(
            decode(&png_of(9000, 100), 360).is_none(),
            "a cover 9000 wide was accepted"
        );
    }

    #[test]
    fn rubbish_that_is_not_an_image_at_all_is_still_none() {
        assert!(decode(b"", 360).is_none());
        assert!(decode(b"not a picture", 360).is_none());
    }

    #[test]
    fn an_ordinary_cover_still_decodes_under_the_limits() {
        let mut png = Vec::new();
        let mut source = image::RgbaImage::new(600, 600);
        source
            .pixels_mut()
            .for_each(|p| *p = image::Rgba([9, 8, 7, 255]));
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let out = decode(&png, 360).expect("a real cover was refused");
        assert_eq!(out.width(), 360, "not scaled to the size asked for");
    }

    /// A player names the address art is fetched from, so "the player said so"
    /// is not on its own a reason to dial somewhere.
    #[test]
    fn art_may_come_from_a_cdn_but_not_from_inside_the_network() {
        let allow = |u: &str| permitted(&reqwest::Url::parse(u).expect(u));

        // A CDN, which is the whole reason this client exists.
        assert!(allow("https://cdn.example.com/cover.jpg"));
        assert!(allow("http://93.184.216.34/cover.jpg"));

        // Somewhere only this machine can reach.
        assert!(!allow("http://127.0.0.1:8080/probe"));
        assert!(!allow("http://[::1]/probe"), "IPv6 loopback is bracketed");
        assert!(!allow("http://192.168.1.1/cgi-bin/reboot"));
        assert!(!allow("http://10.0.0.155:11000/Artwork"));
        assert!(!allow("http://169.254.169.254/latest/meta-data/"));
        assert!(!allow("http://[fe80::1]/probe"), "IPv6 link-local");
        assert!(!allow("http://[fd00::1]/probe"), "IPv6 unique-local");
        assert!(!allow("http://0.0.0.0/probe"));

        // Not a fetchable scheme at all.
        assert!(!allow("file:///etc/passwd"));
    }

    /// An IPv4 address can be written as IPv6, and it dials the same place.
    /// As an `Ipv6Addr` it is not `::1` and in neither private prefix, so
    /// before canonicalizing, every one of these was a CDN as far as the check
    /// could tell.
    #[test]
    fn an_ipv4_address_written_as_ipv6_is_judged_as_ipv4() {
        let allow = |u: &str| permitted(&reqwest::Url::parse(u).expect(u));

        assert!(!allow("http://[::ffff:127.0.0.1]:631/"), "mapped loopback");
        assert!(!allow("http://[::ffff:192.168.1.1]/"), "mapped private");
        assert!(
            !allow("http://[::ffff:169.254.169.254]/"),
            "mapped link-local"
        );
        assert!(!allow("http://[::ffff:0.0.0.0]/"), "mapped unspecified");

        // A public address is public however it is spelled.
        assert!(allow("http://[::ffff:93.184.216.34]/cover.jpg"));
    }

    /// Shared address space, where a tailnet numbers every machine on it.
    #[test]
    fn the_shared_address_space_is_not_the_internet() {
        let allow = |u: &str| permitted(&reqwest::Url::parse(u).expect(u));

        assert!(!allow("http://100.64.0.1/"), "the bottom of 100.64/10");
        assert!(!allow("http://100.101.102.103:8080/"), "a tailnet address");
        assert!(!allow("http://100.127.255.254/"), "the top of 100.64/10");
        assert!(!allow("http://[::ffff:100.100.100.100]/"), "and mapped");

        // Just either side of it is ordinary public space.
        assert!(allow("http://100.63.255.255/"));
        assert!(allow("http://100.128.0.1/"));
    }

    /// Adoption is still what allows a player's own art, wherever the player
    /// is and however its address was written when it was found.
    #[test]
    fn a_player_in_shared_space_or_found_over_ipv6_keeps_its_own_art() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let art = Artwork::in_memory(reqwest::Client::new(), Players::default());

        art.remember_player("100.101.102.103".parse().expect("an address"));
        assert!(art.may_fetch("http://100.101.102.103:11004/library/v1/Artwork"));
        assert!(!art.may_fetch("http://100.101.102.104/"), "only that one");

        art.remember_player("::ffff:10.0.0.155".parse().expect("an address"));
        assert!(
            art.may_fetch("http://10.0.0.155:11000/Artwork"),
            "found as mapped IPv6, fetched as IPv4"
        );
        assert!(art.may_fetch("http://[::ffff:10.0.0.155]:11000/Artwork"));
    }

    /// The desktop shell fetches `mpris:artUrl` itself, with none of this
    /// module's checks, so what it is given has to be safe to hand over.
    #[tokio::test]
    async fn the_desktop_gets_the_cover_on_disk_or_a_url_this_would_fetch() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join(format!("azzurro-desktop-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir");

        let art = Artwork {
            disk: Some(dir.clone()),
            ..Artwork::in_memory(reqwest::Client::new(), Players::default())
        };
        let own = "http://10.0.0.155:11000/Artwork?service=LocalMusic&album=A";

        // Not cached, not adopted: this app would not fetch it, so neither is
        // the shell told of it.
        assert_eq!(art.for_desktop(own).await, None);
        assert_eq!(
            art.for_desktop("http://[::ffff:127.0.0.1]:631/").await,
            None
        );
        assert_eq!(art.for_desktop("file:///etc/passwd").await, None);

        // A CDN's cover is passed on as it is.
        let cdn = "https://cdn.example.com/cover.jpg";
        assert_eq!(art.for_desktop(cdn).await.as_deref(), Some(cdn));

        // Once adopted, the player's own URL is what this would fetch.
        art.remember_player("10.0.0.155".parse().expect("an address"));
        assert_eq!(art.for_desktop(own).await.as_deref(), Some(own));

        // A reply that is not an image is not a cover, even on disk.
        std::fs::write(dir.join(file_name(own)), b"<error/>").expect("writes");
        assert_eq!(art.for_desktop(own).await.as_deref(), Some(own));

        // And a real one on disk is preferred to any request at all.
        std::fs::write(dir.join(file_name(own)), png_of(4, 4)).expect("writes");
        let exported = art.for_desktop(own).await.expect("cached");
        assert!(exported.starts_with("file:///"), "{exported}");
        assert!(exported.ends_with(&file_name(own)), "{exported}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The player's own art is served from its own address, which is in the
    /// space the check above refuses — so adoption is what allows it.
    #[test]
    fn a_players_own_address_is_allowed_once_it_is_adopted() {
        // `run_app` does this for the real binary; a test builds a client
        // without going through it. Idempotent, so several tests may call it.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let art = Artwork::new(reqwest::Client::new(), Players::default());
        let own = "http://10.0.0.155:11000/Artwork?service=LocalMusic";

        assert!(
            !art.may_fetch(own),
            "before adoption it is just another address on the subnet"
        );

        art.remember_player("10.0.0.155".parse().expect("an address"));

        assert!(art.may_fetch(own), "its own art is the point of the app");
        assert!(
            !art.may_fetch("http://10.0.0.156:11000/Artwork"),
            "adopting one player does not open the rest of the subnet"
        );
        assert!(
            art.may_fetch("https://cdn.example.com/cover.jpg"),
            "and a CDN still works"
        );

        // The case that matters most, and the one this got wrong first: a
        // Powernode answers /Artwork on 11000 with a 301 to its own library
        // service on 11004. The redirect is judged by the same rule as the
        // first request, so a rule that knew only the public-address test
        // stopped every cover the player served.
        assert!(
            art.may_fetch("http://10.0.0.155:11004/library/v1/Artwork?album=x"),
            "a player's other ports are the same player"
        );
    }

    /// The file count alone bounds the directory only if the files are the
    /// size a cover is. Each may be up to MAX_BYTES, so the count that looks
    /// generous for real covers allows nearly five gigabytes of whatever a
    /// player chose to serve.
    #[test]
    fn the_cache_is_bounded_by_bytes_as_well_as_by_count() {
        let dir = std::env::temp_dir().join(format!("azzurro-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir");

        // Well inside the file count, well past the byte budget.
        let each = vec![0u8; 4 * 1024 * 1024];
        let wanted = (DISK_CACHE_BYTES / each.len() as u64) as usize + 8;
        for n in 0..wanted {
            std::fs::write(dir.join(format!("{n:016x}")), &each).expect("writes");
        }

        let total = |d: &PathBuf| -> u64 {
            std::fs::read_dir(d)
                .expect("reads")
                .flatten()
                .filter_map(|e| e.metadata().ok().map(|m| m.len()))
                .sum()
        };
        let count = |d: &PathBuf| std::fs::read_dir(d).expect("reads").count();

        assert!(
            count(&dir) < DISK_CACHE_FILES,
            "the count is not what is over"
        );
        assert!(total(&dir) > DISK_CACHE_BYTES, "but the bytes are");

        prune(&dir);

        assert!(
            total(&dir) <= DISK_CACHE_BYTES,
            "prune left {} bytes, over the {DISK_CACHE_BYTES} budget",
            total(&dir)
        );
        assert!(count(&dir) > 0, "and it did not empty the directory");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cover server on the loopback, so the paths below can be walked with
    /// no network at all.
    ///
    /// Loopback is the one address this module refuses out of hand, so every
    /// test using it adopts a player there first — which is also the rule
    /// being exercised. Nothing here speaks to anything off this machine.
    struct Server {
        addr: std::net::SocketAddr,
        /// How many requests have arrived, which is the whole point of several
        /// of the tests below.
        served: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// Serve `replies` in turn, the last one repeating, after waiting `delay`
    /// each time.
    ///
    /// The delay is how a test makes two callers overlap: without it the first
    /// fetch can finish before the second is even polled, and a test for
    /// sharing one fetch would pass whether or not anything was shared.
    ///
    /// `Connection: close`, so each request is its own accept and the count is
    /// a count of requests rather than of connections.
    async fn serving(replies: Vec<Vec<u8>>, delay: Duration) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let addr = listener.local_addr().expect("an address");
        let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&served);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let replies = replies.clone();
                let count = Arc::clone(&count);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};

                    // Read the request head before answering. Windows sends RST
                    // rather than FIN when a socket is closed with unread data
                    // still in it, and the client then loses the reply it had
                    // already been sent.
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while let Ok(read) = socket.read(&mut buf).await {
                        if read == 0 {
                            break;
                        }
                        head.extend_from_slice(&buf[..read]);
                        // The whole of a GET is its head; nothing here sends a
                        // body.
                        if head.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }

                    let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(delay).await;

                    let body = &replies[n.min(replies.len() - 1)];
                    let mut out = format!(
                        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    out.extend_from_slice(body);
                    let _ = socket.write_all(&out).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Server { addr, served }
    }

    /// An [`Artwork`] caching into `dir`, with a player adopted on the
    /// loopback so the server above counts as one.
    fn art_in(dir: &std::path::Path) -> Artwork {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let art = Artwork {
            disk: Some(dir.to_path_buf()),
            ..Artwork::in_memory(reqwest::Client::new(), Players::default())
        };
        art.remember_player("127.0.0.1".parse().expect("an address"));
        art
    }

    /// A directory of this test's own, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("azzurro-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir");
        dir
    }

    /// The queue wants a 72px thumbnail and the sleeve a 720px cover of the
    /// same track at the same moment. Each size is its own entry in the memory
    /// cache, so both missed — and before the fetches were shared, both opened
    /// a connection and read the same bytes.
    #[tokio::test]
    async fn one_cover_wanted_at_two_sizes_is_fetched_once() {
        let dir = scratch("art-shared");
        let art = art_in(&dir);
        let server = serving(vec![png_of(64, 64)], Duration::from_millis(200)).await;
        let url = format!("http://{}/Artwork?album=Shared", server.addr);

        let (thumb, cover) = tokio::join!(art.get(&url, 72), art.get(&url, 720));
        assert!(thumb.is_some(), "the thumbnail did not decode");
        assert!(cover.is_some(), "the cover did not decode");
        assert_eq!(
            server.served.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one cover at two sizes should be one request"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bytes that begin like a PNG and are not one: what a write cut short by
    /// a crash leaves behind, and what a format this build cannot read looks
    /// like. The magic-byte check passes them, so before this the decode
    /// failed silently on every call and that cover stayed blank until pruning
    /// reached the file.
    fn a_broken_png() -> Vec<u8> {
        [&[0x89u8, b'P', b'N', b'G'][..], &[0u8; 64][..]].concat()
    }

    #[tokio::test]
    async fn a_cached_cover_that_will_not_decode_is_dropped_and_fetched_again() {
        let dir = scratch("art-undecodable");
        let art = art_in(&dir);
        let server = serving(vec![png_of(32, 32)], Duration::ZERO).await;
        let url = format!("http://{}/Artwork?album=Broken", server.addr);

        std::fs::write(dir.join(file_name(&url)), a_broken_png()).expect("writes");

        assert!(
            art.get(&url, 72).await.is_some(),
            "the file was kept and the network never asked"
        );
        assert_eq!(
            server.served.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one bad file is worth exactly one more request"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And once, not once per call: a cover that is undecodable wherever it
    /// comes from must not turn into a request every time it is drawn.
    #[tokio::test]
    async fn a_cover_that_is_undecodable_everywhere_is_asked_for_once() {
        let dir = scratch("art-undecodable-twice");
        let art = art_in(&dir);
        let server = serving(vec![a_broken_png()], Duration::ZERO).await;
        let url = format!("http://{}/Artwork?album=Hopeless", server.addr);

        std::fs::write(dir.join(file_name(&url)), a_broken_png()).expect("writes");

        // Three calls, because one is the case that is bounded by
        // construction: the re-fetch is a local flag, so a single `get` could
        // never have asked twice. What the queue actually does is call this
        // again on the next read — and the re-fetch had written the same
        // undecodable bytes back over the file it had just dropped, so every
        // later call read, deleted, fetched and rewrote them again.
        for _ in 0..3 {
            assert!(art.get(&url, 72).await.is_none(), "it cannot be decoded");
        }
        assert_eq!(
            server.served.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the refetch is one attempt, not one per call"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And not for ever. The reply that fails is not always the reply the
    /// player will always give: one answered while its library service is
    /// still coming up is the XML error document `looks_like_an_image` was
    /// written for, and remembering that for the session cost the album its
    /// cover until the app was restarted — the entry is consulted before
    /// anything else, and 64 slots are not turned over in a normal session.
    #[tokio::test]
    async fn a_cover_that_failed_once_is_tried_again_later() {
        let dir = scratch("art-undecodable-expires");
        let art = art_in(&dir);
        // Rubbish first, a real picture afterwards: the player has finished
        // starting up.
        let server = serving(vec![a_broken_png(), png_of(32, 32)], Duration::ZERO).await;
        let url = format!("http://{}/Artwork?album=Recovering", server.addr);

        assert!(
            art.get(&url, 72).await.is_none(),
            "the first reply is not a picture"
        );
        assert!(
            art.get(&url, 72).await.is_none(),
            "and is not asked for again while it is still remembered"
        );
        assert_eq!(
            server.served.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the refetch is one attempt, not one per call"
        );

        // Time passing, without waiting five minutes for it. The value is the
        // instant the entry stops counting, so this is the entry as it will
        // be then.
        art.undecodable
            .lock()
            .unwrap()
            .put(url.clone(), Instant::now());

        assert!(
            art.get(&url, 72).await.is_some(),
            "once the failure has expired the cover is asked for again"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The allow-list is what exempts a player's own address from the rule
    /// against fetching from private space, so it has to be bounded by
    /// something the network cannot move. It used to be bounded by the
    /// registry's adoption cap — which stopped being a bound the moment a
    /// player was allowed to move, since retiring the address it left is what
    /// makes room for the one it announced from.
    #[test]
    fn the_allowed_addresses_are_bounded_and_given_back() {
        let art = Artwork::in_memory(reqwest::Client::new(), Players::default());

        let address = |n: usize| {
            std::net::IpAddr::from(std::net::Ipv4Addr::new(
                10,
                (n >> 8) as u8,
                (n & 0xff) as u8,
                1,
            ))
        };
        for n in 0..MAX_ART_HOSTS + 10 {
            art.remember_player(address(n));
        }
        assert_eq!(
            art.players.lock().unwrap().len(),
            MAX_ART_HOSTS,
            "a flood of announcements must not grow the set without end"
        );
        let over = reqwest::Url::parse(&format!("http://{}/Artwork", address(MAX_ART_HOSTS)))
            .expect("a URL");
        assert!(
            !allowed_by(&art.players, &over),
            "and the ones past the cap are not allowed either"
        );

        // And an address given back is refused again, which is what makes room
        // for a player that has genuinely moved.
        art.forget_player(address(0));
        let first = reqwest::Url::parse(&format!("http://{}/Artwork", address(0))).expect("a URL");
        assert!(!allowed_by(&art.players, &first));
        art.remember_player(address(MAX_ART_HOSTS));
        assert!(allowed_by(&art.players, &over));
    }

    /// A trip is shared, so cancelling the task that happens to own it must
    /// not answer everyone else waiting on it with "there is no art".
    ///
    /// Cancellation here is ordinary: `load_thumbnails` aborts its whole set
    /// as soon as a newer loader takes the generation, and the aborted task
    /// may be the one the sleeve and the sidebar card joined. Read as a miss,
    /// that blanked both of them for the rest of the track — neither is asked
    /// for again until the cover URL changes.
    #[tokio::test]
    async fn a_cancelled_fetch_is_not_a_miss_for_the_callers_that_joined_it() {
        let dir = scratch("art-cancelled");
        let art = Arc::new(art_in(&dir));
        let server = serving(vec![png_of(64, 64)], Duration::from_millis(300)).await;
        let url = format!("http://{}/Artwork?album=Aborted", server.addr);

        // First in, so this one owns the trip.
        let owner = tokio::spawn({
            let art = Arc::clone(&art);
            let url = url.clone();
            async move { art.get(&url, 72).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The sleeve wants the same cover at another size and joins the trip
        // already running.
        let joined = tokio::spawn({
            let art = Arc::clone(&art);
            let url = url.clone();
            async move { art.get(&url, 720).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Then the user scrolls, and the loader that owned the trip is gone.
        owner.abort();

        assert!(
            joined.await.expect("joins").is_some(),
            "a cover the user can see went missing because another caller was cancelled"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fetch limit bounds connections and nothing else, so a cover already
    /// on disk used to reach `spawn_blocking` with no permit of any kind — and
    /// a queue window of a hundred cached rows was a hundred decoders running
    /// at once, each allowed `MAX_DECODED` to itself.
    #[tokio::test]
    async fn a_decode_waits_for_a_permit_of_its_own() {
        let dir = scratch("art-decode-permit");
        let art = Arc::new(art_in(&dir));
        let server = serving(vec![png_of(32, 32)], Duration::ZERO).await;
        let url = format!("http://{}/Artwork?album=Gated", server.addr);

        // Every decode permit, held by this test.
        let held = art
            .decoding
            .acquire_many(CONCURRENT_DECODES as u32)
            .await
            .expect("the semaphore is never closed");

        let mine = Arc::clone(&art);
        let wanted = url.clone();
        let task = tokio::spawn(async move { mine.get(&wanted, 72).await.is_some() });

        // Long enough that the fetch has certainly landed: what is left is the
        // decode, which has nothing to run on.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !task.is_finished(),
            "a cover decoded although every permit was held"
        );

        drop(held);
        assert!(task.await.expect("joins"), "and decodes once one frees");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A permit handed back while the work it bounds is still running is not a
    /// bound at all.
    ///
    /// A blocking task has no cancellation point, so aborting the caller ends
    /// the future and not the decode. Held in a local across the await, the
    /// permit went back at the abort and the walk that replaced this one took
    /// a fresh one at once — and a walk being replaced is routine rather than
    /// exotic: `start_walk` aborts up to six of these on every queue page and
    /// every browse page. Against a player serving covers near [`MAX_SIDE`],
    /// each orphan is another [`MAX_DECODED`] allocation with nothing above it
    /// but the size of tokio's blocking pool.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_aborted_decode_keeps_its_permit_until_it_has_finished() {
        let dir = scratch("art-abort-permit");
        let art = Arc::new(art_in(&dir));
        // Big enough that the decode is certainly still going a moment after
        // the abort lands, which is the whole thing being measured. The fetch
        // is not delayed: what has to be reached is the decode.
        let server = serving(vec![png_of(3072, 3072)], Duration::ZERO).await;

        // One cover per permit, none of them the same, so every permit is
        // inside a `spawn_blocking` when the aborts arrive.
        let decoding: Vec<_> = (0..CONCURRENT_DECODES)
            .map(|n| {
                let art = Arc::clone(&art);
                let url = format!("http://{}/Artwork?album={n}", server.addr);
                tokio::spawn(async move { art.get(&url, 72).await })
            })
            .collect();

        let deadline = Instant::now() + Duration::from_secs(20);
        while art.decoding.available_permits() > 0 {
            assert!(Instant::now() < deadline, "the decodes never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The walk these belong to is replaced.
        for walk in &decoding {
            walk.abort();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            art.decoding.available_permits(),
            0,
            "the permits went back while the decodes were still running, so \
             the next walk's three run alongside these three"
        );

        for walk in decoding {
            let _ = walk.await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two writes of one cover name the same file, so before this they named
    /// the same temporary too — each truncating what the other had put there
    /// and then renaming whatever was in it onto the cover.
    #[test]
    fn a_temporary_name_is_never_reused() {
        let path = PathBuf::from("/var/cache/azzurro/artwork/00112233445566aa");
        let first = temp_name(&path);
        let second = temp_name(&path);

        assert_ne!(first, second, "two writes of one cover would collide");
        assert_ne!(first, path, "and neither is the cover itself");
        assert_eq!(first.parent(), path.parent(), "written beside the target");
        assert!(
            first.to_string_lossy().ends_with(".part"),
            "still recognizable as a part file: {first:?}"
        );
    }

    #[test]
    fn file_names_are_stable_and_distinct() {
        let a = file_name("http://10.0.0.155:11000/Artwork?service=LocalMusic&album=A");
        let b = file_name("http://10.0.0.155:11000/Artwork?service=LocalMusic&album=B");
        assert_eq!(
            a,
            file_name("http://10.0.0.155:11000/Artwork?service=LocalMusic&album=A")
        );
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn decodes_and_scales_to_fit() {
        // A 4x2 PNG, so the aspect ratio has something to preserve.
        let mut png = Vec::new();
        {
            let mut source = image::RgbaImage::new(4, 2);
            source
                .pixels_mut()
                .for_each(|p| *p = image::Rgba([1, 2, 3, 255]));
            image::DynamicImage::ImageRgba8(source)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
        }

        let scaled = decode(&png, 2).expect("a valid PNG decodes");
        assert_eq!(scaled.width(), 2);
        assert_eq!(scaled.height(), 1);

        // Smaller than the box: left alone rather than blown up. `thumbnail`
        // would happily have enlarged it to 64x32.
        let same = decode(&png, 64).expect("a valid PNG decodes");
        assert_eq!(same.width(), 4);
        assert_eq!(same.height(), 2);
    }

    /// Build a buffer of one repeated color, the way a test cover would be.
    fn swatch(r: u8, g: u8, b: u8) -> Pixels {
        let pixel = [r, g, b, 255];
        let data: Vec<u8> = std::iter::repeat_n(pixel, 32 * 32).flatten().collect();
        SharedPixelBuffer::clone_from_slice(&data, 32, 32)
    }

    #[test]
    fn takes_its_color_from_the_colored_pixels() {
        // A saturated cover gives its own hue back, lifted to a usable
        // brightness rather than reproduced exactly.
        let tint = dominant(&swatch(120, 30, 30)).expect("a red cover has a color");
        assert!(
            tint[0] > tint[1] && tint[0] > tint[2],
            "still red: {tint:?}"
        );
        assert!(tint[0] > 150, "lifted out of the dark: {tint:?}");

        // Gray, black and white have no hue to take, and a muddy tint is worse
        // than none.
        assert_eq!(dominant(&swatch(128, 128, 128)), None);
        assert_eq!(dominant(&swatch(0, 0, 0)), None);
        assert_eq!(dominant(&swatch(255, 255, 255)), None);
        // Nearly gray is still gray.
        assert_eq!(dominant(&swatch(130, 140, 136)), None);
    }

    #[test]
    fn a_dark_and_a_light_version_of_one_hue_tint_alike() {
        let dark = dominant(&swatch(40, 20, 90)).unwrap();
        let light = dominant(&swatch(120, 60, 230)).unwrap();
        // Same hue, so the lift should land them close together.
        for channel in 0..3 {
            let gap = (dark[channel] as i32 - light[channel] as i32).abs();
            assert!(
                gap < 40,
                "channel {channel} differs by {gap}: {dark:?} vs {light:?}"
            );
        }
    }

    #[test]
    fn rubbish_is_not_an_image() {
        assert!(decode(b"", 64).is_none());
        assert!(decode(b"<html>404</html>", 64).is_none());
    }
}

#[cfg(test)]
mod frost_timing {
    use super::*;

    #[test]
    fn frosting_a_cover_is_quick_enough_to_do_per_track() {
        let mut data = vec![0u8; 500 * 500 * 4];
        // Hard edges, which is the case the blur has to work hardest on.
        for (i, p) in data.chunks_mut(4).enumerate() {
            let on = (i / 25) % 2 == 0;
            p.copy_from_slice(if on {
                &[240, 30, 10, 255]
            } else {
                &[5, 5, 40, 255]
            });
        }
        let cover = SharedPixelBuffer::clone_from_slice(&data, 500, 500);

        let start = std::time::Instant::now();
        let out = frosted(&cover).expect("frosts");
        let took = start.elapsed();

        assert_eq!(out.width(), 144);
        println!("frosted in {took:?}");
        // 3ms in release, ~170ms unoptimized. The bound is loose on purpose:
        // it is here to catch the blur becoming a whole different order of
        // cost, not to police a few milliseconds on a busy machine.
        assert!(
            took.as_millis() < 1500,
            "too slow for a track change: {took:?}"
        );
    }
}
