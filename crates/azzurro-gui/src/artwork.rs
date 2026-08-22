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

/// Files kept on disk. Pruned to this on startup, oldest first.
const DISK_CACHE_FILES: usize = 512;

/// Refuse anything implausible for a piece of cover art rather than decoding
/// it. Guards against a redirect to something that is not an image at all.
const MAX_BYTES: usize = 8 * 1024 * 1024;

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
    /// One colour per image, computed once when it is decoded. Keyed by URL
    /// alone: the same cover gives the same colour at any size.
    tints: Mutex<LruCache<String, [u8; 3]>>,
    /// `None` when there is nowhere to write, which is not fatal: the memory
    /// cache and the network still work.
    disk: Option<PathBuf>,
    limit: Semaphore,
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

    /// What is already decoded, without touching the disk or the network.
    ///
    /// This is what the queue view calls while building rows: it fills in the
    /// covers it has and leaves the rest blank, and a later republish picks up
    /// whatever arrived in the meantime.
    pub fn cached(&self, url: &str, size: u32) -> Option<Pixels> {
        let key = Key {
            url: url.to_owned(),
            size,
        };
        self.memory.lock().unwrap().get(&key).cloned()
    }

    /// The colour to tint a panel with behind this artwork, if it has been
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
            tokio::spawn(async move {
                let written = tokio::task::spawn_blocking(move || std::fs::write(path, copy)).await;
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

/// The cover again, as a wash of colour rather than a picture.
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

/// One colour standing for a whole cover, for tinting the panel behind it.
///
/// Not the average, which on almost any sleeve is a muddy brown: greys, the
/// near-black and the near-white are all thrown away first, so what is left is
/// whatever the cover is actually *coloured*. A sleeve that really is
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

        // Too dark or too bright to have a usable hue, or too close to grey
        // to have one at all.
        if high < 40 || low > 232 || high - low < 28 {
            continue;
        }
        r += pr as u64;
        g += pg as u64;
        b += pb as u64;
        n += 1;
    }

    // A handful of stray coloured pixels on a black-and-white sleeve is noise,
    // not a colour.
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
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();

    if files.len() <= DISK_CACHE_FILES {
        return;
    }

    files.sort_unstable_by_key(|(modified, _)| *modified);
    let excess = files.len() - DISK_CACHE_FILES;
    tracing::debug!("pruning {excess} cached artwork files");
    for (_, path) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid 24-bit BMP header declaring `w` by `h`, with almost no pixel
    /// data behind it — the shape a header bomb takes.
    fn header_claiming(w: i32, h: i32) -> Vec<u8> {
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&0u32.to_le_bytes()); // file size, unchecked
        bmp.extend_from_slice(&0u32.to_le_bytes()); // reserved
        bmp.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
        bmp.extend_from_slice(&40u32.to_le_bytes()); // DIB header size
        bmp.extend_from_slice(&w.to_le_bytes());
        bmp.extend_from_slice(&h.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
        bmp.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        bmp.resize(54 + 16, 0);
        bmp
    }

    #[test]
    fn a_cover_wider_than_any_cover_is_refused_on_its_dimensions() {
        // 9000 by 100 is the shape that tells this apart from the crate's
        // default limits: it is far under the 512 MB those allow, so without
        // a width bound the decoder starts reading it. Deliberately not a
        // square — a square large enough to breach 512 MB would be refused
        // either way and would prove nothing about this change.
        const { assert!(MAX_SIDE < 9000, "the test no longer exceeds the limit") };
        assert!(
            decode(&header_claiming(9000, 100), 360).is_none(),
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

    /// Build a buffer of one repeated colour, the way a test cover would be.
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

        // Grey, black and white have no hue to take, and a muddy tint is worse
        // than none.
        assert_eq!(dominant(&swatch(128, 128, 128)), None);
        assert_eq!(dominant(&swatch(0, 0, 0)), None);
        assert_eq!(dominant(&swatch(255, 255, 255)), None);
        // Nearly grey is still grey.
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
