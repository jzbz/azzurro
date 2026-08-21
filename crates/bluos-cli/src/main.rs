//! `bluosctl` — the protocol crate with a command line bolted on.
//!
//! This exists mostly to be the fast way to check something against real
//! hardware without opening a window, and it is what the GUI's behaviour gets
//! compared against when the two disagree.

use std::time::Duration;

use anyhow::{Context, Result};
use bluos::{Client, DeviceId, Discovery, Repeat, discovery::DEFAULT_SWEEP};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "bluosctl",
    version,
    about = "Control BluOS players from the shell"
)]
struct Cli {
    /// Log at debug level. Discovery in particular is quiet without it.
    #[arg(long, short, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Broadcast for players and list what answers.
    Discover {
        /// How long to keep listening.
        #[arg(long, default_value = "12")]
        seconds: u64,
    },
    /// What a player is: name, model, firmware, grouping.
    Info {
        device: DeviceId,
    },
    /// What a player is doing right now.
    Status {
        device: DeviceId,
    },
    /// Print a line every time a player changes, until interrupted.
    Watch {
        device: DeviceId,
        /// How long each poll is held open.
        #[arg(long, default_value = "100")]
        poll: u64,
    },
    /// Read the volume, or set it to a level from 0 to 100.
    Volume {
        device: DeviceId,
        level: Option<i32>,
    },
    /// Mute or unmute.
    Mute {
        device: DeviceId,
        #[arg(value_parser = clap::value_parser!(bool))]
        on: bool,
    },
    Play {
        device: DeviceId,
    },
    Pause {
        device: DeviceId,
    },
    /// Play or pause, whichever it is not doing.
    Toggle {
        device: DeviceId,
    },
    Stop {
        device: DeviceId,
    },
    Skip {
        device: DeviceId,
    },
    Back {
        device: DeviceId,
    },
    /// Jump to a position in the current track, in seconds.
    Seek {
        device: DeviceId,
        secs: u32,
    },
    /// Load a stored preset by its number.
    Preset {
        device: DeviceId,
        id: u32,
    },
    Shuffle {
        device: DeviceId,
        on: bool,
    },
    /// all, one or off.
    Repeat {
        device: DeviceId,
        mode: String,
    },
    /// Put a player under another one's control.
    Group {
        master: DeviceId,
        slave: DeviceId,
    },
    /// Break a player out of a group.
    Ungroup {
        master: DeviceId,
        slave: DeviceId,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| if cli.verbose { "debug" } else { "warn" }.into()),
        )
        .without_time()
        .init();

    match cli.command {
        Command::Discover { seconds } => discover(seconds).await,
        Command::Info { device } => info(device).await,
        Command::Status { device } => status(device).await,
        Command::Watch { device, poll } => watch(device, poll).await,
        Command::Volume { device, level } => volume(device, level).await,
        Command::Mute { device, on } => client(device)?.set_mute(on).await.map_err(Into::into),
        Command::Play { device } => client(device)?.play().await.map_err(Into::into),
        Command::Pause { device } => client(device)?.pause().await.map_err(Into::into),
        Command::Toggle { device } => client(device)?.toggle().await.map_err(Into::into),
        Command::Stop { device } => client(device)?.stop().await.map_err(Into::into),
        Command::Skip { device } => client(device)?.skip().await.map_err(Into::into),
        Command::Back { device } => client(device)?.back().await.map_err(Into::into),
        Command::Seek { device, secs } => client(device)?.seek(secs).await.map_err(Into::into),
        Command::Preset { device, id } => client(device)?.load_preset(id).await.map_err(Into::into),
        Command::Shuffle { device, on } => {
            client(device)?.set_shuffle(on).await.map_err(Into::into)
        }
        Command::Repeat { device, mode } => {
            let mode = match mode.as_str() {
                "all" => Repeat::All,
                "one" => Repeat::One,
                "off" => Repeat::Off,
                other => anyhow::bail!("repeat takes all, one or off, not {other:?}"),
            };
            client(device)?.set_repeat(mode).await.map_err(Into::into)
        }
        Command::Group { master, slave } => {
            client(master)?.add_slave(slave).await.map_err(Into::into)
        }
        Command::Ungroup { master, slave } => client(master)?
            .remove_slave(slave)
            .await
            .map_err(Into::into),
    }
}

fn client(device: DeviceId) -> Result<Client> {
    Client::new(device).with_context(|| format!("building an HTTP client for {device}"))
}

async fn discover(seconds: u64) -> Result<()> {
    let discovery = Discovery::bind().context(
        "binding UDP 11430 — another controller may already hold it without SO_REUSEPORT",
    )?;
    eprintln!(
        "broadcasting to {} for {seconds}s",
        discovery
            .targets()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let window = if seconds == 0 {
        DEFAULT_SWEEP
    } else {
        Duration::from_secs(seconds)
    };
    let found = discovery.sweep(window).await?;

    if found.is_empty() {
        eprintln!("nothing answered. Players asleep, on another subnet, or broadcast filtered.");
        return Ok(());
    }

    for announce in found {
        let Some(player) = announce.player() else {
            continue;
        };
        let id = DeviceId::new(announce.address, player.port());
        println!(
            "{id}\t{}\t{}\t{}",
            player.get("name").unwrap_or("?"),
            player.get("model").unwrap_or("?"),
            player.get("version").unwrap_or("?"),
        );
    }
    Ok(())
}

async fn info(device: DeviceId) -> Result<()> {
    let s = client(device)?.sync_status().await?;
    println!("name      {}", s.name);
    println!("model     {} ({})", s.display_model(), s.model);
    if let Some(brand) = &s.brand {
        println!("brand     {brand}");
    }
    if let Some(version) = &s.version {
        println!("firmware  {version}");
    }
    if let Some(volume) = s.volume {
        println!("volume    {volume}");
    }
    println!("grouped   {}", if s.is_grouped() { "yes" } else { "no" });
    if let Some(zones) = &s.zone_options {
        let positions: Vec<&str> = zones.options.iter().map(|o| o.position.as_str()).collect();
        println!("zones     {}", positions.join(", "));
    }
    Ok(())
}

async fn status(device: DeviceId) -> Result<()> {
    print_status(&client(device)?.status().await?);
    Ok(())
}

async fn watch(device: DeviceId, poll: u64) -> Result<()> {
    let client = client(device)?;
    let mut watch = client.watch().with_poll(Duration::from_secs(poll));
    // Deliberately not retried: a controller backs off and reconnects here, and
    // seeing the raw failure is the point of this subcommand.
    loop {
        let status = watch.next().await?;
        print_status(&status);
        println!("--");
    }
}

fn print_status(s: &bluos::Status) {
    println!("state     {}", s.state.as_deref().unwrap_or("?"));
    if let Some(now) = s.now_playing() {
        println!("playing   {now}");
    }
    if let Some(service) = &s.service {
        println!("service   {service}");
    }
    if let Some(volume) = s.volume {
        println!(
            "volume    {volume}{}",
            if s.is_muted() { " (muted)" } else { "" }
        );
    }
    if let (Some(secs), Some(total)) = (s.secs, s.totlen) {
        println!("position  {secs}s / {total:.0}s");
    }
    if let Some(art) = s.artwork() {
        println!("artwork   {art}");
    }
    println!("etag      {}", s.etag);
}

async fn volume(device: DeviceId, level: Option<i32>) -> Result<()> {
    let client = client(device)?;
    match level {
        Some(level) => client.set_volume(level).await?,
        None => {
            let status = client.status().await?;
            println!("{}", status.volume.unwrap_or(-1));
        }
    }
    Ok(())
}
