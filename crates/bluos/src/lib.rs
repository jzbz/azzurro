//! Discovery and control for BluOS players — Bluesound, NAD and PSB.
//!
//! BluOS has no published protocol. This crate is written against what the
//! players themselves say and what the official controller does with it; see
//! `docs/protocol.md` in the repository for what was observed directly and
//! what was transcribed.
//!
//! Nothing here draws anything. The types that cross into a GUI are this
//! crate's own, so the front end never sees an XML parser or an HTTP client.
//!
//! ```no_run
//! # async fn example() -> bluos::Result<()> {
//! use bluos::{Client, Discovery, discovery::DEFAULT_SWEEP};
//!
//! let discovery = Discovery::bind()?;
//! for announce in discovery.sweep(DEFAULT_SWEEP).await? {
//!     let Some(player) = announce.player() else { continue };
//!     let client = Client::new(bluos::DeviceId::new(announce.address, player.port()))?;
//!
//!     println!("{}", client.sync_status().await?.name);
//!
//!     // Blocks until this player does something.
//!     let mut watch = client.watch();
//!     let status = watch.next().await?;
//!     println!("{:?}", status.now_playing());
//! }
//! # Ok(())
//! # }
//! ```

pub mod alarms;
pub mod client;
pub mod device;
pub mod dialog;
pub mod discovery;
pub mod error;
pub mod forms;
pub mod lsdp;
pub mod playlists;
pub mod presets;
pub mod queue;
pub mod reports;
pub mod screen;
pub mod settings;
pub mod stations;
pub mod status;
pub mod upgrade;

/// Shared by the two HTML scrapers; not part of the crate's surface.
mod html;

/// Shared by the two XML parsers; not part of the crate's surface.
mod xml;

pub use client::{Client, Repeat, StatusWatch};
pub use device::DeviceId;
pub use discovery::Discovery;
pub use error::{Error, Result};
pub use lsdp::Announce;
pub use queue::{Queue, QueueSong};
pub use screen::{Action, ActionKind, Configuration, Item, ItemKind, Screen, Section};
pub use status::{Status, SyncStatus};

/// Seconds as a clock: `4:03`, or `1:02:17` once it runs past an hour.
///
/// Negative input clamps to zero, because a position can briefly read past the
/// end of a track and the difference is not worth showing as `-0:02`.
pub fn clock(seconds: i64) -> String {
    let total = seconds.max(0);
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The control API's port. Players advertise it in their LSDP announcement and
/// have never been seen to use another, but the announcement is authoritative.
pub const DEFAULT_PORT: u16 = 11000;
