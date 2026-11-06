//! Development tool: print a ticket signed with `QUIZ_TICKET_KEY`, so the
//! bank API and the game socket can be tried by hand. In production the Go
//! server signs tickets; this binary is not part of the image.
//!
//! ```text
//! QUIZ_TICKET_KEY=... mint-ticket --role teacher --sub teacher1 --name "Ms Lan"
//! QUIZ_TICKET_KEY=... mint-ticket --role host --sub teacher1 --name "Ms Lan" \
//!     --sid session1 --cid class1 [--rc <unix ms>] [--ttl <seconds>]
//! ```

use quiz_service::auth::{Role, Ticket};
use quiz_service::config::{MIN_KEY_LEN, Secret};
use std::collections::HashMap;

const USAGE: &str = "usage: QUIZ_TICKET_KEY=<key> mint-ticket --role teacher|host|player --sub <id> \
--name <name> [--sid <session> --cid <class>] [--rc <unix ms, default now + 2 h>] [--ttl <seconds, default 600>]";

fn fail(msg: &str) -> ! {
    eprintln!("{msg}\n{USAGE}");
    std::process::exit(2);
}

fn main() {
    let key =
        std::env::var("QUIZ_TICKET_KEY").unwrap_or_else(|_| fail("QUIZ_TICKET_KEY is not set"));
    if key.len() < MIN_KEY_LEN {
        fail(&format!(
            "QUIZ_TICKET_KEY must be at least {MIN_KEY_LEN} bytes"
        ));
    }
    let mut args: HashMap<String, String> = HashMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let name = flag
            .strip_prefix("--")
            .unwrap_or_else(|| fail(&format!("unexpected argument {flag}")));
        let value = it
            .next()
            .unwrap_or_else(|| fail(&format!("--{name} needs a value")));
        args.insert(name.to_string(), value);
    }
    let get = |name: &str| args.get(name).cloned();
    let need = |name: &str| get(name).unwrap_or_else(|| fail(&format!("--{name} is required")));

    let role = match need("role").as_str() {
        "teacher" => Role::Teacher,
        "host" => Role::Host,
        "player" => Role::Player,
        other => fail(&format!("unknown role {other}")),
    };
    let num = |name: &str| -> Option<i64> {
        get(name).map(|v| {
            v.parse()
                .unwrap_or_else(|_| fail(&format!("--{name} must be a number")))
        })
    };
    let now_secs = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()) as i64;
    let in_room = role != Role::Teacher;
    let ticket = Ticket {
        sub: need("sub"),
        name: need("name"),
        role,
        sid: in_room.then(|| need("sid")),
        cid: in_room.then(|| need("cid")),
        rc: in_room.then(|| num("rc").unwrap_or(now_secs * 1000 + 2 * 60 * 60 * 1000)),
    };
    let ttl = num("ttl").unwrap_or(600);
    let token = ticket
        .sign(&Secret::new(key), now_secs, now_secs + ttl)
        .unwrap_or_else(|e| fail(&e.to_string()));
    println!("{token}");
}
