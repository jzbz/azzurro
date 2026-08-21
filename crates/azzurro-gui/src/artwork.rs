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

use lru::LruCache;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use tokio::sync::Semaphore;

/// Decoded art, ready for `slint::Image::from_rgba8`.
pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// How many decoded images to keep. A screenful of queue thumbnails plus the
/// covers of everything recently selected, with room to spare; at 80×80 RGBA
/// that is a few megabytes at most.
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

/// A decoded image is identified by where it came from and how big it was
/// wanted, since the same cover is drawn at two sizes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    url: String,
    size: u32,
}

pub struct Artwork {
    http: reqwest::Client,
    memory: Mutex<LruCache<Key, Pixels>>,
    /// `None` when there is nowhere to write, which is not fatal: the memory
    /// cache and the network still work.
    disk: Option<PathBuf>,
    limit: Semaphore,
}

impl Artwork {
    pub fn new(http: reqwest::Client) -> Self {
        let disk = dirs::cache_dir().map(|d| d.join("azzurro").join("artwork"));

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

        if let Some(path) = &path
            && let Ok(bytes) = std::fs::read(path)
            && !bytes.is_empty()
        {
            return Some(bytes);
        }

        // Held across the request, not the decode: the point is to bound
        // connections, and a permit kept through decoding would idle it.
        let _permit = self.limit.acquire().await.ok()?;

        let response = match self.http.get(url).send().await {
            Ok(response) => response.error_for_status().ok()?,
            Err(e) => {
                tracing::debug!(url, "artwork fetch failed: {e}");
                return None;
            }
        };

        // Trust the declared length where there is one, and check the real
        // length either way — a chunked response declares nothing.
        if response
            .content_length()
            .is_some_and(|n| n as usize > MAX_BYTES)
        {
            tracing::debug!(url, "artwork is implausibly large; skipping");
            return None;
        }
        let bytes = response.bytes().await.ok()?;
        if bytes.len() > MAX_BYTES {
            return None;
        }

        if let Some(path) = &path
            && let Err(e) = std::fs::write(path, &bytes)
        {
            tracing::debug!("could not cache artwork: {e}");
        }
        Some(bytes.to_vec())
    }
}

/// Decode and scale to fit a `size` box.
fn decode(bytes: &[u8], size: u32) -> Option<Pixels> {
    let image = image::load_from_memory(bytes)
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

    #[test]
    fn rubbish_is_not_an_image() {
        assert!(decode(b"", 64).is_none());
        assert!(decode(b"<html>404</html>", 64).is_none());
    }
}
