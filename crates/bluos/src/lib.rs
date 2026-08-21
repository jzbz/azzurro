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

pub mod client;
pub mod device;
pub mod discovery;
pub mod error;
pub mod lsdp;
pub mod queue;
pub mod screen;
pub mod status;

pub use client::{Client, Repeat, StatusWatch};
pub use device::DeviceId;
pub use discovery::Discovery;
pub use error::{Error, Result};
pub use lsdp::Announce;
pub use queue::{Queue, QueueSong};
pub use screen::{Action, ActionKind, Configuration, Item, ItemKind, Screen, Section};
pub use status::{Status, SyncStatus};

/// The control API's port. Players advertise it in their LSDP announcement and
/// have never been seen to use another, but the announcement is authoritative.
pub const DEFAULT_PORT: u16 = 11000;
