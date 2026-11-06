//! The game socket: `GET /ws`, subprotocol `quiz.v1` (see `PROTOCOL.md`).
//!
//! One task per socket. After the upgrade it waits 2 s for `hello`, checks
//! the ticket once, attaches to the room of the ticket's session and then
//! only relays: frames from the client go to the room, shared bytes from the
//! room go to the client. The only periodic traffic is a websocket ping.

use crate::auth::{Role, Ticket};
use crate::error::AppError;
use crate::game::frames::{self, ClientFrame, Out, parse_client};
use crate::game::registry::{Turned, UnknownGuard, out_channel};
use crate::game::room::Member;
use crate::state::AppState;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::header::{ORIGIN, SEC_WEBSOCKET_PROTOCOL};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::time::Instant;

/// The subprotocol the socket speaks.
pub const SUBPROTOCOL: &str = "quiz.v1";
/// How long a new socket has to say hello.
pub const HELLO_WITHIN: Duration = Duration::from_secs(2);
/// A frame we send must be taken by the peer within this long, or the socket
/// is dropped (a peer that stopped reading must not pin a task and a queue).
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest frame accepted before hello (a ticket is ~600 bytes).
const MAX_HELLO_BYTES: usize = 2048;
/// Largest frame accepted after hello.
const MAX_FRAME_BYTES: usize = 128;
/// Frames a client may send: a burst, then this many per second.
const BURST: f32 = 10.0;
const REFILL_PER_SEC: f32 = 2.0;

/// Per-socket buffer sizes: a few KB, not the library's 128 KiB defaults.
const READ_BUFFER: usize = 1024;
const WRITE_BUFFER: usize = 0;
/// A ceiling, not an allocation (the buffer is flushed after every frame): the
/// biggest frames are the host's, a roster or the full order at the podium of
/// 500 players with names of up to [`crate::auth::MAX_NAME_BYTES`] bytes and
/// their JSON escapes, about 180 KB at the very worst.
const MAX_WRITE_BUFFER: usize = 512 * 1024;

/// The address of the client behind the request.
///
/// The edge (Traefik, behind Cloudflare that only it admits) passes
/// `CF-Connecting-IP`, the address Cloudflare saw, and `X-Real-Ip`; they are
/// read in that order, then the address of the peer. Nothing but the edge can
/// reach the public listener, so the headers are not forgeable by a client; run
/// the service somewhere else and they would be.
pub struct ClientIp(pub IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &AppState) -> Result<Self, Infallible> {
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<IpAddr>().ok())
        };
        let ip = header("cf-connecting-ip")
            .or_else(|| header("x-real-ip"))
            .or_else(|| {
                parts
                    .extensions
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|c| c.0.ip())
            })
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        Ok(ClientIp(ip))
    }
}

/// The buffer sizes and limits every game socket gets.
pub fn tune(ws: WebSocketUpgrade) -> WebSocketUpgrade {
    ws.protocols([SUBPROTOCOL])
        .read_buffer_size(READ_BUFFER)
        .write_buffer_size(WRITE_BUFFER)
        .max_write_buffer_size(MAX_WRITE_BUFFER)
        .max_message_size(MAX_HELLO_BYTES)
        .max_frame_size(MAX_HELLO_BYTES)
}

pub async fn handler(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let origin_ok = headers
        .get(ORIGIN)
        .and_then(|o| o.to_str().ok())
        .is_some_and(|o| state.cfg.allowed_origins.iter().any(|a| a == o));
    if !origin_ok {
        return AppError::Forbidden.into_response();
    }
    let offers_protocol = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim() == SUBPROTOCOL);
    if !offers_protocol {
        return AppError::BadRequest(format!("the {SUBPROTOCOL} subprotocol is required"))
            .into_response();
    }
    let guard = match state.rooms.admit_unknown(ip) {
        Ok(guard) => guard,
        Err(Turned::TooManyFromThisAddress) => {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        Err(Turned::TooManyInAll) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    tune(ws).on_upgrade(move |socket| serve(state, socket, guard))
}

/// Send, giving up when the peer does not take it within [`SEND_TIMEOUT`].
async fn send_message(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn send(socket: &mut WebSocket, text: axum::extract::ws::Utf8Bytes) -> bool {
    send_message(socket, Message::Text(text)).await
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = send_message(
        socket,
        Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })),
    )
    .await;
}

async fn sleep_until_opt(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Wait for the `hello` frame and verify its ticket.
async fn hello(state: &AppState, socket: &mut WebSocket) -> Result<Ticket, (u8, &'static str)> {
    let first = tokio::time::timeout(HELLO_WITHIN, socket.recv()).await;
    let text = match first {
        Ok(Some(Ok(Message::Text(t)))) => t,
        Err(_) => return Err((1, "hello timeout")),
        _ => return Err((4, "expected hello")),
    };
    let Some(ClientFrame::Hello(token)) = parse_client(&text) else {
        return Err((4, "expected hello"));
    };
    let ticket = Ticket::verify(&state.ticket_key, &token).map_err(|_| (1, "bad ticket"))?;
    match ticket.role {
        Role::Host | Role::Player => Ok(ticket),
        Role::Teacher => Err((2, "no game for this ticket")),
    }
}

async fn serve(state: AppState, mut socket: WebSocket, unknown: UnknownGuard) {
    let ticket = match hello(&state, &mut socket).await {
        Ok(t) => t,
        Err((code, reason)) => {
            let _ = send(&mut socket, frames::error(code, frames::client::HELLO)).await;
            close(&mut socket, 1008, reason).await;
            return;
        }
    };
    drop(unknown);
    let sock = state.rooms.next_sock_id();
    // a fifth socket of the same person pushes the oldest out
    let conn = state.rooms.conn(&ticket.sub, sock);

    let (out_tx, mut out_rx) = out_channel();
    let sid = ticket
        .sid
        .clone()
        .expect("host and player tickets carry a session");
    let member = Member {
        sock,
        user: ticket.sub.clone(),
        name: ticket.name.clone(),
        role: ticket.role,
        cid: ticket.cid.clone().unwrap_or_default(),
        rc: ticket.rc.unwrap_or(0),
        out: out_tx,
    };
    let room = match state.rooms.attach(state.deps(), &sid, member).await {
        Ok(room) => room,
        Err(e) => {
            let _ = send(
                &mut socket,
                frames::error(e.frame_code(), frames::client::HELLO),
            )
            .await;
            close(&mut socket, 1008, "refused").await;
            return;
        }
    };

    let mut ping = tokio::time::interval_at(
        Instant::now() + state.cfg.ws_ping_every,
        state.cfg.ws_ping_every,
    );
    let (mut tokens, mut last) = (BURST, Instant::now());
    let mut pong_deadline: Option<Instant> = None;
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    // answers are timed from here, not from when the room
                    // gets to the frame
                    let now = Instant::now();
                    pong_deadline = None;
                    tokens = (tokens + (now - last).as_secs_f32() * REFILL_PER_SEC).min(BURST);
                    last = now;
                    if tokens < 1.0 {
                        let _ = send(&mut socket, frames::error(6, 0)).await;
                        close(&mut socket, 1008, "too many frames").await;
                        break;
                    }
                    tokens -= 1.0;
                    if text.len() > MAX_FRAME_BYTES {
                        close(&mut socket, 1009, "frame too large").await;
                        break;
                    }
                    match parse_client(&text) {
                        Some(ClientFrame::Hello(_)) | None => {
                            let _ = send(&mut socket, frames::error(4, 0)).await;
                        }
                        Some(frame) => room.frame(sock, frame, now).await,
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => pong_deadline = None,
                Some(Ok(_)) | Some(Err(_)) | None => break,
            },
            out = out_rx.recv() => match out {
                Some(Out::Close(code, reason)) => {
                    close(&mut socket, code, reason).await;
                    break;
                }
                Some(out) => {
                    if let Some(text) = out.into_text()
                        && !send(&mut socket, text).await
                    {
                        break;
                    }
                }
                None => {
                    close(&mut socket, 1001, "room closed").await;
                    break;
                }
            },
            _ = ping.tick() => {
                if !send_message(&mut socket, Message::Ping(Default::default())).await {
                    break;
                }
                pong_deadline.get_or_insert(Instant::now() + state.cfg.ws_pong_within);
            }
            _ = sleep_until_opt(pong_deadline) => {
                close(&mut socket, 1008, "no pong").await;
                break;
            }
            _ = conn.evicted() => {
                close(&mut socket, 1008, "replaced by a newer socket").await;
                break;
            }
        }
    }
    room.detach(sock).await;
}
