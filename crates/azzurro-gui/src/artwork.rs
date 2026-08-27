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

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use lru::LruCache;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use tokio::sync::Semaphore;

/// Decoded art, ready for `slint::Image::from_rgba8`.
pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// How many decoded images to keep. A screenful of queue thumbnails plus the
/// covers of everything recently selected, with room to spare.
///
/// Counted in entries rather than bytes, so what it costs depends on which
/// sizes are in it. Nothing asks for the 80×80 this comment used to claim; the
/// three tiers the GUI requests are `THUMB_SIZE` 72, `TILE_SIZE` 232 and
/// `COVER_SIZE` 360, and one LRU holds all of them. At four bytes a pixel the
/// worst case for a full cache of each is
///
/// | tier | one entry | 256 entries |
/// |------|-----------|-------------|
/// | 72   | 20 KiB    | 5 MiB       |
/// | 232  | 210 KiB   | 53 MiB      |
/// | 360  | 506 KiB   | 127 MiB     |
///
/// so a realistic mix — mostly thumbnails and shelf tiles, a few covers — sits
/// in the tens of megabytes. The 360 row needs 256 *different albums* opened
/// at full size to be reached and is the least likely of the three; the 232
/// row is the one a few minutes of browsing actually approaches.
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

/// How many redirects a cover may take before it is not worth following.
///
/// The same figure the protocol client uses, and for the same reason: a chain
/// longer than this is not a CDN doing its job.
pub const MAX_HOPS: usize = 5;

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
fn literal_address(host: &str) -> Option<std::net::IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Address space that is reachable from this machine but not from the internet.
fn is_private(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            v4.is_private() || v4.is_link_local() || v4.is_broadcast() || v4.is_documentation()
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

pub struct Artwork {
    http: reqwest::Client,
    memory: Mutex<LruCache<Key, Pixels>>,
    /// One color per image, computed once when it is decoded. Keyed by URL
    /// alone: the same cover gives the same color at any size.
    tints: Mutex<LruCache<String, [u8; 3]>>,
    /// `None` when there is nowhere to write, which is not fatal: the memory
    /// cache and the network still work.
    disk: Option<PathBuf>,
    /// Covers written since the process started. Every [`PRUNE_EVERY`] of them
    /// buys a look at the directory; see [`DISK_CACHE_FILES`].
    written: std::sync::atomic::AtomicUsize,
    limit: Semaphore,
    /// The addresses of players this app has adopted.
    ///
    /// A player serves its own cover art from its own address, which is on the
    /// user's subnet and so is exactly the private space [`permitted`] refuses
    /// by default. Recording the players separates "this speaker's own art"
    /// from "some other machine on the LAN a player pointed us at", which is
    /// the whole distinction that check exists to make.
    players: Mutex<std::collections::HashSet<std::net::IpAddr>>,
}

impl Artwork {
    pub fn new(http: reqwest::Client) -> Self {
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
            players: Mutex::new(std::collections::HashSet::new()),
            memory: Mutex::new(LruCache::new(
                NonZeroUsize::new(MEMORY_CACHE).expect("MEMORY_CACHE is not zero"),
            )),
            tints: Mutex::new(LruCache::new(
                NonZeroUsize::new(MEMORY_CACHE).expect("MEMORY_CACHE is not zero"),
            )),
            disk: disk.filter(|d| d.is_dir()),
            limit: Semaphore::new(CONCURRENT_FETCHES),
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
        let key = Key {
            url: url.to_owned(),
            size,
        };
        self.memory.lock().unwrap().peek(&key).cloned()
    }

    /// Note that this player's own address serves art that is allowed.
    ///
    /// Called as players are adopted. Nothing is ever removed: a player that
    /// goes away does not make the art it already served dangerous, and the
    /// set is bounded by `MAX_TRACKED` adoptions either way.
    pub fn remember_player(&self, host: std::net::IpAddr) {
        self.players.lock().unwrap().insert(host);
    }

    /// Whether art may be fetched from here, given the players adopted so far.
    fn may_fetch(&self, url: &str) -> bool {
        let Ok(url) = reqwest::Url::parse(url) else {
            return false;
        };
        if permitted(&url) {
            return true;
        }
        match url.host_str().and_then(literal_address) {
            Some(addr) => self.players.lock().unwrap().contains(&addr),
            None => false,
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

        let bytes = self.bytes(url).await?;

        // Decoding a large JPEG is tens of milliseconds and would otherwise
        // occupy an async worker doing nothing else.
        let decoded = tokio::task::spawn_blocking(move || decode(&bytes, size))
            .await
            .ok()
            .flatten()?;

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
        Some(decoded)
    }

    /// The bytes as fetched, from disk if they are there.
    async fn bytes(&self, url: &str) -> Option<Vec<u8>> {
        let path = self.disk.as_ref().map(|dir| dir.join(file_name(url)));

        // Off the async workers. Reading a cover is a millisecond on a local
        // disk and rather more when the cache directory turns out to live on a
        // network mount, which is exactly the case that would otherwise stall a
        // runtime thread.
        if let Some(path) = path.clone() {
            let cached = tokio::task::spawn_blocking(move || std::fs::read(path).ok())
                .await
                .ok()
                .flatten();
            if let Some(bytes) = cached.filter(|b| !b.is_empty()) {
                return Some(bytes);
            }
        }

        // The player chose this address, so it is checked before it is dialled
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
        if let Some(path) = path {
            let copy = bytes.clone();
            // Every so often, check the directory has not run away. Counted
            // here rather than timed, because writing is what fills it.
            let due = self
                .written
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let sweep = (due % PRUNE_EVERY == 0)
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
                    let temp = path.with_extension("part");
                    let out = std::fs::write(&temp, copy)
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
        Some(bytes)
    }
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
        .inspect_err(|e| tracing::debug!("unrecognisable artwork: {e}"))
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

    /// The player's own art is served from its own address, which is in the
    /// space the check above refuses — so adoption is what allows it.
    #[test]
    fn a_players_own_address_is_allowed_once_it_is_adopted() {
        // `run_app` does this for the real binary; a test builds a client
        // without going through it. Idempotent, so several tests may call it.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let art = Artwork::new(reqwest::Client::new());
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
    fn takes_its_colour_from_the_coloured_pixels() {
        // A saturated cover gives its own hue back, lifted to a usable
        // brightness rather than reproduced exactly.
        let tint = dominant(&swatch(120, 30, 30)).expect("a red cover has a colour");
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
        // 3ms in release, ~170ms unoptimised. The bound is loose on purpose:
        // it is here to catch the blur becoming a whole different order of
        // cost, not to police a few milliseconds on a busy machine.
        assert!(
            took.as_millis() < 1500,
            "too slow for a track change: {took:?}"
        );
    }
}
