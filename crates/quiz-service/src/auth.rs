//! Tickets: the only identity the service knows.
//!
//! The Go server signs a short-lived HS256 JWT per person; everything the
//! service needs to know about them is in its claims. There are no accounts,
//! passwords or logins here.
//!
//! The ticket travels in `Authorization: Bearer` on HTTP and in the payload
//! of the websocket's `connection_init` message, never in a URL.

use crate::config::Secret;
use crate::error::{AppError, AppResult};
use axum::http::HeaderMap;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

/// `iss` claim: the Go server.
pub const ISSUER: &str = "lms";
/// `aud` claim: this service.
pub const AUDIENCE: &str = "quiz";
/// Clock skew accepted on `exp`, in seconds.
const LEEWAY_SECS: u64 = 5;
/// Longest accepted `sub`.
const MAX_SUB_LEN: usize = 64;
/// Longest display name kept, in characters...
pub const MAX_NAME_CHARS: usize = 60;
/// ...and in bytes (60 emoji would be 240): 500 names must fit one frame.
pub const MAX_NAME_BYTES: usize = 160;

/// A display name that is safe to show and to put in a frame: no control
/// characters, at most [`MAX_NAME_CHARS`] characters and [`MAX_NAME_BYTES`]
/// bytes (cut at a character boundary).
pub fn clean_name(raw: &str) -> String {
    let mut name = String::new();
    for c in raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
    {
        if name.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        name.push(c);
    }
    name.trim().to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A teacher outside any session: the quiz bank only.
    Teacher,
    /// The teacher of this session: the bank plus running its game.
    Host,
    /// A student of this session: answering and watching its game.
    Player,
}

/// What the claims of a verified ticket say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub sub: String,
    pub name: String,
    pub role: Role,
    /// Session id (`host` and `player` only).
    pub sid: Option<String>,
    /// Class id (`host` and `player` only).
    pub cid: Option<String>,
    /// When the room closes, unix ms (`host` and `player` only).
    pub rc: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    iss: String,
    aud: String,
    sub: String,
    name: String,
    role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rc: Option<i64>,
    #[serde(default)]
    iat: i64,
    exp: i64,
}

impl Ticket {
    /// Verify `token`: signature, `iss`, `aud`, `exp` and the shape of the
    /// claims for the role.
    pub fn verify(key: &DecodingKey, token: &str) -> AppResult<Ticket> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[AUDIENCE]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = LEEWAY_SECS;
        let claims = decode::<Claims>(token, key, &validation)
            .map_err(|_| AppError::Unauthorized("missing or invalid ticket".into()))?
            .claims;
        Self::from_claims(claims)
    }

    fn from_claims(c: Claims) -> AppResult<Ticket> {
        let bad = |why: &str| AppError::Unauthorized(format!("invalid ticket: {why}"));
        if c.sub.is_empty() || c.sub.len() > MAX_SUB_LEN {
            return Err(bad("sub"));
        }
        let (sid, cid, rc) = match c.role {
            Role::Teacher => (None, None, None),
            Role::Host | Role::Player => {
                let sid = c.sid.filter(|s| !s.is_empty()).ok_or_else(|| bad("sid"))?;
                let cid = c.cid.filter(|s| !s.is_empty()).ok_or_else(|| bad("cid"))?;
                let rc = c.rc.filter(|rc| *rc > 0).ok_or_else(|| bad("rc"))?;
                (Some(sid), Some(cid), Some(rc))
            }
        };
        Ok(Ticket {
            sub: c.sub,
            name: clean_name(&c.name),
            role: c.role,
            sid,
            cid,
            rc,
        })
    }

    /// Sign this ticket, valid until `exp` (unix seconds). The Go server
    /// does this in production; the dev tool and the tests use it too.
    pub fn sign(&self, key: &Secret, now_secs: i64, exp: i64) -> AppResult<String> {
        let claims = Claims {
            iss: ISSUER.into(),
            aud: AUDIENCE.into(),
            sub: self.sub.clone(),
            name: self.name.clone(),
            role: self.role,
            sid: self.sid.clone(),
            cid: self.cid.clone(),
            rc: self.rc,
            iat: now_secs,
            exp,
        };
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(key.expose().as_bytes()),
        )
        .map_err(|e| AppError::Internal(format!("ticket signing failed: {e}")))
    }

    /// The bank (quizzes of one's own): teachers and hosts.
    pub fn require_bank(&self) -> AppResult<()> {
        match self.role {
            Role::Teacher | Role::Host => Ok(()),
            Role::Player => Err(AppError::Forbidden),
        }
    }

    /// Running a game: the host of the session in the ticket.
    pub fn require_host(&self) -> AppResult<&str> {
        match (self.role, self.sid.as_deref()) {
            (Role::Host, Some(sid)) => Ok(sid),
            _ => Err(AppError::Forbidden),
        }
    }

    /// Answering: a player of the session in the ticket.
    pub fn require_player(&self) -> AppResult<&str> {
        match (self.role, self.sid.as_deref()) {
            (Role::Player, Some(sid)) => Ok(sid),
            _ => Err(AppError::Forbidden),
        }
    }

    /// Reaching the game of `session_id`: its host or one of its players, and
    /// nobody else.
    pub fn require_session(&self, session_id: &str) -> AppResult<()> {
        match (self.role, self.sid.as_deref()) {
            (Role::Host | Role::Player, Some(sid)) if sid == session_id => Ok(()),
            _ => Err(AppError::Forbidden),
        }
    }
}

/// The ticket in `Authorization: Bearer <ticket>`, if present and valid.
pub fn ticket_from_headers(key: &DecodingKey, headers: &HeaderMap) -> Option<Ticket> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    Ticket::verify(key, token).ok()
}

/// The ticket in a `connection_init` payload: `{"ticket": "..."}`.
pub fn ticket_from_payload(key: &DecodingKey, payload: &serde_json::Value) -> AppResult<Ticket> {
    let token = payload
        .get("ticket")
        .and_then(|t| t.as_str())
        .ok_or_else(|| AppError::Unauthorized("connection_init needs a ticket".into()))?;
    Ticket::verify(key, token)
}

/// Compare two keys without leaking where they differ.
pub fn keys_match(expected: &Secret, given: &str) -> bool {
    expected.expose().as_bytes().ct_eq(given.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn secret() -> Secret {
        crate::config::Config::from_lookup(|n| match n {
            "QUIZ_TICKET_KEY" | "QUIZ_INTERNAL_KEY" => Some(KEY.into()),
            "QUIZ_ALLOWED_ORIGINS" => Some("https://site.example".into()),
            _ => None,
        })
        .unwrap()
        .ticket_key
    }

    fn dkey() -> DecodingKey {
        DecodingKey::from_secret(KEY.as_bytes())
    }

    fn host() -> Ticket {
        Ticket {
            sub: "u1".into(),
            name: "Ms Lan".into(),
            role: Role::Host,
            sid: Some("s1".into()),
            cid: Some("c1".into()),
            rc: Some(4_000_000_000_000),
        }
    }

    fn now() -> i64 {
        jsonwebtoken::get_current_timestamp() as i64
    }

    #[test]
    fn roundtrip() {
        let token = host().sign(&secret(), now(), now() + 600).unwrap();
        assert_eq!(Ticket::verify(&dkey(), &token).unwrap(), host());
    }

    #[test]
    fn rejects_expired_wrong_key_wrong_audience_and_unknown_role() {
        let expired = host().sign(&secret(), now() - 700, now() - 100).unwrap();
        assert!(Ticket::verify(&dkey(), &expired).is_err());

        let token = host().sign(&secret(), now(), now() + 600).unwrap();
        let other = DecodingKey::from_secret(b"another-key-another-key-another-key");
        assert!(Ticket::verify(&other, &token).is_err());

        // hand-made tokens that break one rule each
        let enc = EncodingKey::from_secret(KEY.as_bytes());
        let base = serde_json::json!({
            "iss": "lms", "aud": "quiz", "sub": "u1", "name": "n", "role": "host",
            "sid": "s", "cid": "c", "rc": 4_000_000_000_000_i64, "exp": now() + 600
        });
        let sign = |v: serde_json::Value| encode(&Header::default(), &v, &enc).unwrap();
        assert!(Ticket::verify(&dkey(), &sign(base.clone())).is_ok());
        for (field, value) in [("aud", "other"), ("iss", "someone"), ("role", "admin")] {
            let mut v = base.clone();
            v[field] = value.into();
            assert!(Ticket::verify(&dkey(), &sign(v)).is_err(), "{field}");
        }
        for missing in ["sid", "cid", "rc", "exp", "sub"] {
            let mut v = base.clone();
            v.as_object_mut().unwrap().remove(missing);
            assert!(Ticket::verify(&dkey(), &sign(v)).is_err(), "no {missing}");
        }
    }

    #[test]
    fn names_are_cut_by_characters_and_bytes_and_lose_control_characters() {
        assert_eq!(clean_name("  Lan\u{0}\n\t Nguyen \u{7f}"), "Lan Nguyen");
        assert_eq!(clean_name(&"a".repeat(100)).len(), MAX_NAME_CHARS);
        let emoji = clean_name(&"\u{1F600}".repeat(60));
        assert!(emoji.len() <= MAX_NAME_BYTES, "{} bytes", emoji.len());
        assert_eq!(emoji.chars().count(), MAX_NAME_BYTES / 4);
        // a cut never splits a character
        assert!(String::from_utf8(emoji.clone().into_bytes()).is_ok());
        assert_eq!(clean_name("\u{200B}\u{0}"), "\u{200B}", "only controls go");
        let mut t = host();
        t.name = "x".into();
        let token = Ticket {
            name: format!("\u{1F600}\u{0}{}", "é".repeat(200)),
            ..t
        }
        .sign(&secret(), now(), now() + 600)
        .unwrap();
        let name = Ticket::verify(&dkey(), &token).unwrap().name;
        assert!(name.len() <= MAX_NAME_BYTES && !name.contains('\u{0}'));
    }

    #[test]
    fn teacher_tickets_have_no_session() {
        let t = Ticket {
            role: Role::Teacher,
            sid: Some("s1".into()),
            ..host()
        };
        let token = t.sign(&secret(), now(), now() + 600).unwrap();
        let back = Ticket::verify(&dkey(), &token).unwrap();
        assert_eq!(back.sid, None);
        assert!(back.require_session("s1").is_err());
        assert!(back.require_bank().is_ok());
    }

    #[test]
    fn roles_reach_only_what_they_may() {
        let h = host();
        let p = Ticket {
            role: Role::Player,
            ..host()
        };
        assert!(h.require_bank().is_ok() && p.require_bank().is_err());
        assert!(h.require_host().is_ok() && p.require_host().is_err());
        assert!(p.require_player().is_ok() && h.require_player().is_err());
        assert!(h.require_session("s1").is_ok() && p.require_session("s1").is_ok());
        assert!(h.require_session("other").is_err());
    }

    #[test]
    fn internal_key_comparison() {
        assert!(keys_match(&secret(), KEY));
        assert!(!keys_match(&secret(), "nope"));
        assert!(!keys_match(&secret(), ""));
    }
}
