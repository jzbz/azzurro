//! The play queue.
//!
//! Two endpoints describe it. `/ui/Queue` is the server-driven screen the
//! official controller renders: paginated twenty at a time, with the device's
//! own context menus and Save/Edit/Clear buttons attached, and durations
//! pre-formatted as `3:58`. `/Playlist` is the older, structured one: the whole
//! queue in one document, `time` in seconds, and fields with types.
//!
//! This module reads `/Playlist`, because a library wants data and not a UI.
//! `/ui/Queue` is what to reach for when the device's own context menus are
//! wanted; see `docs/protocol.md`.

use serde::Deserialize;

use crate::status::Status;

/// `/Playlist` — the queue as the player holds it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Queue {
    /// How many tracks are in the queue, which is not necessarily how many are
    /// in `songs`: a range request returns a window onto this.
    #[serde(rename = "@length")]
    pub length: u32,
    /// Queue identity. Matches [`Status::pid`], and changes when the queue is
    /// replaced — which is the cue to re-read it.
    #[serde(rename = "@id")]
    pub id: Option<u32>,
    #[serde(rename = "@shuffle")]
    pub shuffle: Option<u8>,
    #[serde(rename = "@repeat")]
    pub repeat: Option<u8>,

    #[serde(default, rename = "song")]
    pub songs: Vec<QueueSong>,
}

impl Queue {
    /// Whether `status` describes this queue at all, rather than one the player
    /// has since replaced.
    pub fn describes(&self, status: &Status) -> bool {
        matches!((self.id, status.pid), (Some(mine), Some(theirs)) if mine == theirs)
    }

    /// Where the player sits in this queue: the track playing, or the one that
    /// would resume.
    ///
    /// A player switched to an HDMI input keeps its queue and keeps its
    /// position in it, so this stays put and is still the right row to mark —
    /// but it is the cursor, not necessarily what you are hearing. For that,
    /// ask [`Queue::is_playing_from`] as well.
    pub fn cursor(&self, status: &Status) -> Option<u32> {
        self.describes(status).then_some(status.song).flatten()
    }

    /// Whether the player is actually working through this queue right now,
    /// rather than sitting on it while an input or a stream plays.
    pub fn is_playing_from(&self, status: &Status) -> bool {
        self.describes(status) && status.is_queue_based()
    }
}

/// One track in the queue.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QueueSong {
    /// Position in the queue, and the value `/Play?id=` takes.
    #[serde(rename = "@id")]
    pub id: u32,
    /// `LocalMusic`, `TuneIn`, `Tidal`, and so on.
    #[serde(rename = "@service")]
    pub service: Option<String>,

    pub title: Option<String>,
    /// The player abbreviates these two; they are the artist and the album.
    #[serde(rename = "art")]
    pub artist: Option<String>,
    #[serde(rename = "alb")]
    pub album: Option<String>,

    pub track: Option<u32>,
    #[serde(rename = "discno")]
    pub disc: Option<u32>,
    /// Release year, as the player has it — a string, because it is not always
    /// a year.
    pub date: Option<String>,
    /// Track length in whole seconds.
    #[serde(rename = "time")]
    pub seconds: Option<u32>,
    /// `cd`, `hd`, `dolbyAtmos`, and so on.
    pub quality: Option<String>,
    /// Cover art, as a path on the player or an absolute URL at a service CDN.
    /// Resolve with [`crate::Client::image_url`].
    pub image: Option<String>,
    /// Where the file lives, for library tracks. `fn` is a Rust keyword.
    #[serde(rename = "fn")]
    pub file: Option<String>,
}

impl QueueSong {
    /// `4:03`, or `1:02:17` for something long. `None` when the player did not
    /// say — a live stream in the queue has no length.
    pub fn duration(&self) -> Option<String> {
        Some(crate::clock(self.seconds? as i64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two songs from a real 42-track queue on an NAD Powernode, with the
    /// library's own paths replaced.
    const PLAYLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist length="42" id="692" shuffle="0" repeat="0">
  <song service="LocalMusic" id="0">
    <art>The Rolling Stones</art>
    <alb>The Rolling Stones in (Remastered 2016)</alb>
    <title>19th Nervous Breakdown (Remastered)</title>
    <track>49</track>
    <date>2016</date>
    <time>238</time>
    <fn>/var/mnt/music/example.flac</fn>
    <quality>hd</quality>
    <image>/Artwork?service=LocalMusic&amp;artist=The+Rolling+Stones</image>
  </song>
  <song service="LocalMusic" id="41">
    <art>The Rolling Stones</art>
    <alb>Forty Licks CD2</alb>
    <title>You Got Me Rocking</title>
    <track>8</track>
    <discno>2</discno>
    <date>2002</date>
    <time>214</time>
    <fn>/var/mnt/music/example2.flac</fn>
    <quality>cd</quality>
    <image>/Artwork?service=LocalMusic&amp;artist=The+Rolling+Stones</image>
  </song>
</playlist>"#;

    fn queue() -> Queue {
        quick_xml::de::from_str(PLAYLIST).unwrap()
    }

    #[test]
    fn reads_a_real_queue() {
        let q = queue();
        // 42 in the queue, two in this document: the count is not the window.
        assert_eq!(q.length, 42);
        assert_eq!(q.songs.len(), 2);
        assert_eq!(q.id, Some(692));

        let first = &q.songs[0];
        assert_eq!(first.id, 0);
        assert_eq!(first.service.as_deref(), Some("LocalMusic"));
        assert_eq!(
            first.title.as_deref(),
            Some("19th Nervous Breakdown (Remastered)")
        );
        assert_eq!(first.artist.as_deref(), Some("The Rolling Stones"));
        assert_eq!(first.seconds, Some(238));
        assert_eq!(first.quality.as_deref(), Some("hd"));
        // Only on multi-disc releases.
        assert_eq!(first.disc, None);
        assert_eq!(q.songs[1].disc, Some(2));
    }

    #[test]
    fn formats_durations() {
        assert_eq!(queue().songs[0].duration().as_deref(), Some("3:58"));
        assert_eq!(
            QueueSong {
                seconds: Some(3737),
                ..Default::default()
            }
            .duration()
            .as_deref(),
            Some("1:02:17")
        );
        assert_eq!(
            QueueSong {
                seconds: Some(9),
                ..Default::default()
            }
            .duration()
            .as_deref(),
            Some("0:09")
        );
        // A live stream in the queue has no length.
        assert_eq!(QueueSong::default().duration(), None);
    }

    #[test]
    fn tracks_the_cursor_even_when_the_queue_is_not_what_is_playing() {
        let q = queue();

        let playing = Status {
            pid: Some(692),
            song: Some(41),
            ..Default::default()
        };
        assert!(q.describes(&playing));
        assert_eq!(q.cursor(&playing), Some(41));
        assert!(q.is_playing_from(&playing));

        // Switched to HDMI ARC. The queue and the position survive — that is
        // where playback would resume — but the sound is coming from the
        // input, so the row is a bookmark and not a now-playing marker.
        let on_input = Status {
            pid: Some(692),
            song: Some(41),
            stream_url: Some("Capture:hw:imxspdif,0/1/25/2?id=input4".into()),
            ..Default::default()
        };
        assert!(q.describes(&on_input));
        assert_eq!(q.cursor(&on_input), Some(41));
        assert!(!q.is_playing_from(&on_input));

        // A different queue entirely.
        let replaced = Status {
            pid: Some(700),
            song: Some(0),
            ..Default::default()
        };
        assert!(!q.describes(&replaced));
        assert_eq!(q.cursor(&replaced), None);
        assert_eq!(q.cursor(&Status::default()), None);
    }
}
