//! The wire format of the game socket (`PROTOCOL.md` is the reference).
//!
//! Every frame is a text frame holding a JSON array whose first element is
//! the frame type. Numbers stand in for names and ids: a question is its
//! number in the game, an option is its position, a set of chosen options is
//! a bit mask (bit 0 = the first option). The service's row ids never go on
//! the wire here.

use axum::extract::ws::Utf8Bytes;
use serde_json::Value;
use std::fmt::Write as _;

/// Client to server frame types.
pub mod client {
    pub const HELLO: u64 = 0;
    pub const START: u64 = 1;
    pub const NEXT: u64 = 2;
    pub const END: u64 = 3;
    pub const ANSWER: u64 = 4;
}

/// Server to client frame types.
pub mod server {
    pub const NONE: u8 = 0;
    pub const LOBBY: u8 = 1;
    pub const QUESTION: u8 = 2;
    pub const COUNT: u8 = 3;
    pub const PROGRESS: u8 = 4;
    pub const REVEAL: u8 = 5;
    pub const PODIUM: u8 = 6;
    pub const ENDED: u8 = 7;
    pub const ERROR: u8 = 8;
    pub const ACK: u8 = 9;
    pub const NAMES: u8 = 10;
    pub const CLOSED: u8 = 11;
}

/// Why a game ended (`ended` frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    Host = 0,
    RoomClosed = 1,
    Idle = 2,
    Restart = 3,
}

/// A frame from a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientFrame {
    Hello(String),
    /// Host: open a lobby for the quiz with this (decimal) id.
    Start(i64),
    /// Host: close the question, move on, or leave the podium.
    Next,
    /// Host: end the game now.
    End,
    /// Player: answer question number `q` with the options in `mask`.
    Answer {
        q: u32,
        mask: u32,
    },
}

/// Parse a client text frame; `None` when it is not a well-formed frame.
pub fn parse_client(text: &str) -> Option<ClientFrame> {
    let Value::Array(items) = serde_json::from_str::<Value>(text).ok()? else {
        return None;
    };
    let kind = items.first()?.as_u64()?;
    match (kind, items.len()) {
        (client::HELLO, 2) => Some(ClientFrame::Hello(items[1].as_str()?.to_string())),
        (client::START, 2) => Some(ClientFrame::Start(items[1].as_str()?.parse().ok()?)),
        (client::NEXT, 1) => Some(ClientFrame::Next),
        (client::END, 1) => Some(ClientFrame::End),
        (client::ANSWER, 3) => Some(ClientFrame::Answer {
            q: u32::try_from(items[1].as_u64()?).ok()?,
            mask: u32::try_from(items[2].as_u64()?).ok()?,
        }),
        _ => None,
    }
}

/// The bytes a socket is told to send.
#[derive(Clone, Debug)]
pub enum Out {
    /// A complete frame, shared by every socket that gets it.
    Plain(Utf8Bytes),
    /// A shared frame prefix (no closing bracket) plus a few personal values.
    Open(Utf8Bytes, Tail),
    /// Close the socket.
    Close(u16, &'static str),
}

/// The personal values appended to a shared prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tail {
    /// A question seen again after reconnecting: the options already chosen.
    Mask(u32),
    /// The reveal: score, rank (0 = unranked), points earned (the streak
    /// bonus included), right (0/1), the options chosen (0 = no answer),
    /// the streak of fully right answers after this question, and the part
    /// of `points` that is the streak bonus.
    Reveal {
        score: i64,
        rank: u32,
        points: i64,
        correct: bool,
        mask: u32,
        streak: u32,
        bonus: i64,
    },
    /// The podium: score and rank.
    Podium { score: i64, rank: u32 },
}

impl Out {
    /// The final text of the frame this item stands for.
    pub fn into_text(self) -> Option<Utf8Bytes> {
        match self {
            Out::Plain(t) => Some(t),
            Out::Open(head, tail) => {
                let mut s = String::with_capacity(head.len() + 48);
                s.push_str(&head);
                match tail {
                    Tail::Mask(m) => write!(s, ",{m}"),
                    Tail::Reveal {
                        score,
                        rank,
                        points,
                        correct,
                        mask,
                        streak,
                        bonus,
                    } => write!(
                        s,
                        ",{score},{rank},{points},{},{mask},{streak},{bonus}",
                        correct as u8
                    ),
                    Tail::Podium { score, rank } => write!(s, ",{score},{rank}"),
                }
                .ok()?;
                s.push(']');
                Some(Utf8Bytes::from(s))
            }
            Out::Close(..) => None,
        }
    }
}

fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// `[0,now]`: no game in this session.
pub fn none(now: i64) -> Utf8Bytes {
    Utf8Bytes::from(format!("[{},{now}]", server::NONE))
}

/// `[1,now,"title",questions,players]`
pub fn lobby(now: i64, title: &str, questions: usize, players: usize) -> Utf8Bytes {
    Utf8Bytes::from(format!(
        "[{},{now},{},{questions},{players}]",
        server::LOBBY,
        js(title)
    ))
}

/// `[3,players]`
pub fn count(players: usize) -> Utf8Bytes {
    Utf8Bytes::from(format!("[{},{players}]", server::COUNT))
}

/// `[4,answered,players,[c0,c1,...],[["name",total],...]]` (host only):
/// `counts[i]` is how many players have chosen option `i` so far, and the
/// last element is the provisional top ten by total score.
pub fn progress(answered: usize, players: usize, counts: &[u32], top: &[(&str, i64)]) -> Utf8Bytes {
    let mut s = format!("[{},{answered},{players},[", server::PROGRESS);
    for (i, c) in counts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{c}");
    }
    s.push_str("],");
    push_rows(&mut s, top);
    s.push(']');
    Utf8Bytes::from(s)
}

/// `[10,["name",...]]` (host only): display names of players who joined.
pub fn names(names: &[&str]) -> Utf8Bytes {
    let mut s = format!("[{},[", server::NAMES);
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&js(n));
    }
    s.push_str("]]");
    Utf8Bytes::from(s)
}

/// The open prefix of the question frame:
/// `[2,now,number,count,endsAt,kind,"text",["opt",...]`
pub fn question_head(
    now: i64,
    number: usize,
    count: usize,
    ends_at: i64,
    kind: u8,
    text: &str,
    options: &[&str],
) -> String {
    let mut s = format!(
        "[{},{now},{number},{count},{ends_at},{kind},{},[",
        server::QUESTION,
        js(text)
    );
    for (i, o) in options.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&js(o));
    }
    s.push(']');
    s
}

/// The open prefix of the reveal frame:
/// `[5,number,correctMask,[counts...],explanation|null,[["name",score]...]`
pub fn reveal_head(
    number: usize,
    correct_mask: u32,
    counts: &[u32],
    explanation: Option<&str>,
    top: &[(&str, i64)],
) -> String {
    let mut s = format!("[{},{number},{correct_mask},[", server::REVEAL);
    for (i, c) in counts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{c}");
    }
    s.push_str("],");
    match explanation {
        Some(e) => s.push_str(&js(e)),
        None => s.push_str("null"),
    }
    s.push(',');
    push_rows(&mut s, top);
    s
}

/// The open prefix of the podium frame: `[6,[["name",score]...]` and, for
/// the host, `,[["name",score]...]` with the full order.
pub fn podium_head(top: &[(&str, i64)], all: Option<&[(&str, i64)]>) -> String {
    let mut s = format!("[{},", server::PODIUM);
    push_rows(&mut s, top);
    if let Some(all) = all {
        s.push(',');
        push_rows(&mut s, all);
    }
    s
}

fn push_rows(s: &mut String, rows: &[(&str, i64)]) {
    s.push('[');
    for (i, (name, score)) in rows.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "[{},{score}]", js(name));
    }
    s.push(']');
}

/// Close a prefix into a complete frame.
pub fn close(mut head: String) -> Utf8Bytes {
    head.push(']');
    Utf8Bytes::from(head)
}

/// `[7,reason]`
pub fn ended(reason: EndReason) -> Utf8Bytes {
    Utf8Bytes::from(format!("[{},{}]", server::ENDED, reason as u8))
}

/// `[8,code,request]`: `request` is the type of the frame that failed.
pub fn error(code: u8, request: u64) -> Utf8Bytes {
    Utf8Bytes::from(format!("[{},{code},{request}]", server::ERROR))
}

/// `[11,q]`: question `q` is closed; the reveal follows after a short beat.
pub fn closed(q: usize) -> Utf8Bytes {
    Utf8Bytes::from(format!("[{},{q}]", server::CLOSED))
}

/// `[9]`: the answer is locked in.
pub fn ack() -> Utf8Bytes {
    Utf8Bytes::from_static("[9]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_client_frame() {
        assert_eq!(
            parse_client(r#"[0,"tok"]"#),
            Some(ClientFrame::Hello("tok".into()))
        );
        assert_eq!(parse_client(r#"[1,"123"]"#), Some(ClientFrame::Start(123)));
        assert_eq!(parse_client("[2]"), Some(ClientFrame::Next));
        assert_eq!(parse_client("[3]"), Some(ClientFrame::End));
        assert_eq!(
            parse_client("[4,2,5]"),
            Some(ClientFrame::Answer { q: 2, mask: 5 })
        );
    }

    #[test]
    fn rejects_malformed_frames() {
        for bad in [
            "",
            "{}",
            "[]",
            "[9]",
            "[0]",
            "[0,1]",
            "[1,5]",
            "[1,\"x\"]",
            "[2,1]",
            "[4,1]",
            "[4,-1,1]",
            "[4,1,\"a\"]",
            "[4,1,2,3]",
            "not json",
        ] {
            assert_eq!(parse_client(bad), None, "{bad}");
        }
    }

    #[test]
    fn question_frame_is_compact_json() {
        let head = question_head(1000, 2, 5, 21000, 1, "Pick \"two\"", &["a", "b"]);
        let text = close(head);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v,
            serde_json::json!([2, 1000, 2, 5, 21000, 1, "Pick \"two\"", ["a", "b"]])
        );
    }

    #[test]
    fn tails_complete_a_prefix() {
        let head = Utf8Bytes::from(reveal_head(1, 2, &[3, 4], None, &[("Lan", 900)]));
        let text = Out::Open(
            head,
            Tail::Reveal {
                score: 900,
                rank: 1,
                points: 900,
                correct: true,
                mask: 2,
                streak: 3,
                bonus: 200,
            },
        )
        .into_text()
        .unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v,
            serde_json::json!([
                5,
                1,
                2,
                [3, 4],
                null,
                [["Lan", 900]],
                900,
                1,
                900,
                1,
                2,
                3,
                200
            ])
        );
    }

    #[test]
    fn the_closed_frame_is_tiny() {
        assert_eq!(&*closed(3), "[11,3]");
    }

    #[test]
    fn host_frames_for_selections_and_names() {
        assert_eq!(
            &*progress(3, 10, &[2, 0, 1, 0], &[("An", 1100), ("Bình", 900)]),
            "[4,3,10,[2,0,1,0],[[\"An\",1100],[\"Bình\",900]]]"
        );
        assert_eq!(
            &*names(&["An", "Bình \"B\""]),
            "[10,[\"An\",\"Bình \\\"B\\\"\"]]"
        );
        assert_eq!(&*names(&[]), "[10,[]]");
    }

    #[test]
    fn podium_with_and_without_the_full_order() {
        let top = [("A", 3), ("B", 2)];
        let all = [("A", 3), ("B", 2), ("C", 1)];
        let player = close(podium_head(&top, None));
        let host = close(podium_head(&top, Some(&all)));
        assert_eq!(
            serde_json::from_str::<Value>(&player)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            serde_json::from_str::<Value>(&host)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
}
