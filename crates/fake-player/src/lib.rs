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
use std::time::Duration;

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
    /// Every request head in full, for the things a target cannot show — a
    /// header the client is supposed to be carrying, chiefly.
    heads: Vec<String>,
    /// A `X-Sovi-Ui-Context` to put on every reply, if the test wants one.
    context: Option<String>,
    /// How long to sit on a request before answering it, by path.
    ///
    /// A real player is not instant, and one that is waking or busy can take
    /// seconds. Whatever the app does while a reply is on its way — the user
    /// choosing another player, most of all — is only reachable in a test if
    /// the reply can be made to arrive late.
    delays: HashMap<String, Duration>,
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
            heads: Vec::new(),
            context: None,
            delays: HashMap::new(),
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

    /// Answer a path only after `by` has passed, from here on.
    ///
    /// The request is recorded as asked for the moment it arrives, so a test
    /// can tell a reply that is on its way from one that was never sent for.
    pub fn delay(&self, path: &str, by: Duration) {
        self.state
            .lock()
            .unwrap()
            .delays
            .insert(path.to_owned(), by);
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

    /// Put a `X-Sovi-Ui-Context` on every reply from here on.
    ///
    /// This is what a real player does when a filter is chosen: the state is
    /// handed to the client to carry, not kept.
    pub fn hand_out_context(&self, value: impl Into<String>) {
        self.state.lock().unwrap().context = Some(value.into());
    }

    /// Stop putting a context on replies, without forgetting what was sent.
    pub fn state_without_context(&self) {
        self.state.lock().unwrap().context = None;
    }

    /// Every request head so far, in full.
    pub fn heads(&self) -> Vec<String> {
        self.state.lock().unwrap().heads.clone()
    }

    /// Whether the most recent request carried this text anywhere in its head.
    pub fn last_head_had(&self, fragment: &str) -> bool {
        self.heads().last().is_some_and(|head| {
            head.to_ascii_lowercase()
                .contains(&fragment.to_ascii_lowercase())
        })
    }
}

/// One request, one reply.
async fn answer(mut stream: TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    // Read to the end of the head before answering, and not merely once.
    //
    // Windows sends RST rather than FIN when a socket is closed with unread
    // data still in its receive buffer, and the client then loses the reply it
    // was already sent — WSAECONNABORTED in place of the body. A single read
    // usually takes the whole of a GET and sometimes does not, and "usually"
    // is how a test earns a reputation for being flaky on one platform.
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
        // Nothing a player offers sends a body, so the head is the request.
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();

    let target = head
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let path = target.split('?').next().unwrap_or_default().to_owned();

    let (body, context, delay) = {
        let mut state = state.lock().unwrap();
        state.seen.push(target.clone());
        state.heads.push(head.clone());
        (
            state.routes.get(&path).cloned(),
            state.context.clone(),
            state.delays.get(&path).copied(),
        )
    };

    // Read which route to answer before waiting, and the body too: what a
    // player sends is what it had when it was asked.
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }

    let carried = match &context {
        Some(value) => format!("X-Sovi-Ui-Context: {value}\r\n"),
        None => String::new(),
    };

    let reply = match body {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/xml\r\n\
             {carried}\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        ),
        None => {
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
        }
    };

    stream.write_all(reply.as_bytes()).await?;
    stream.flush().await?;
    // Close the writing half deliberately rather than letting the drop do it,
    // so the client sees an orderly end to the body.
    stream.shutdown().await
}
