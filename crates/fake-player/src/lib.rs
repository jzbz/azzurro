//! A BluOS player that is not there.
//!
//! Everything this app does begins with an HTTP request to a speaker, which
//! makes almost none of it testable: a test would need a Powernode on the
//! network, in a known state, answering the same way twice. So this answers
//! instead. It binds a port on the loopback, serves documents in the shapes a
//! real player uses, and lets a test say what those documents contain.
//!
//! ```no_run
//! # async fn go() {
//! let player = fake_player::Player::start().await;
//! let client = bluos::client::Client::new(player.id()).unwrap();
//! let status = client.sync_status().await.unwrap();
//! # }
//! ```
//!
//! Deliberately hand-written rather than captured. A capture off a real player
//! carries whoever's library it was — track titles, file paths, the address of
//! their NAS — and none of that belongs in a repository. The shapes here were
//! taken from documents a Powernode on BluOS 4.16.6 served; the contents are
//! invented.
//!
//! No HTTP crate. The server speaks the small part of HTTP/1.1 that `reqwest`
//! needs for these requests, in about a hundred lines, which is cheaper than
//! another dependency in a build that vendors every crate for an offline
//! Flatpak.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub mod fixtures;

/// What the player answers, by path.
///
/// Keyed on the path alone, without its query: a test that wants to tell two
/// queries apart can install a route for the path and look at what it was
/// asked. Requests to a path with no route get 404, which is what a real
/// player does for a route it does not have.
#[derive(Default)]
struct State {
    routes: HashMap<String, String>,
    /// Every path-and-query asked for, in order, so a test can assert on what
    /// the app actually sent rather than only on what it did with the reply.
    seen: Vec<String>,
}

/// A player on the loopback.
pub struct Player {
    port: u16,
    state: Arc<Mutex<State>>,
}

impl Player {
    /// Start one, answering the documents a player answers at rest.
    pub async fn start() -> Self {
        Self::with_routes(fixtures::at_rest()).await
    }

    /// Start one with exactly these routes and nothing else.
    pub async fn with_routes(routes: Vec<(&str, String)>) -> Self {
        let state = Arc::new(Mutex::new(State {
            routes: routes
                .into_iter()
                .map(|(path, body)| (path.to_owned(), body))
                .collect(),
            seen: Vec::new(),
        }));

        // Port zero: the kernel picks a free one, so tests can run at the same
        // time as each other and as a real player.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("bound").port();

        let serving = state.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = serving.clone();
                tokio::spawn(async move {
                    let _ = answer(stream, state).await;
                });
            }
        });

        Self { port, state }
    }

    /// Where it is, in the form the rest of the crate uses.
    pub fn id(&self) -> bluos::DeviceId {
        bluos::DeviceId::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port)
    }

    pub fn address(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port)
    }

    /// Change what a path answers, from here on.
    pub fn serve(&self, path: &str, body: impl Into<String>) {
        self.state
            .lock()
            .unwrap()
            .routes
            .insert(path.to_owned(), body.into());
    }

    /// Stop answering a path at all, so it 404s.
    pub fn forget(&self, path: &str) {
        self.state.lock().unwrap().routes.remove(path);
    }

    /// Everything asked for so far, path and query, in order.
    pub fn asked(&self) -> Vec<String> {
        self.state.lock().unwrap().seen.clone()
    }

    /// Whether any request so far carried this substring — the usual way to
    /// ask "did it send the parameter I expected".
    pub fn asked_for(&self, fragment: &str) -> bool {
        self.asked().iter().any(|seen| seen.contains(fragment))
    }
}

/// One request, one reply.
async fn answer(mut stream: TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    // Enough for a request line and headers. Nothing here reads a body: every
    // route a player offers is a GET, including the ones that change something.
    let mut buffer = vec![0u8; 8192];
    let read = stream.read(&mut buffer).await?;
    let head = String::from_utf8_lossy(&buffer[..read]).into_owned();

    let target = head
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let path = target.split('?').next().unwrap_or_default().to_owned();

    let body = {
        let mut state = state.lock().unwrap();
        state.seen.push(target.clone());
        state.routes.get(&path).cloned()
    };

    let reply = match body {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/xml\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        ),
        None => {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
        }
    };

    stream.write_all(reply.as_bytes()).await?;
    stream.flush().await
}
