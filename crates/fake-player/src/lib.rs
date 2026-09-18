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
//! needs for these requests, in a couple of hundred lines, which is cheaper
//! than another dependency in a build that vendors every crate for an offline
//! Flatpak. Bodies are read by `Content-Length`; nothing here sends chunked.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub mod fixtures;

/// One request as it arrived, for a route that answers according to what it
/// was asked.
pub struct Request {
    pub method: String,
    pub path: String,
    /// Everything after the `?`, as sent: not percent-decoded, which the
    /// numbers and flags a route looks at never need.
    pub query: String,
    pub body: String,
}

impl Request {
    /// The first value given for `key` on the query, if any.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    }
}

/// A route that works its reply out from the request. `None` is a 404.
type Handler = Arc<dyn Fn(&Request) -> Option<String> + Send + Sync>;

/// What a path answers: the same document every time, or whatever a handler
/// makes of the request.
enum Route {
    Fixed(String),
    Answer(Handler),
}

/// What the player answers, by path.
///
/// Keyed on the path alone, without its query. A path whose reply depends on
/// the query or the method — a window onto the queue, a delete that should
/// come back without what it deleted — takes a handler instead of a document.
/// Requests to a path with no route get 404, which is what a real player does
/// for a route it does not have.
#[derive(Default)]
struct State {
    routes: HashMap<String, Route>,
    /// Every path-and-query asked for, in order, so a test can assert on what
    /// the app actually sent rather than only on what it did with the reply.
    seen: Vec<String>,
    /// Every request head in full, for the things a target cannot show — a
    /// header the client is supposed to be carrying, chiefly. The head stops
    /// at the blank line; what follows it is in `bodies`.
    heads: Vec<String>,
    /// Every request body, in the same order as `heads`, empty for a GET.
    bodies: Vec<String>,
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
                .map(|(path, body)| (path.to_owned(), Route::Fixed(body)))
                .collect(),
            ..State::default()
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
            .insert(path.to_owned(), Route::Fixed(body.into()));
    }

    /// Answer a path by working the reply out from each request, from here on.
    ///
    /// The handler runs outside the fake's lock, so it may keep state of its
    /// own behind one.
    pub fn handle(
        &self,
        path: &str,
        answer: impl Fn(&Request) -> Option<String> + Send + Sync + 'static,
    ) {
        self.state
            .lock()
            .unwrap()
            .routes
            .insert(path.to_owned(), Route::Answer(Arc::new(answer)));
    }

    /// Hold a queue of `length` songs and answer `/Playlist` from it the way a
    /// player does: the whole queue with no window asked for, and with
    /// `start=` and `end=` only the songs between them, **both ends included**.
    /// `length` on the reply is the whole queue either way.
    ///
    /// A window running past the end is cut short rather than refused. What a
    /// player does with only one of the pair has not been seen; this treats the
    /// missing end as open.
    pub fn hold_queue(&self, length: u32) {
        self.handle("/Playlist", move |request| {
            let number = |key| request.param(key).and_then(|v| v.parse::<u32>().ok());
            let start = number("start").unwrap_or(0);
            let end = number("end").unwrap_or(u32::MAX);
            let window: Vec<u32> = (start..length).take_while(|&i| i <= end).collect();
            Some(fixtures::queue_of(length, &window))
        });
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

    /// Every request body so far, in the order the requests arrived.
    pub fn bodies(&self) -> Vec<String> {
        self.state.lock().unwrap().bodies.clone()
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
    // Read the whole request before answering, and not merely once.
    //
    // Windows sends RST rather than FIN when a socket is closed with unread
    // data still in its receive buffer, and the client then loses the reply it
    // was already sent — WSAECONNABORTED in place of the body. A single read
    // usually takes the whole of a GET and sometimes does not, and "usually"
    // is how a test earns a reputation for being flaky on one platform. The
    // same goes for a POST's body, which can arrive in a later read than its
    // head, and which a test asserting on it would then see empty.
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    let mut wanted = None;
    loop {
        if let Some(wanted) = wanted
            && request.len() >= wanted
        {
            break;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if wanted.is_none()
            && let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n")
        {
            let head = String::from_utf8_lossy(&request[..end]);
            wanted = Some(end + 4 + content_length(&head));
        }
        if request.len() > 1024 * 1024 {
            break;
        }
    }

    let split = request
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(request.len(), |end| end + 4);
    let head = String::from_utf8_lossy(&request[..split]).into_owned();
    let body = String::from_utf8_lossy(&request[split..]).into_owned();

    let mut words = head.split_whitespace();
    let method = words.next().unwrap_or_default().to_owned();
    let target = words.next().unwrap_or_default().to_owned();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let request = Request {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        body,
    };

    let (route, context, delay) = {
        let mut state = state.lock().unwrap();
        state.seen.push(target.clone());
        state.heads.push(head);
        state.bodies.push(request.body.clone());
        let route = match state.routes.get(&request.path) {
            Some(Route::Fixed(body)) => Route::Fixed(body.clone()),
            Some(Route::Answer(handler)) => Route::Answer(handler.clone()),
            None => Route::Answer(Arc::new(|_| None)),
        };
        let delay = state.delays.get(&request.path).copied();
        (route, state.context.clone(), delay)
    };
    // Outside the lock, so a handler that keeps state of its own cannot
    // deadlock against a test reading `asked()` at the same moment.
    let body = match route {
        Route::Fixed(body) => Some(body),
        Route::Answer(handler) => handler(&request),
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

/// How many bytes of body the head says follow it; none if it does not say.
fn content_length(head: &str) -> usize {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A body that arrives after its head, in a separate write, is still the
    /// request's body. A client is free to send them that way, and `reqwest`
    /// sometimes does.
    #[tokio::test]
    async fn a_body_sent_after_its_head_is_waited_for() {
        let player = Player::with_routes(vec![("/Presets/edit", "<presets/>".to_owned())]).await;
        let mut stream = TcpStream::connect(player.address()).await.unwrap();

        let body = r#"{"prid":4,"ordering":[]}"#;
        let head = format!(
            "POST /Presets/edit?prid=4 HTTP/1.1\r\nHost: x\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        stream.write_all(body.as_bytes()).await.unwrap();

        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert_eq!(player.bodies(), vec![body.to_owned()]);
        assert!(
            !player.heads()[0].contains("prid\":"),
            "the head stops at the blank line: {:?}",
            player.heads()
        );
    }

    #[test]
    fn a_query_is_read_by_name() {
        let request = Request {
            method: "GET".to_owned(),
            path: "/Alarms".to_owned(),
            query: "id=4&delete=1".to_owned(),
            body: String::new(),
        };
        assert_eq!(request.param("delete"), Some("1"));
        assert_eq!(request.param("id"), Some("4"));
        assert_eq!(request.param("enable"), None);
    }
}
