//! One MPRIS player per BluOS player.
//!
//! The desktop's own media controls — GNOME's shell menu, KDE's applet, the
//! media keys, a lock screen — all speak MPRIS over D-Bus. Exporting each
//! player separately rather than exporting one "currently selected" player is
//! the point of a multi-room controller: two speakers playing two different
//! things are two things the desktop should be able to see and drive, and a
//! single aggregate would have to lie about one of them.
//!
//! Everything here reads the last status the poller stored and writes commands
//! into the same channel the window's buttons use, so a D-Bus caller and a
//! click are indistinguishable by the time they reach a player.

use std::sync::Mutex;
use std::time::Instant;

use bluos::{DeviceId, Repeat, Status};
use mpris_server::zbus::{Result as ZbusResult, fdo};
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, Property, RootInterface,
    Server, Signal, Time, TrackId, Volume,
};
use tokio::sync::mpsc;

use crate::{Action, Command, Registry};

/// A position report this far from what the last one implied is treated as a
/// seek rather than as playback drifting on, and gets a `Seeked` signal so
/// clients resync their progress bar instead of waiting to poll.
const SEEK_TOLERANCE_SECS: i64 = 3;

/// What was last announced on the bus, so that only genuine changes are
/// emitted. A player long-polling a live stream reports a new document every
/// few seconds, and re-announcing an unchanged title on each one would wake
/// every media widget on the desktop for nothing.
struct Announced {
    playback: PlaybackStatus,
    loop_status: LoopStatus,
    shuffle: bool,
    volume_percent: i32,
    can_seek: bool,
    track: TrackKey,
    position_secs: i64,
}

/// The parts of a track that, taken together, mean "this is a different track".
#[derive(PartialEq)]
struct TrackKey {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    art: Option<String>,
    length_micros: Option<i64>,
}

/// The D-Bus object for one player, and the memory of what it last said.
pub struct Bridge {
    server: Server<Exported>,
    announced: Mutex<Option<Announced>>,
}

impl Bridge {
    /// Claim a bus name for this player and start serving.
    ///
    /// Returns `None` rather than failing the caller: a controller on a machine
    /// with no session bus should still control speakers, just without the
    /// desktop integration.
    pub async fn attach(
        index: usize,
        id: DeviceId,
        name: String,
        registry: Registry,
        commands: mpsc::UnboundedSender<Command>,
    ) -> Option<Self> {
        // `org.mpris.MediaPlayer2.azzurro.instance<pid>_<n>`. The spec wants a
        // unique suffix and suggests a process id; the index disambiguates the
        // several players one process exports. Each dot-separated element has
        // to start with a non-digit, which is what `instance` is doing here
        // besides matching what every other player does.
        let suffix = format!("azzurro.instance{}_{index}", std::process::id());

        let exported = Exported {
            id,
            name,
            registry,
            commands,
        };

        match Server::new(&suffix, exported).await {
            Ok(server) => {
                tracing::info!(%id, bus = %server.bus_name(), "exported over MPRIS");
                Some(Self {
                    server,
                    announced: Mutex::new(None),
                })
            }
            Err(e) => {
                tracing::warn!(%id, "no MPRIS for this player: {e}");
                None
            }
        }
    }

    /// Announce whatever changed since the last status.
    pub async fn publish(&self, status: &Status) {
        let art = self.server.imp().art_url(status);
        let current = Announced {
            playback: playback_status(status),
            loop_status: loop_status(status),
            shuffle: status.shuffle_on(),
            volume_percent: status.volume.unwrap_or(0),
            can_seek: status.seekable(),
            track: track_key(status, art.clone()),
            position_secs: status.secs.unwrap_or(0) as i64,
        };

        let (properties, seeked) = {
            let mut guard = self.announced.lock().unwrap();
            let diff = diff(guard.as_ref(), &current, status, art);
            *guard = Some(current);
            diff
        };

        if !properties.is_empty()
            && let Err(e) = self.server.properties_changed(properties).await
        {
            tracing::debug!("MPRIS properties_changed failed: {e}");
        }

        if let Some(position) = seeked
            && let Err(e) = self.server.emit(Signal::Seeked { position }).await
        {
            tracing::debug!("MPRIS Seeked failed: {e}");
        }
    }

    /// Say the player has stopped, without disturbing anything else.
    ///
    /// Used when a poll fails: the track stays on the bus so a brief network
    /// blip does not empty the desktop's media widget, but the player stops
    /// claiming to be playing something nobody can currently see.
    pub async fn publish_offline(&self) {
        let changed = {
            let mut guard = self.announced.lock().unwrap();
            match guard.as_mut() {
                Some(announced) if announced.playback != PlaybackStatus::Stopped => {
                    announced.playback = PlaybackStatus::Stopped;
                    true
                }
                _ => false,
            }
        };

        if changed
            && let Err(e) = self
                .server
                .properties_changed([Property::PlaybackStatus(PlaybackStatus::Stopped)])
                .await
        {
            tracing::debug!("MPRIS offline announcement failed: {e}");
        }
    }
}

/// Work out what to announce, and whether the position moved by more than
/// playback alone can explain.
fn diff(
    previous: Option<&Announced>,
    current: &Announced,
    status: &Status,
    art: Option<String>,
) -> (Vec<Property>, Option<Time>) {
    let metadata = || metadata(status, art.clone());

    let Some(previous) = previous else {
        // First announcement: send everything, so a client that connected
        // before the first poll landed is not left with defaults.
        return (
            vec![
                Property::PlaybackStatus(current.playback),
                Property::LoopStatus(current.loop_status),
                Property::Shuffle(current.shuffle),
                Property::Volume(volume(current.volume_percent)),
                Property::CanSeek(current.can_seek),
                Property::Metadata(metadata()),
            ],
            None,
        );
    };

    let mut properties = Vec::new();
    if previous.playback != current.playback {
        properties.push(Property::PlaybackStatus(current.playback));
    }
    if previous.loop_status != current.loop_status {
        properties.push(Property::LoopStatus(current.loop_status));
    }
    if previous.shuffle != current.shuffle {
        properties.push(Property::Shuffle(current.shuffle));
    }
    if previous.volume_percent != current.volume_percent {
        properties.push(Property::Volume(volume(current.volume_percent)));
    }
    if previous.can_seek != current.can_seek {
        properties.push(Property::CanSeek(current.can_seek));
    }

    let same_track = previous.track == current.track;
    if !same_track {
        properties.push(Property::Metadata(metadata()));
    }

    // Only a jump *within* one track is a seek. A track change moves the
    // position too, and announcing that as a seek would make clients rewind a
    // progress bar they are about to rebuild anyway.
    let seeked = (same_track
        && (current.position_secs - previous.position_secs).abs() > SEEK_TOLERANCE_SECS)
        .then(|| Time::from_secs(current.position_secs));

    (properties, seeked)
}

fn playback_status(status: &Status) -> PlaybackStatus {
    match status.state.as_deref() {
        Some("play" | "stream") => PlaybackStatus::Playing,
        Some("pause") => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

fn loop_status(status: &Status) -> LoopStatus {
    match status.repeat.map(Repeat::from_status) {
        Some(Repeat::All) => LoopStatus::Playlist,
        Some(Repeat::One) => LoopStatus::Track,
        _ => LoopStatus::None,
    }
}

fn volume(percent: i32) -> Volume {
    percent.clamp(0, 100) as Volume / 100.0
}

fn length_micros(status: &Status) -> Option<i64> {
    status
        .totlen
        .filter(|t| *t > 0.0)
        .map(|t| (t * 1_000_000.0) as i64)
}

/// The display lines a player fills in depend on the source, so fall back the
/// way the official controller does: an explicit artist beats the second line,
/// which beats nothing.
fn artist_of(status: &Status) -> Option<String> {
    status
        .artist
        .clone()
        .or_else(|| status.title2.clone())
        .filter(|s| !s.is_empty())
}

fn album_of(status: &Status) -> Option<String> {
    status
        .album
        .clone()
        .or_else(|| status.title3.clone())
        .filter(|s| !s.is_empty())
}

fn track_key(status: &Status, art: Option<String>) -> TrackKey {
    TrackKey {
        title: status.title1.clone().filter(|s| !s.is_empty()),
        artist: artist_of(status),
        album: album_of(status),
        art,
        length_micros: length_micros(status),
    }
}

/// A path that changes exactly when the track does.
///
/// The queue position is the natural identity where there is a queue. A radio
/// stream has none — the queue index sits still while the title changes every
/// few minutes — so there the title is hashed instead, which gives clients the
/// track change they are watching for.
fn track_id(status: &Status) -> TrackId {
    let key = match (status.pid, status.song) {
        (Some(queue), Some(index)) if status.service.as_deref() != Some("Capture") => {
            format!("q{queue}s{index}")
        }
        _ => match status.title1.as_deref().filter(|s| !s.is_empty()) {
            Some(title) => format!("t{:016x}", fnv1a(title)),
            None => return TrackId::NO_TRACK,
        },
    };

    TrackId::try_from(format!("/app/azzurro/track/{key}")).unwrap_or(TrackId::NO_TRACK)
}

/// FNV-1a, inlined to keep a hashing crate out of the graph for something whose
/// only requirement is that different titles usually differ.
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn metadata(status: &Status, art: Option<String>) -> Metadata {
    let mut builder = Metadata::builder().trackid(track_id(status));

    if let Some(title) = status.title1.as_deref().filter(|s| !s.is_empty()) {
        builder = builder.title(title);
    }
    if let Some(artist) = artist_of(status) {
        builder = builder.artist([artist]);
    }
    if let Some(album) = album_of(status) {
        builder = builder.album(album);
    }
    if let Some(art) = art {
        builder = builder.art_url(art);
    }
    if let Some(micros) = length_micros(status) {
        builder = builder.length(Time::from_micros(micros));
    }

    builder.build()
}

/// The object that actually answers D-Bus calls.
struct Exported {
    id: DeviceId,
    name: String,
    registry: Registry,
    commands: mpsc::UnboundedSender<Command>,
}

impl Exported {
    /// The last status this player reported, and when it arrived.
    fn snapshot(&self) -> Option<(Status, Instant)> {
        let guard = self.registry.lock().unwrap();
        let entry = guard.get(&self.id)?;
        Some((entry.status.clone()?, entry.status_at?))
    }

    fn art_url(&self, status: &Status) -> Option<String> {
        let art = status.artwork()?;
        let guard = self.registry.lock().unwrap();
        Some(guard.get(&self.id)?.client.image_url(art))
    }

    fn send(&self, action: Action) -> fdo::Result<()> {
        self.commands
            .send(Command::Player(self.id, action))
            .map_err(|_| fdo::Error::Failed("the controller is shutting down".into()))
    }

    /// Where the track is now.
    ///
    /// The player only reports a position when it sends a status, so a playing
    /// track's position is that report plus the time since it arrived. Without
    /// this every client's progress bar would sit still between polls and then
    /// jump.
    fn position_secs(&self) -> i64 {
        let Some((status, at)) = self.snapshot() else {
            return 0;
        };
        let reported = status.secs.unwrap_or(0) as i64;
        if !status.is_playing() {
            return reported;
        }

        let elapsed = at.elapsed().as_secs() as i64;
        match length_micros(&status) {
            Some(micros) => (reported + elapsed).min(micros / 1_000_000),
            None => reported + elapsed,
        }
    }
}

impl RootInterface for Exported {
    async fn raise(&self) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "Azzurro cannot raise itself".into(),
        ))
    }

    // Deliberately false. One speaker's media widget is the wrong place to
    // offer to close a controller that is driving four others.
    async fn quit(&self) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "Azzurro cannot be quit over MPRIS".into(),
        ))
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> ZbusResult<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    // Raising a window is not something a Wayland client can do for itself.
    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    /// The speaker's own name, not the app's: in a desktop media list beside
    /// Firefox and a music player, "Kitchen" is the useful label.
    async fn identity(&self) -> fdo::Result<String> {
        Ok(self.name.clone())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok("azzurro".into())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl PlayerInterface for Exported {
    async fn next(&self) -> fdo::Result<()> {
        self.send(Action::Next)
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.send(Action::Previous)
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.send(Action::Pause)
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        self.send(Action::Toggle)
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.send(Action::Stop)
    }

    async fn play(&self) -> fdo::Result<()> {
        self.send(Action::Play)
    }

    /// MPRIS seeks by an offset and BluOS seeks to a position, so this needs
    /// the current one. A negative offset past the start clamps to zero, which
    /// is what the spec asks for.
    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        let target = (self.position_secs() + offset.as_secs()).max(0);
        self.send(Action::Seek(target as u32))
    }

    async fn set_position(&self, _track_id: TrackId, position: Time) -> fdo::Result<()> {
        self.send(Action::Seek(position.as_secs().max(0) as u32))
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "Azzurro cannot open URIs yet".into(),
        ))
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(self
            .snapshot()
            .map(|(s, _)| playback_status(&s))
            .unwrap_or(PlaybackStatus::Stopped))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(self
            .snapshot()
            .map(|(s, _)| loop_status(&s))
            .unwrap_or(LoopStatus::None))
    }

    async fn set_loop_status(&self, loop_status: LoopStatus) -> ZbusResult<()> {
        let mode = match loop_status {
            LoopStatus::None => Repeat::Off,
            LoopStatus::Track => Repeat::One,
            LoopStatus::Playlist => Repeat::All,
        };
        let _ = self.send(Action::Repeat(mode));
        Ok(())
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    /// BluOS has no rate control, and the spec's answer for that is to pin the
    /// minimum and maximum to 1.0 and ignore writes.
    async fn set_rate(&self, _rate: PlaybackRate) -> ZbusResult<()> {
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self
            .snapshot()
            .map(|(s, _)| s.shuffle_on())
            .unwrap_or(false))
    }

    async fn set_shuffle(&self, shuffle: bool) -> ZbusResult<()> {
        let _ = self.send(Action::Shuffle(shuffle));
        Ok(())
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(match self.snapshot() {
            Some((status, _)) => {
                let art = self.art_url(&status);
                metadata(&status, art)
            }
            None => Metadata::new(),
        })
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self
            .snapshot()
            .map(|(s, _)| volume(s.volume.unwrap_or(0)))
            .unwrap_or(0.0))
    }

    async fn set_volume(&self, volume: Volume) -> ZbusResult<()> {
        let _ = self.send(Action::Volume(
            (volume.clamp(0.0, 1.0) * 100.0).round() as i32
        ));
        Ok(())
    }

    async fn position(&self) -> fdo::Result<Time> {
        Ok(Time::from_secs(self.position_secs()))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    // BluOS accepts skip and back against every source; where they are
    // meaningless — a physical input — it ignores them rather than erroring.
    // `/Status` carries an `<actions>` list that says which are live, and
    // reading it would let these two be honest.
    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().map(|(s, _)| s.seekable()).unwrap_or(false))
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: &str) -> Status {
        Status {
            state: Some(state.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn maps_every_transport_state() {
        assert_eq!(playback_status(&status("play")), PlaybackStatus::Playing);
        // A radio stream reports `stream`, not `play`, and is still playing.
        assert_eq!(playback_status(&status("stream")), PlaybackStatus::Playing);
        assert_eq!(playback_status(&status("pause")), PlaybackStatus::Paused);
        assert_eq!(playback_status(&status("stop")), PlaybackStatus::Stopped);
        assert_eq!(
            playback_status(&status("connecting")),
            PlaybackStatus::Stopped
        );
        assert_eq!(playback_status(&Status::default()), PlaybackStatus::Stopped);
    }

    /// BluOS numbers repeat 0=all, 1=one, 2=off, which is not the order anyone
    /// guesses. Confirmed against the official controller's own labels, where
    /// `repeat === 0` renders "Repeat All" and `repeat === 2` renders "Off".
    #[test]
    fn maps_repeat_without_getting_off_and_all_backwards() {
        let with = |r| Status {
            repeat: Some(r),
            ..Default::default()
        };
        assert_eq!(loop_status(&with(0)), LoopStatus::Playlist);
        assert_eq!(loop_status(&with(1)), LoopStatus::Track);
        assert_eq!(loop_status(&with(2)), LoopStatus::None);
        assert_eq!(loop_status(&Status::default()), LoopStatus::None);
    }

    #[test]
    fn volume_spans_zero_to_one_and_clamps() {
        assert_eq!(volume(0), 0.0);
        assert_eq!(volume(31), 0.31);
        assert_eq!(volume(100), 1.0);
        assert_eq!(volume(140), 1.0);
        assert_eq!(volume(-5), 0.0);
    }

    #[test]
    fn track_id_follows_the_queue_where_there_is_one() {
        let queued = Status {
            pid: Some(692),
            song: Some(3),
            service: Some("TuneIn".into()),
            title1: Some("Anything".into()),
            ..Default::default()
        };
        assert_eq!(
            track_id(&queued).into_inner().as_str(),
            "/app/azzurro/track/q692s3"
        );
    }

    #[test]
    fn track_id_follows_the_title_on_an_input_or_a_stream() {
        // A physical input keeps one queue position forever, so the queue
        // cannot say when the track changed. Two different titles have to give
        // two different ids or clients never see a track change.
        let one = Status {
            pid: Some(692),
            song: Some(0),
            service: Some("Capture".into()),
            title1: Some("HDMI ARC".into()),
            ..Default::default()
        };
        let two = Status {
            title1: Some("Optical In".into()),
            ..one.clone()
        };
        assert_ne!(track_id(&one), track_id(&two));
        assert_eq!(track_id(&one), track_id(&one.clone()));

        // Nothing to identify at all.
        assert_eq!(track_id(&Status::default()), TrackId::NO_TRACK);
    }

    #[test]
    fn length_is_only_reported_when_the_player_knows_it() {
        assert_eq!(length_micros(&Status::default()), None);
        // A live stream reports zero rather than omitting the field.
        assert_eq!(
            length_micros(&Status {
                totlen: Some(0.0),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            length_micros(&Status {
                totlen: Some(245.5),
                ..Default::default()
            }),
            Some(245_500_000)
        );
    }

    #[test]
    fn falls_back_through_the_display_lines() {
        let s = Status {
            title2: Some("A Band".into()),
            title3: Some("A Record".into()),
            ..Default::default()
        };
        assert_eq!(artist_of(&s).as_deref(), Some("A Band"));
        assert_eq!(album_of(&s).as_deref(), Some("A Record"));

        // An explicit artist wins over the second display line.
        let s = Status {
            artist: Some("The Real Artist".into()),
            ..s
        };
        assert_eq!(artist_of(&s).as_deref(), Some("The Real Artist"));

        // Empty is not a value.
        assert_eq!(
            artist_of(&Status {
                title2: Some(String::new()),
                ..Default::default()
            }),
            None
        );
    }
}
