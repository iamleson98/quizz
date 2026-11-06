//! Frames of quest mode (`PROTOCOL.md` is the reference). Types 20 and up.

use super::chest::Outcome;
use crate::game::frames::server;
use axum::extract::ws::Utf8Bytes;
use std::fmt::Write as _;

fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// `[20,now,"title",questions,players,seconds,flags]`
pub fn lobby(
    now: i64,
    title: &str,
    questions: usize,
    players: usize,
    seconds: u32,
    flags: u32,
) -> Utf8Bytes {
    Utf8Bytes::from(format!(
        "[{},{now},{},{questions},{players},{seconds},{flags}]",
        server::QUEST_LOBBY,
        js(title)
    ))
}

/// `[21,[["name",character],...]]` (host only)
pub fn roster(players: &[(&str, u8)]) -> Utf8Bytes {
    let mut s = format!("[{},[", server::QUEST_ROSTER);
    for (i, (name, character)) in players.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "[{},{character}]", js(name));
    }
    s.push_str("]]");
    Utf8Bytes::from(s)
}

/// `[22,now,endsAt]`: the game is running until `endsAt`.
pub fn running(now: i64, ends_at: i64) -> Utf8Bytes {
    Utf8Bytes::from(format!("[{},{now},{ends_at}]", server::QUEST_RUNNING))
}

/// Where a player is in their own loop, for `me`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    /// Nothing to do yet (between a chest and the next question).
    Nothing = 0,
    /// A question is open.
    Question = 1,
    /// A chest is waiting to be picked.
    Chest = 2,
    /// Thinking about the last answer: the pause is running.
    Pause = 3,
}

/// A player's whole state, for after a hello:
/// `[23,coins,rank,streak,booster,shield,character,serial,pending,pauseMs]`
#[allow(clippy::too_many_arguments)]
pub fn me(
    coins: i64,
    rank: u32,
    streak: u32,
    booster: u8,
    shield: bool,
    character: u8,
    serial: u32,
    pending: Pending,
    pause_ms: i64,
) -> Utf8Bytes {
    Utf8Bytes::from(format!(
        "[{},{coins},{rank},{streak},{booster},{},{character},{serial},{},{pause_ms}]",
        server::QUEST_ME,
        shield as u8,
        pending as u8
    ))
}

/// The open prefix of a question, shared by everyone who gets it:
/// `[24,kind,"text",["opt",...],limitSec` (the tail adds `serial,remainingMs`).
pub fn question_head(kind: u8, text: &str, options: &[&str], limit_sec: i64) -> String {
    let mut s = format!("[{},{kind},{},[", server::QUEST_QUESTION, js(text));
    for (i, o) in options.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&js(o));
    }
    let _ = write!(s, "],{limit_sec}");
    s
}

/// A right answer: `[25,serial,1,earned,streak,bonus,coins,boosterLeft]`.
pub fn verdict_right(
    serial: u32,
    earned: i64,
    streak: u32,
    bonus: i64,
    coins: i64,
    booster: u8,
) -> Utf8Bytes {
    Utf8Bytes::from(format!(
        "[{},{serial},1,{earned},{streak},{bonus},{coins},{booster}]",
        server::QUEST_VERDICT
    ))
}

/// A wrong (`result` 0) or missed (`result` 2) answer:
/// `[25,serial,result,0,0,0,coins,correctMask,pauseMs,explanation|null]`.
pub fn verdict_wrong(
    serial: u32,
    result: u8,
    coins: i64,
    correct_mask: u32,
    pause_ms: i64,
    explanation: Option<&str>,
) -> Utf8Bytes {
    let expl = explanation.map_or_else(|| "null".to_string(), js);
    Utf8Bytes::from(format!(
        "[{},{serial},{result},0,0,0,{coins},{correct_mask},{pause_ms},{expl}]",
        server::QUEST_VERDICT
    ))
}

/// What a chest pick did:
/// `[26,serial,picked,kind,amount,coins,[k0,a0,k1,a1,k2,a2],"victim"|null,blocked]`
#[allow(clippy::too_many_arguments)]
pub fn chest(
    serial: u32,
    picked: usize,
    effect: (u8, i64),
    coins: i64,
    shown: &[Outcome; 3],
    victim: Option<&str>,
    blocked: bool,
) -> Utf8Bytes {
    let mut s = format!(
        "[{},{serial},{picked},{},{},{coins},[",
        server::QUEST_CHEST,
        effect.0,
        effect.1
    );
    for (i, o) in shown.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let (k, a) = o.wire();
        let _ = write!(s, "{k},{a}");
    }
    s.push_str("],");
    match victim {
        Some(v) => s.push_str(&js(v)),
        None => s.push_str("null"),
    }
    let _ = write!(s, ",{}]", blocked as u8);
    Utf8Bytes::from(s)
}

/// Something that happened to one player.
pub enum Event<'a> {
    /// `[27,0,"thief",amount,coins]`: coins were stolen from you.
    Stolen {
        thief: &'a str,
        amount: i64,
        coins: i64,
    },
    /// `[27,1,"thief"]`: your shield blocked a steal.
    Blocked { thief: &'a str },
    /// `[27,2]`: your booster ran out.
    BoosterEnded,
}

pub fn event(e: &Event<'_>) -> Utf8Bytes {
    Utf8Bytes::from(match e {
        Event::Stolen {
            thief,
            amount,
            coins,
        } => {
            format!("[{},0,{},{amount},{coins}]", server::QUEST_EVENT, js(thief))
        }
        Event::Blocked { thief } => format!("[{},1,{}]", server::QUEST_EVENT, js(thief)),
        Event::BoosterEnded => format!("[{},2]", server::QUEST_EVENT),
    })
}

/// The open prefix of the standings strip, shared by every player:
/// `[28,[["name",character,coins]...]` (the tail adds `rank,coins`).
pub fn standings_head(top: &[(&str, u8, i64)]) -> String {
    let mut s = format!("[{},[", server::QUEST_STANDINGS);
    push_rows(&mut s, top);
    s.push(']');
    s
}

/// The host's board: `[29,[["name",character,coins]...(10)],players,answers,correct,msLeft]`
pub fn board(
    top: &[(&str, u8, i64)],
    players: usize,
    answers: u64,
    correct: u64,
    ms_left: i64,
) -> Utf8Bytes {
    let mut s = format!("[{},[", server::QUEST_BOARD);
    push_rows(&mut s, top);
    let _ = write!(s, "],{players},{answers},{correct},{ms_left}]");
    Utf8Bytes::from(s)
}

fn push_rows(s: &mut String, rows: &[(&str, u8, i64)]) {
    for (i, (name, character, coins)) in rows.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "[{},{character},{coins}]", js(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn v(t: &Utf8Bytes) -> Value {
        serde_json::from_str(t).unwrap()
    }

    #[test]
    fn every_frame_is_valid_json_with_its_type_first() {
        assert_eq!(
            v(&lobby(10, "Q", 5, 3, 600, 1)),
            json!([20, 10, "Q", 5, 3, 600, 1])
        );
        assert_eq!(
            v(&roster(&[("An", 3), ("Bình", 11)])),
            json!([21, [["An", 3], ["Bình", 11]]])
        );
        assert_eq!(v(&running(5, 99)), json!([22, 5, 99]));
        assert_eq!(
            v(&me(120, 2, 3, 2, true, 7, 9, Pending::Chest, 0)),
            json!([23, 120, 2, 3, 2, 1, 7, 9, 2, 0])
        );
        assert_eq!(
            v(&verdict_right(4, 24, 2, 5, 90, 1)),
            json!([25, 4, 1, 24, 2, 5, 90, 1])
        );
        assert_eq!(
            v(&verdict_wrong(4, 0, 90, 5, 2500, Some("because"))),
            json!([25, 4, 0, 0, 0, 0, 90, 5, 2500, "because"])
        );
        assert_eq!(v(&verdict_wrong(4, 2, 0, 1, 2500, None))[9], Value::Null);
        assert_eq!(
            v(&chest(
                4,
                1,
                (0, 40),
                130,
                &[Outcome::Slip, Outcome::Coins(40), Outcome::Steal],
                None,
                false
            )),
            json!([26, 4, 1, 0, 40, 130, [5, 20, 0, 40, 4, 0], null, 0])
        );
        assert_eq!(
            v(&chest(
                4,
                2,
                (4, 30),
                130,
                &[Outcome::Slip, Outcome::Slip, Outcome::Steal],
                Some("Binh"),
                true
            )),
            json!([26, 4, 2, 4, 30, 130, [5, 20, 5, 20, 4, 0], "Binh", 1])
        );
        assert_eq!(
            v(&event(&Event::Stolen {
                thief: "An",
                amount: 12,
                coins: 80
            })),
            json!([27, 0, "An", 12, 80])
        );
        assert_eq!(
            v(&event(&Event::Blocked { thief: "An" })),
            json!([27, 1, "An"])
        );
        assert_eq!(v(&event(&Event::BoosterEnded)), json!([27, 2]));
        assert_eq!(
            v(&board(&[("An", 1, 50)], 12, 40, 30, 61_000)),
            json!([29, [["An", 1, 50]], 12, 40, 30, 61000])
        );
    }

    #[test]
    fn a_question_is_a_shared_prefix_plus_a_personal_tail() {
        let head = question_head(1, "Pick \"two\"", &["a", "b"], 20);
        let frame = crate::game::frames::Out::Open(
            Utf8Bytes::from(head),
            crate::game::frames::Tail::Serial {
                serial: 7,
                remaining_ms: 19_500,
            },
        )
        .into_text()
        .unwrap();
        assert_eq!(
            v(&frame),
            json!([24, 1, "Pick \"two\"", ["a", "b"], 20, 7, 19500])
        );
    }

    #[test]
    fn standings_are_a_shared_prefix_plus_the_players_own_place() {
        let head = standings_head(&[("An", 1, 90), ("Binh", 2, 80)]);
        let frame = crate::game::frames::Out::Open(
            Utf8Bytes::from(head),
            crate::game::frames::Tail::Standing { rank: 5, coins: 40 },
        )
        .into_text()
        .unwrap();
        assert_eq!(
            v(&frame),
            json!([28, [["An", 1, 90], ["Binh", 2, 80]], 5, 40])
        );
    }
}
