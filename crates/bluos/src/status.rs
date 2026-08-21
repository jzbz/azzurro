//! The two documents a controller reads constantly.
//!
//! `/SyncStatus` describes the player — what it is, what it is called, how loud
//! it is, and who it is grouped with. `/Status` describes what it is doing.
//! Both carry an `etag`, and `/Status` accepts one back to long-poll on; see
//! [`crate::StatusWatch`].
//!
//! Every field beyond identity is optional. A player on an HDMI input reports
//! nothing about artists or track length; one playing a stream reports no input
//! id. Modelling that as `Option` rather than empty strings keeps "the player
//! did not say" distinct from "the player said nothing".

use serde::Deserialize;

/// `/SyncStatus` — the player itself, and its grouping.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncStatus {
    #[serde(rename = "@etag")]
    pub etag: String,
    /// The player's own idea of its address, as `host:port`.
    #[serde(rename = "@id")]
    pub id: String,
    #[serde(rename = "@name")]
    pub name: String,
    /// Short model code, e.g. `N330`.
    #[serde(rename = "@model")]
    pub model: String,
    /// Marketing name, e.g. `POWERNODE`.
    #[serde(rename = "@modelName")]
    pub model_name: Option<String>,
    #[serde(rename = "@brand")]
    pub brand: Option<String>,
    /// Broad kind: `streamer-amplifier`, `speaker`, and so on. Worth showing,
    /// and worth using to pick an icon when the artwork below is unreachable.
    #[serde(rename = "@class")]
    pub class: Option<String>,
    /// Player artwork, as a path on the player itself.
    #[serde(rename = "@icon")]
    pub icon: Option<String>,
    #[serde(rename = "@mac")]
    pub mac: Option<String>,
    /// BluOS firmware version.
    #[serde(rename = "@version")]
    pub version: Option<String>,
    #[serde(rename = "@volume")]
    pub volume: Option<i32>,
    #[serde(rename = "@db")]
    pub db: Option<f32>,
    #[serde(rename = "@schemaVersion")]
    pub schema_version: Option<u32>,
    #[serde(rename = "@initialized")]
    pub initialized: Option<bool>,

    /// Present when this player is a slave in a group: the master's address.
    ///
    /// Not observed on the ungrouped player this crate was developed against —
    /// the shape here follows the official controller. Absent fields decode as
    /// `None` either way, so a wrong guess here is silent rather than fatal.
    pub master: Option<Master>,
    /// Present when this player is a group master: one entry per slave.
    #[serde(default, rename = "slave")]
    pub slaves: Vec<Slave>,

    #[serde(rename = "zoneOptions")]
    pub zone_options: Option<ZoneOptions>,
}

impl SyncStatus {
    /// The name to show, preferring the marketing name over the model code.
    pub fn display_model(&self) -> &str {
        self.model_name.as_deref().unwrap_or(&self.model)
    }

    pub fn is_grouped(&self) -> bool {
        self.master.is_some() || !self.slaves.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Master {
    #[serde(rename = "@port")]
    pub port: Option<u16>,
    #[serde(rename = "$text")]
    pub host: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Slave {
    #[serde(rename = "@id")]
    pub id: Option<String>,
    #[serde(rename = "@port")]
    pub port: Option<u16>,
}

/// Where this player can sit in a stereo or surround zone.
#[derive(Debug, Clone, Deserialize)]
pub struct ZoneOptions {
    #[serde(default, rename = "option")]
    pub options: Vec<ZoneOption>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoneOption {
    #[serde(rename = "@canHaveCentre")]
    pub can_have_centre: Option<bool>,
    #[serde(rename = "@zoneMaster")]
    pub zone_master: Option<bool>,
    /// `front`, `side`, `left`, `right`, and so on.
    #[serde(rename = "$text")]
    pub position: String,
}

/// `/Status` — what the player is doing right now.
#[derive(Debug, Clone, Deserialize)]
pub struct Status {
    /// Changes whenever anything below does. Hand it back to
    /// [`crate::StatusWatch`] to block until it does.
    #[serde(rename = "@etag")]
    pub etag: String,

    /// `play`, `pause`, `stop`, `stream`, `connecting`.
    pub state: Option<String>,
    pub volume: Option<i32>,
    pub db: Option<f32>,
    pub mute: Option<u8>,
    /// 0 off, 1 on.
    pub shuffle: Option<u8>,
    /// 0 all, 1 one, 2 off — note that 2, not 0, is the off value.
    pub repeat: Option<u8>,
    /// Elapsed seconds.
    pub secs: Option<u32>,
    /// Track length in seconds. Fractional on some services.
    pub totlen: Option<f64>,
    #[serde(rename = "canSeek")]
    pub can_seek: Option<u8>,

    /// The three display lines, in the player's own priority order. What lands
    /// in each depends on the source: on a stream `title1` is often the track
    /// and `title2` the artist, while on an input `title1` is the input name.
    pub title1: Option<String>,
    pub title2: Option<String>,
    pub title3: Option<String>,
    /// Present on library and service playback, where the split is unambiguous.
    pub artist: Option<String>,
    pub album: Option<String>,
    pub name: Option<String>,

    /// Cover art, as a path on the player or an absolute URL at a service CDN.
    /// Resolve with [`crate::Client::image_url`].
    pub image: Option<String>,
    #[serde(rename = "currentImage")]
    pub current_image: Option<String>,
    #[serde(rename = "stationImage")]
    pub station_image: Option<String>,

    /// Which service is playing: `Capture` for a physical input, `TuneIn`,
    /// `RadioParadise`, `LocalMusic`, and so on.
    pub service: Option<String>,
    #[serde(rename = "serviceType")]
    pub service_type: Option<String>,
    #[serde(rename = "streamUrl")]
    pub stream_url: Option<String>,
    #[serde(rename = "streamFormat")]
    pub stream_format: Option<String>,
    pub quality: Option<String>,

    #[serde(rename = "inputId")]
    pub input_id: Option<String>,

    /// Index of the playing track in the queue.
    pub song: Option<u32>,
    /// Total queue length, as the player counts it.
    pub cursor: Option<u32>,
    /// Queue identity. Changes when the queue is replaced.
    pub pid: Option<u32>,
    /// Preset list identity.
    pub prid: Option<u32>,
    /// Service identity.
    pub sid: Option<u32>,
    /// Non-zero while the player is indexing a share.
    pub indexing: Option<u8>,
    /// Minutes left on the sleep timer; empty when it is off.
    pub sleep: Option<String>,
    /// Mirrors `/SyncStatus`'s etag, so a change in grouping is visible from a
    /// status poll without a second request.
    #[serde(rename = "syncStat")]
    pub sync_stat: Option<String>,
}

impl Status {
    pub fn is_playing(&self) -> bool {
        matches!(self.state.as_deref(), Some("play" | "stream"))
    }

    pub fn is_muted(&self) -> bool {
        self.mute.unwrap_or(0) != 0
    }

    pub fn shuffle_on(&self) -> bool {
        self.shuffle.unwrap_or(0) != 0
    }

    /// Seekable *and* long enough for a position to mean anything. A live
    /// stream reports `canSeek` 0 and no length.
    pub fn seekable(&self) -> bool {
        self.can_seek.unwrap_or(0) != 0 && self.totlen.unwrap_or(0.0) > 0.0
    }

    /// Elapsed fraction of the track, if both ends are known.
    pub fn progress(&self) -> Option<f32> {
        let total = self.totlen.filter(|t| *t > 0.0)?;
        Some((self.secs? as f64 / total).clamp(0.0, 1.0) as f32)
    }

    /// The best available cover art path, in the order the official controller
    /// prefers them.
    pub fn artwork(&self) -> Option<&str> {
        self.image
            .as_deref()
            .or(self.current_image.as_deref())
            .or(self.station_image.as_deref())
            .filter(|s| !s.is_empty())
    }

    /// A single line for a notification or an MPRIS title.
    pub fn now_playing(&self) -> Option<String> {
        let title = self.title1.as_deref().filter(|s| !s.is_empty())?;
        match self.artist.as_deref().or(self.title2.as_deref()) {
            Some(by) if !by.is_empty() && by != title => Some(format!("{title} — {by}")),
            _ => Some(title.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from an NAD Powernode N330 on BluOS 4.16.6, sitting on its HDMI
    /// ARC input, with the address and MAC replaced by documentation values.
    const SYNC_STATUS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SyncStatus etag="97" syncStat="97" version="4.16.6" id="192.0.2.155:11000" db="-30.6" volume="31" name="Powernode" model="N330" modelName="POWERNODE" class="streamer-amplifier" icon="/images/players/N225_nt.png" brand="Bluesound" schemaVersion="34" initialized="true" mac="AA:BB:CC:DD:EE:FF"><zoneOptions><option canHaveCentre="true" zoneMaster="true">front</option><option zoneMaster="true">side</option></zoneOptions><pairWithSub></pairWithSub><bluetoothOutput></bluetoothOutput></SyncStatus>"#;

    const STATUS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<status etag="8a52c91ed3074a395d457626b80b20c2"><actions><action name="back" state="0"></action></actions><canSeek>0</canSeek><currentImage>/images/capture/ic_tvNP.png</currentImage><cursor>41</cursor><db>-30.6</db><image>/images/capture/ic_tvNP.png</image><indexing>0</indexing><inputId>input4</inputId><inputTypeIndex>arc-1</inputTypeIndex><mid>185</mid><mode>1</mode><mute>0</mute><pid>692</pid><prid>0</prid><repeat>0</repeat><secs>0</secs><service>Capture</service><serviceType>AudioInputs</serviceType><settingsGroupId>capture-input4</settingsGroupId><shuffle>0</shuffle><sid>8</sid><sleep></sleep><song>0</song><state>pause</state><stationImage>/images/capture/ic_tvNP.png</stationImage><streamUrl>Capture:hw:imxspdif,0/1/25/2?id=input4</streamUrl><syncStat>97</syncStat><title1>HDMI ARC</title1><twoline_title1>HDMI ARC</twoline_title1><volume>31</volume></status>"#;

    #[test]
    fn reads_a_real_sync_status() {
        let s: SyncStatus = quick_xml::de::from_str(SYNC_STATUS).unwrap();
        assert_eq!(s.etag, "97");
        assert_eq!(s.id, "192.0.2.155:11000");
        assert_eq!(s.name, "Powernode");
        assert_eq!(s.model, "N330");
        assert_eq!(s.display_model(), "POWERNODE");
        assert_eq!(s.brand.as_deref(), Some("Bluesound"));
        assert_eq!(s.volume, Some(31));
        assert_eq!(s.db, Some(-30.6));
        assert_eq!(s.initialized, Some(true));
        assert!(!s.is_grouped());

        let zones = s.zone_options.expect("this model can be zoned");
        assert_eq!(zones.options.len(), 2);
        assert_eq!(zones.options[0].position, "front");
        assert_eq!(zones.options[0].can_have_centre, Some(true));
        assert_eq!(zones.options[1].can_have_centre, None);
    }

    #[test]
    fn reads_a_real_status() {
        let s: Status = quick_xml::de::from_str(STATUS).unwrap();
        assert_eq!(s.etag, "8a52c91ed3074a395d457626b80b20c2");
        assert_eq!(s.state.as_deref(), Some("pause"));
        assert!(!s.is_playing());
        assert_eq!(s.volume, Some(31));
        assert!(!s.is_muted());
        assert_eq!(s.service.as_deref(), Some("Capture"));
        assert_eq!(s.input_id.as_deref(), Some("input4"));
        assert_eq!(s.title1.as_deref(), Some("HDMI ARC"));
        assert_eq!(s.artwork(), Some("/images/capture/ic_tvNP.png"));
        assert_eq!(s.now_playing().as_deref(), Some("HDMI ARC"));

        // An input is not seekable and reports no length, so there is no
        // progress to draw.
        assert!(!s.seekable());
        assert_eq!(s.progress(), None);
        // Fields the player did not send stay absent rather than becoming "".
        assert_eq!(s.artist, None);
        assert_eq!(s.totlen, None);
    }

    #[test]
    fn progress_needs_both_ends() {
        let mut s: Status = quick_xml::de::from_str(STATUS).unwrap();
        s.secs = Some(30);
        s.totlen = Some(120.0);
        assert_eq!(s.progress(), Some(0.25));
        // Past the end — a player can report this briefly at a track change.
        s.secs = Some(200);
        assert_eq!(s.progress(), Some(1.0));
    }
}
