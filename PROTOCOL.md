# The game socket: protocol `quiz.v1`

The live game runs on one websocket, `GET /ws` on the public listener. The
bank (quizzes, results) is the REST API in [`openapi.json`](openapi.json); this
document is the other half of the contract, and the web client is written from
it.

The game is deliberately not GraphQL or JSON objects: a class of 500 hears the
same question at the same moment, and every byte and every packet is paid
for on a shared network card. A frame is a short JSON array, its first element
is the frame type, and numbers stand in for names and ids.

## Connecting

```
GET /ws
Origin: https://<one of QUIZ_ALLOWED_ORIGINS>
Sec-WebSocket-Protocol: quiz.v1
```

- The `Origin` must be one of the service's allowed origins (browsers send it
  by themselves; a request without one is refused with 403).
- The subprotocol `quiz.v1` must be offered (400 otherwise) and is the one
  selected.
- The ticket never goes in the URL. The **first client frame must be
  `hello`, within 5 s**; otherwise the socket gets `[8,1,0]` and is closed.
- Tickets are signed by the LMS server: `role` `host` (the session's teacher) or
  `player` (a student of the session), with `sid`, `cid` and `rc`. A `teacher`
  ticket has no game: it is answered `[8,2,0]` and the socket is closed. The
  ticket is checked **once**, when the socket opens.
- The socket belongs to the room of the ticket's `sid`. There is one room per
  live session, created by the first socket and gone with the last; it exists
  before any game does, so students already in the room see the game begin.

All frames are **text** frames. Binary frames close the socket. Numbers are JSON
numbers; times are unix epoch milliseconds.

## Client to server

| Frame | Who | Meaning |
|---|---|---|
| `[0,"<ticket>"]` | both | **hello**: the first frame, and only the first |
| `[1,"<quizId>"]` | host | **start**: open a lobby for this quiz (the id from the bank API, a string) |
| `[2]` | host | **next**: start the first question; close the open question; move on from a reveal; leave the podium (ends the game) |
| `[3]` | host | **end**: end the game now |
| `[4,q,mask]` | player | **answer** question number `q` (1-based, as in the question frame) with the options in `mask` |

`mask` is a bit mask of option positions: bit 0 is the first option, bit 1 the
second, and so on. A single-choice or true/false question needs exactly one
bit; a multiple-choice question one or more. `[4,2,5]` answers question 2 with
options 1 and 3.

The first answer locks: a second one for the same question is refused. The time
taken is the server's receive time minus the question's start; the client never
reports time.

## Server to client

| Type | Frame | When |
|---|---|---|
| 0 | `[0,now]` | **no game** in this session (first frame after hello, or after a game ended and a socket re-syncs) |
| 1 | `[1,now,"title",questions,players]` | **lobby**: a game was opened |
| 2 | `[2,now,q,count,endsAt,kind,"text",["opt",...]]` | **question** `q` of `count` is open |
| 3 | `[3,players]` | **count**: players in the lobby (everyone, in the lobby only) |
| 4 | `[4,answered,players,[c0,c1,...],[["name",total],...]]` | **progress** (host only): answers so far, how many players chose each option so far, and the provisional top ten, during a question |
| 5 | `[5,q,correct,[counts],explanation,[["name",score]...],score,rank,points,right,mine,streak,bonus]` | **reveal** of question `q` |
| 6 | `[6,[["name",score]...(3)],score,rank]` | **podium** for a player |
| 6 | `[6,[["name",score]...(3)],[["name",score]...(all)]]` | **podium** for the host |
| 7 | `[7,reason]` | **ended**: `0` the host ended it, `1` the room closed, `2` idle for an hour, `3` the service is restarting |
| 8 | `[8,code,request]` | **error** for a client frame of type `request` (`0` for hello) |
| 9 | `[9]` | **ack**: your answer is locked in |
| 10 | `[10,["name",...]]` | **names** (host only): display names of players who joined the room |

Details:

- `now` is the server clock at the moment the frame was built (see
  [Clock](#clock)). `kind` is `0` single choice, `1` multiple choice, `2`
  true/false. Options are listed in order; their position is their bit.
- `endsAt` is the question's deadline on the server's clock. The server closes
  the question at `endsAt`, when every connected player eligible to answer has
  answered, or when the host sends `next`, whichever comes first.
- A **reveal** has a personal tail for players (`score, rank, points, right,
  mine, streak, bonus`: total score, rank among the players (0 until ranked),
  points this question **including the streak bonus**, 1 if right else 0, the
  options chosen as a mask (0 for no answer), the number of fully right answers
  in a row after this question, and the part of `points` that is the streak
  bonus) and none for the host. `correct` is the mask of the right options,
  `counts[i]` how many players chose option `i`, `explanation` a string or
  `null`, and the list holds the top five by score, best first.
- **Streak bonus**: a fully right answer after `n-1` fully right answers in a
  row (`n` counts the question just answered) earns `min(100 x (n-1), 500)` on
  top of its speed score: nothing for the first, 100 for the second, up to 500.
  A wrong, partly right (multiple choice) or missing answer ends the streak, and
  a player who joined mid-game starts at 0. The server counts it, per player, for
  the whole game, so it survives a reconnect.
- **Live selections** (host only): `counts[i]` in a progress frame is how many
  players have chosen option `i` so far in the open question; a multiple-choice
  answer counts toward every option it chose. The frame goes out when something
  changed (an answer, or a player joining or leaving), at most once every 500 ms,
  and never to players. A host that says hello while a question is open gets
  one right after the state frame.
- **Live ranking** (host only): the last element of a progress frame is the
  **provisional top ten**: each player's total before this question plus what
  their answer has earned so far, best first. Ties are broken by total answer
  time so far (answers given, a missed question not counted yet), then by who
  joined first. An answer is scored **when it arrives**, by the very function
  and inputs the close uses (the server's receive time for the speed part,
  partial credit, the streak bonus from the streak the player had before this
  question), and the close applies exactly those values: a provisional figure
  never changes at the close, and the reveal's totals equal the last provisional
  ones. A progress frame is sent when anything it shows changed (a choice, the
  ranking, the number of players), so one may carry unchanged counts when only
  the ranking moved. It is for the host: see below why players get nothing of it.
- **Why players never get it.** While a question is open, any figure that moves
  with the answers tells an unanswered student which option is right (the
  ranking shuffles when somebody who knows the topic answers). Players are
  therefore sent nothing of the open question's scores or counts: their frames
  are the question, their own ack, and the reveal, exactly as before.
- **Names** (host only): `[10,["name",...]]` lists the display names of players
  who joined the room since the previous names frame, in the lobby or in the
  middle of a game, at most once every 500 ms. Right after the host's hello,
  when a lobby or a game exists, ONE frame carries every current name (at most
  500). Leavers are not reported: `[3,players]` and `[4,...]` stay the
  authority on how many are there. Players are never sent names (the scoreboard
  rows in a reveal or podium carry only the top ones).
- The **podium** lists the top three; the host's frame lists the full order
  instead of the personal tail. The scoreboard orders by total score, then by
  total answer time (a question not answered counts as its whole time limit),
  then by who joined first.
- `[3,players]` is sent to the lobby only; `[4,answered,players]` goes to the
  host only. Neither is ever sent per answer: they are refreshed **at most once
  every 500 ms**.
- Answer keys are never in a question frame: the right options appear only in
  the reveal.

### Error codes

| Code | Meaning |
|---|---|
| 1 | unauthorized: bad or missing ticket, or no hello in time |
| 2 | forbidden: the role may not do that |
| 3 | not found: no such quiz of the host's, or no game |
| 4 | bad request: malformed frame, or an invalid choice of options |
| 5 | conflict: a game is already running (in the session, or by this teacher), the question is not open, already answered, joined after the question opened, the game is full (500) or the room is closed |
| 6 | too many sockets for this person (4), or too many frames |
| 9 | internal |

After a `hello` error the socket is closed. Errors for later frames leave the
socket open.

## The first frame after hello is the current state (reconnecting)

There is no resume token. After `hello` the server sends one frame describing
where the room is:

| State | First frame |
|---|---|
| no game | `[0,now]` |
| lobby | the lobby frame |
| a question is open | the question frame, with a **tail**: the options you already chose as a mask (`0` for none), as a last element after the options array. The host's has no tail. |
| reveal | the reveal frame with your personal tail |
| podium | the podium frame |

So a client that reconnects (back after a lost network, a reload) simply sends
`hello` again and renders whatever arrives first. A player keeps their score,
rank and answer: they are the game's, keyed by the ticket's `sub`. A player who
opens a second tab is the same player (up to four sockets per person). Players
who join in the middle of a game score from the next question; they see the open
one and an answer to it is refused with error 5.

Reconnect with jittered exponential backoff (for example 0.5 s doubling to 8 s,
plus up to 50% random), and with a **fresh ticket** if the old one has expired
(tickets last 10 minutes and are checked at hello only).

## Clock

The server's clock decides the deadline; the client's clock only draws the bar.
Every frame that carries `now` (types 0, 1, 2) lets the client estimate its
offset: `offset = now - Date.now()` at receive time (take the sample with the
lowest round-trip if you can; the first frame after hello is fine). A
countdown is then `endsAt - (Date.now() + offset)`. Answers are timed on the
server, so a wrong client clock cannot buy points.

## Limits and rate limits

- Up to **4 sockets per person**, 500 players per game, one running game per
  session and per teacher.
- A client frame is at most 128 bytes after hello (2 KiB for hello). A longer
  one closes the socket with code 1009.
- A burst of 10 frames, then 2 per second: faster than that is answered
  `[8,6,0]` and the socket is closed (1008).
- A socket that cannot take the frames sent to it (its small queue stays full)
  is closed with 1013; reconnect and resume.
- The server pings every 25 s (a websocket ping, not a frame); browsers answer by
  themselves. There is no application heartbeat: do not send one.
- A game ends by itself when the ticket's `rc` passes (reason `1`) or after an
  hour without a host command or an answer (reason `2`). A deploy of the
  service ends running games (reason `3`) and closes sockets with 1001.

## What it costs

Per player per question (80 characters, four options of 30): the question frame
(~250 B), the reveal with the player's own result (~160 B) and a 5-byte ack are
received: 3 frames, about 430 B on the wire including websocket headers; one
answer frame of about 7 B (13 B with the websocket header and mask) is sent.
Nothing else is sent per answer to anybody.

The host gets a little more, and only the host: a progress frame at most every
500 ms while answers come in (counts and the provisional top ten, about 280 B
with names like `Student 0001`, about 400 B with 20-character names, 800 B at the
very worst), and the names of those who join in
one frame per 500 ms. The one full-names frame a host gets after its hello is
about 15 B per name: 7.6 KB for 500 players named like `Student 0001`, about
11 KB for typical 20-character names, and 31 KB at the very worst (500 names of
60 characters, the longest kept). The shared part of every frame is
encoded once and the same bytes go to every socket; only a few personal numbers
are appended to a reveal. The bench (`quiz-bench`) measures all of this against
a running service; see the README.

## An example round

```
→ [0,"<ticket>"]                          hello (player)
← [0,1793931348000]                        no game yet
← [1,1793931349100,"Fractions",3,12]       the host opened a lobby; 12 players
← [2,1793931351000,1,3,1793931371000,0,"1/2 + 1/2 = ?",["1","2","0","1/4"]]
→ [4,1,1]                                  answer question 1: option 1
← [9]
← [5,1,1,[8,3,1,0],"one whole",[["An",1000],["Binh",990]],990,2,990,1,1,1,0]
                                           score 990, rank 2, 990 points, right,
                                           chose option 1, streak 1, bonus 0
← [2,...]                                  question 2 ... (right again: +100 bonus)
← [6,[["An",2980],["Binh",2960],["Chi",2100]],2960,2]
← [7,0]                                    the host ended the game
```

And what the host sees of the same round, besides the question and the reveal:

```
← [10,["An","Binh","Chi"]]                  who joined (lobby, mid-game)
← [4,1,12,[0,1,0,0],[["An",990],["Binh",0],["Chi",0]]]
                                           1 of 12 answered; one chose option 1; the
                                           provisional top ten (at most 10 rows)
← [4,9,12,[1,6,2,0],[["An",990],["Chi",980],["Binh",960],...]]
                                           at most every 500 ms
```
