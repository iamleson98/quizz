//! The game socket: `GET /ws`, subprotocol `quiz.v1` (see `PROTOCOL.md`).
//!
//! One task per socket. After the upgrade it waits 5 s for `hello`, checks
//! the ticket once, attaches to the room of the ticket's session and then
//! only relays: frames from the client go to the room, shared bytes from the
//! room go to the client. The only periodic traffic is a websocket ping.

use crate::auth::{Role, Ticket};
use crate::error::AppError;
use crate::game::frames::{self, ClientFrame, Out, parse_client};
use crate::game::registry::{UnknownGuard, out_channel};
use crate::game::room::Member;
use crate::state::AppState;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::header::{ORIGIN, SEC_WEBSOCKET_PROTOCOL};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::time::Duration;
use tokio::time::Instant;

/// The subprotocol the socket speaks.
pub const SUBPROTOCOL: &str = "quiz.v1";
/// How long a new socket has to say hello.
pub const HELLO_WITHIN: Duration = Duration::from_secs(5);
/// Websocket ping interval (keeps proxies' idle limits away).
pub const PING_EVERY: Duration = Duration::from_secs(25);
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
/// A ceiling, not an allocation: the host's one full list of 500 names can
/// be 30 KB.
const MAX_WRITE_BUFFER: usize = 64 * 1024;

pub async fn handler(
    State(state): State<AppState>,
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
    let Some(guard) = state.rooms.admit_unknown() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.protocols([SUBPROTOCOL])
        .read_buffer_size(READ_BUFFER)
        .write_buffer_size(WRITE_BUFFER)
        .max_write_buffer_size(MAX_WRITE_BUFFER)
        .max_message_size(MAX_HELLO_BYTES)
        .max_frame_size(MAX_HELLO_BYTES)
        .on_upgrade(move |socket| serve(state, socket, guard))
}

async fn send(socket: &mut WebSocket, text: axum::extract::ws::Utf8Bytes) -> bool {
    socket.send(Message::Text(text)).await.is_ok()
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
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
    let Some(_conn) = state.rooms.conn(&ticket.sub) else {
        let _ = send(&mut socket, frames::error(6, frames::client::HELLO)).await;
        close(&mut socket, 1008, "too many sockets").await;
        return;
    };

    let (out_tx, mut out_rx) = out_channel();
    let sock = state.rooms.next_sock_id();
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

    let mut ping = tokio::time::interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    let (mut tokens, mut last) = (BURST, Instant::now());
    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let now = Instant::now();
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
                        Some(frame) => room.frame(sock, frame).await,
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
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
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
        }
    }
    room.detach(sock).await;
}
