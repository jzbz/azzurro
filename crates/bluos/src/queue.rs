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
    /// Set once the queue has been edited away from whatever loaded it.
    #[serde(rename = "@modified")]
    pub modified: Option<u8>,

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

    /// Whether the cursor is a now-playing marker rather than a bookmark.
    ///
    /// Two conditions, and both matter: the queue has to be what the player is
    /// working through, *and* the player has to be playing. A paused player's
    /// cursor is where it would resume, which is not the same as where it is.
    ///
    /// Here rather than at each caller because it was written out twice and
    /// the copies had already drifted — the CLI reported a paused queue as
    /// playing, the window did not.
    pub fn is_live(&self, status: &Status) -> bool {
        self.is_playing_from(status) && status.is_playing()
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

    /// As the player reports it, which is **not always a number**: some files
    /// come back as `3/15`, meaning track three of fifteen. Kept as text so a
    /// queue does not fail to parse over a tag format, with
    /// [`QueueSong::track_number`] for when a number is wanted.
    pub track: Option<String>,
    #[serde(rename = "discno")]
    pub disc: Option<String>,
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
    /// The track number, from either `3` or `3/15`.
    pub fn track_number(&self) -> Option<u32> {
        leading_number(self.track.as_deref()?)
    }

    /// The disc number, same treatment.
    pub fn disc_number(&self) -> Option<u32> {
        leading_number(self.disc.as_deref()?)
    }

    /// `4:03`, or `1:02:17` for something long. `None` when the player did not
    /// say — a live stream in the queue has no length.
    pub fn duration(&self) -> Option<String> {
        Some(crate::clock(self.seconds? as i64))
    }
}

/// The number before any `/`, which is how a `3/15` tag reads.
fn leading_number(raw: &str) -> Option<u32> {
    raw.split('/').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two songs from a real 42-track queue on a Bluesound Powernode, with the
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

    /// Two conditions, and the copies of this rule had already drifted: the
    /// CLI reported a paused queue as playing because it checked only the
    /// first.
    #[test]
    fn a_paused_queue_is_not_playing() {
        let queue = Queue {
            id: Some(7),
            length: 3,
            ..Default::default()
        };
        let playing = Status {
            pid: Some(7),
            state: Some("play".to_owned()),
            ..Default::default()
        };
        let paused = Status {
            state: Some("pause".to_owned()),
            ..playing.clone()
        };

        assert!(queue.is_playing_from(&playing));
        assert!(queue.is_live(&playing), "playing from this queue is live");

        assert!(
            queue.is_playing_from(&paused),
            "still the queue the player is on"
        );
        assert!(
            !queue.is_live(&paused),
            "but paused, so the cursor is a bookmark and not a marker"
        );
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
        assert_eq!(first.track_number(), Some(49));
        // Only on multi-disc releases.
        assert_eq!(first.disc, None);
        assert_eq!(q.songs[1].disc_number(), Some(2));
    }

    #[test]
    fn a_track_number_may_carry_its_total() {
        // Seen on a real player: "3/15" is track three of fifteen. Typing this
        // as a number made the whole queue fail to parse.
        let q: Queue = quick_xml::de::from_str(
            r#"<playlist length="1" id="709" modified="1">
                 <song service="LocalMusic" id="0">
                   <title>In the Air Tonight</title>
                   <track>3/15</track>
                   <discno>1/2</discno>
                   <time>336</time>
                 </song>
               </playlist>"#,
        )
        .unwrap();

        assert_eq!(q.songs.len(), 1);
        assert_eq!(q.songs[0].track.as_deref(), Some("3/15"));
        assert_eq!(q.songs[0].track_number(), Some(3));
        assert_eq!(q.songs[0].disc_number(), Some(1));

        // A plain number still works, and rubbish is None rather than fatal.
        assert_eq!(leading_number("7"), Some(7));
        assert_eq!(leading_number(" 7 / 12"), Some(7));
        assert_eq!(leading_number("A1"), None);
        assert_eq!(leading_number(""), None);
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
