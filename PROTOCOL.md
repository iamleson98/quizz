# The game socket: protocol `quiz.v1`

The live game runs on one websocket, `GET /ws` on the public listener. The
bank (quizzes, results) is the REST API in [`openapi.json`](openapi.json); this
document is the other half of the contract, and the web client is written from
it. The game has two modes: the class-paced **race**, described first, and the
self-paced **quest**, described at the end ("Quest mode").

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
  `hello`, within 2 s**; otherwise the socket gets `[8,1,0]` and is closed.
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
| `[2]` | host | **next**: start the first question; close the open question; reveal a closed question at once (skipping the beat); move on from a reveal; leave the podium (ends the game) |
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
| 11 | `[11,q]` | **closed**: question `q` is closed (no more answers); its reveal follows after a beat |
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
- **Closed, then the beat, then the reveal.** The moment a question closes
  every socket (players and host) gets the tiny shared `[11,q]`: players lock
  their screen, the host shows "revealing". The reveal (`[5,...]`, which carries
  the correct options and the ranking) is sent only `REVEAL_DELAY_MS = 2500` ms
  later, so the class waits for it together. Until then **nothing on any socket
  carries the correct options or a ranking**: not the question, not the
  progress frames, not the closed frame. A host `next` during the beat reveals
  at once; a `next` after the reveal moves on, as before (so pressing it twice
  while a question is open closes it and then reveals it; a press that arrives
  after the beat's own timer is simply the "move on" press). An answer that
  arrives during the beat is refused with `[8,5,4]`. The provisional ranking
  frames to the host continue as long as anything changes, which is only until
  the close. If the game ends during the beat (the host's `end`, the room
  closing) the reveal is sent first and the end after it.
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
| closed, waiting for the reveal | `[11,q]` (the reveal follows by itself) |
| reveal | the reveal frame with your personal tail |
| podium | the podium frame |

So a client that reconnects (back after a lost network, a reload) simply sends
`hello` again and renders whatever arrives first. A player keeps their score,
rank and answer: they are the game's, keyed by the ticket's `sub`. A player who
opens a second tab is the same player (up to four sockets per person; see below). Players
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

- Up to **4 sockets per person**: a fifth is accepted and the **oldest** is closed
  (1008, "replaced by a newer socket"), so a phone that lost its network never locks
  the person out. 500 players per game, one running game per session and per
  teacher. The service also limits sockets that have not said hello yet, per client
  address (64, enough for a school behind one address) and in all, so say hello
  at once.
- A client frame is at most 128 bytes after hello (2 KiB for hello). A longer
  one closes the socket with code 1009.
- A burst of 10 frames, then 2 per second: faster than that is answered
  `[8,6,0]` and the socket is closed (1008).
- A socket that cannot take the frames sent to it (its small queue stays full)
  is closed with 1013; reconnect and resume.
- The server pings every 25 s (a websocket ping, not a frame); browsers answer by
  themselves, and a peer that has not answered 10 s after a ping is closed (1008).
  A frame the peer does not take within 10 s drops the socket too. There is no
  application heartbeat: do not send one.
- A game ends by itself when the ticket's `rc` passes (reason `1`) or after an
  hour without a host command or an answer (reason `2`). A deploy of the
  service ends running games (reason `3`) and closes sockets with 1001.

## What it costs

Per player per question (80 characters, four options of 30): the question frame
(~250 B), a 5-byte ack, the 8-byte closed frame and the reveal with the
player's own result (~160 B) are received: 4 frames, about 435 B on the wire
including websocket headers; one
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
← [11,1]                                   closed: lock the screen
                                           ... 2.5 s later, or when the host presses next:
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

---

# Quest mode (`quiz.v1`, same socket)

Quest is a second game mode beside the race above. Everything about the
socket, hello, tickets, errors `[8,code,request]`, the limits and the
resync-on-hello rule applies unchanged. Quest adds client frames 5 to 8 and
server frames 20 to 29; nothing in the race uses those numbers. The race's
`[2]` next, `[3]` end, `[6]` podium, `[7]` ended and `[3,players]` count are
reused. Quest has its own rules and shape: **self-paced**. Every student
answers at their own speed from their own shuffled queue; a right answer earns
coins and a treasure pick; the teacher watches a live board and the time runs
out.

## Starting

`[1,"quizId",mode,seconds,flags]` (host). `mode` `0` is the race (the
one-element form `[1,"quizId"]` stays valid and means the same); `1` is quest.
`seconds` must be `300`, `600` or `900` (5, 10 or 15 minutes), otherwise
`[8,4,1]`. `flags` bit 0 is **friendly mode**: no steals.

| Phase | What happens |
|---|---|
| lobby | everybody gets `[20,...]`; players may choose a character; the host sees who is there |
| running | the host's `[2]` starts the clock; every player gets their first question at once |
| podium | at the end of the time, `[6,...]` (coins are the score); answers are refused |
| ended | the host's `[2]` (or `[3]` at any time, or the room closing) ends it: `[7,reason]`, results stored |

A second `[2]` while the game is running is refused `[8,5,2]`.

## Client to server

| Frame | Who | Meaning |
|---|---|---|
| `[5,character]` | player | choose a character, `0` to `11` (lobby or running); `[8,4,5]` for another number |
| `[6,serial,mask]` | player | answer the question served with this `serial`; `mask` as in the race (one bit for single choice and true/false, the exact set for multiple choice) |
| `[7,serial,chest]` | player | open chest `0`, `1` or `2` of the verdict with this `serial` |
| `[8]` | player | ask for the question again (with the time you have left), or for the next one if none is open |

Refused with `[8,code,request]` (`request` is the frame's first number):

| Case | Code |
|---|---|
| an answer or pick for a serial that is not the current one | 5 |
| a second answer (the first locks), an answer after the time, an answer while a chest is waiting or a pause runs | 5 |
| an invalid choice of options (no bit, a bit beyond the options, several bits for a single question); the question stays open | 4 |
| an answer sooner than 800 ms after its question was served (nobody reads, thinks and taps that fast): refused, and the question stays open | 5 |
| an answer or pick the socket received after the game's time was up (even a few ms) | 5 |
| a pick without a chest waiting, or of a chest already opened | 5 |
| a chest number above 2 | 4 |
| `[8]` during the pause, or while a chest is waiting | 5 |
| any quest frame before the game runs, or after the time is up | 5 |
| a quest frame from the host, a race frame in a quest game, `[1,...]` from a player | 2 / 5 |

The pause after a wrong answer is the server's: a player cannot get the next
question earlier by asking.

## Server to client

| Type | Frame | To | When |
|---|---|---|---|
| 20 | `[20,now,"title",questions,players,seconds,flags]` | all | the lobby; also the first frame of anyone arriving in it |
| 21 | `[21,[["name",character],...]]` | host | who joined or changed character, at most every 500 ms; once in full after the host's hello |
| 22 | `[22,now,endsAt]` | all | the game is running until `endsAt` (the server's clock) |
| 23 | `[23,coins,rank,streak,booster,shield,character,serial,pending,pauseMs]` | player | after hello, your whole state (below) |
| 24 | `[24,kind,"text",["opt",...],limitSec,serial,remainingMs]` | player | a question for you |
| 25 | `[25,serial,1,earned,streak,bonus,coins,boosterLeft]` | player | verdict: right |
| 25 | `[25,serial,0\|2,0,0,0,coins,correctMask,pauseMs,explanation]` | player | verdict: wrong (0) or missed (2) |
| 26 | `[26,serial,picked,kind,amount,coins,[k0,a0,k1,a1,k2,a2],victim,blocked]` | player | what the chest held |
| 27 | `[27,0,"thief",amount,coins]` / `[27,1,"thief"]` / `[27,2]` | player | stolen from you / your shield blocked a steal / your booster ran out |
| 28 | `[28,[["name",character,coins]x5],rank,coins]` | player | the standings strip |
| 29 | `[29,[["name",character,coins]x10],players,answers,correct,msLeft]` | host | the live board |
| 3 | `[3,players]` | all | the lobby's player count (race frame, reused) |
| 6 | `[6,[["name",coins]x3],coins,rank]` / host: `[6,top3,all]` | all | the podium at the end of the time (race frame, reused) |
| 7 | `[7,reason]` | all | ended |

Details:

- **Question (24).** `kind` is `0` single choice, `1` multiple choice, `2`
  true/false, as in the race. Options keep their authored order, so the
  frame's shared part is encoded once per question for everyone; only
  `serial` and `remainingMs` are yours. `serial` counts the questions served to
  *you* (1, 2, 3, ...); every answer and pick names it. `limitSec` is the longest
  you may think: past it the question counts as missed. The answer key is never
  in a question frame.
- **Verdict (25).** `earned` is the answer's coins (double while a booster
  runs), `streak` your right answers in a row counting this one, `bonus` the
  streak bonus part of `earned` before doubling, `coins` your total, `boosterLeft`
  right answers left that earn double. A right answer also means three chests are
  waiting: pick one with `[7,serial,chest]`. A wrong or missed one shows
  `correctMask` (bits of the right options) and `explanation` (a string or
  `null`) and starts the pause of `pauseMs` (2500): the next question arrives by
  itself when it is over. Missed (`2`) arrives when the time is up.
- **Chest result (26).** `picked` is the chest you opened; `kind`/`amount` its
  effect: kind `0` coins (+amount), `1` jackpot (+200), `2` booster (3 answers),
  `3` shield, `4` steal (amount taken; `0` when blocked), `5` slip (amount lost,
  never more than you had). `coins` is your new total. The array lists all three
  chests as `kind,amount` pairs, shown afterwards for fun (an unpicked steal shows
  amount `0`). For a steal, `victim` is the name of the player it was aimed at and
  `blocked` is `1` when their shield stopped it; otherwise `null` and `0`. The
  next question follows in its own frame.
- **Events (27).** `[27,0,"Binh",12,80]`: Binh took 12 coins, you have 80 left.
  `[27,1,"Binh"]`: your shield blocked Binh's steal (the shield is used up).
  `[27,2]`: your booster has run out. These go only to the player they concern.
- **Your state (23)** after a hello: `rank` is as of the last standings (0 at
  first), `booster` the right answers left that earn double, `shield` 0/1,
  `character` 0 to 11, `serial` the last question served to you, `pending` what
  you are in the middle of (`0` nothing, `1` a question is open and follows
  in its own frame with the time left, `2` a chest is waiting for `[7,serial,chest]`,
  `3` the pause is running with `pauseMs` left; the next question comes by
  itself). Right after it you get `[22,...]` first. A second socket of the same
  player gets the same.
- **Standings (28).** Top five with their characters and coins, then your own
  rank and coins. At most once every 3 seconds, only to a player for whom
  something changed (the top five, their rank, their coins). The shared part is
  encoded once. A late joiner or a reconnect gets it at the next tick.
- **Board (29)** for the host: the top ten by coins, the players, the answers
  given so far, the right ones (so accuracy is `correct/answers`), and the ms
  left. At most every 500 ms and only when something changed. A host that says
  hello gets one at once.
- **Roster (21)**: the host's list of names with their characters (increments in
  the lobby and when somebody changes character; everything after a hello).
  Players are never sent names other than in the standings and the podium.
- **Podium (6)**: the race frame with coins as the score: the top three, your own
  coins and rank; the host gets the full order. Then `[2]` (or `[3]`) from the host
  ends it: `[7,0]`. The service closing, the room closing and an hour without a
  command give `[7,3]`, `[7,1]` and `[7,2]` as in the race.

## The rules (so the client can show them)

- **Questions.** Each player's queue is the quiz's questions in a shuffle of their
  own, repeated with a new shuffle when it runs out (a new round never begins with
  the question just asked). A player who joins mid-game gets a queue and the first
  question at once.
- **Coins for an answer.** A fully right answer earns `10`, plus `floor(10 x
  remaining/limit)` for speed, plus a streak bonus `min(5 x (streak - 1), 25)`.
  Multiple choice needs the exact set (no partial credit). While a **booster** runs
  (3 right answers) the answer's coins are doubled. A wrong or missed answer earns
  nothing, ends the streak and starts a pause of 2.5 s.
- **Chests.** After every right answer the server draws three hidden outcomes and
  keeps them until you pick; nothing you send changes them. Odds per thousand at a
  streak of 0:

| Outcome | Effect | Odds |
|---|---|---|
| +20 coins | | 33% |
| +40 coins | | 16.5% |
| +80 coins | | 5.5% |
| Jackpot | +200 coins | 4% |
| Booster | the next 3 right answers earn double | 12% |
| Shield | blocks the next steal against you (one at a time) | 10% |
| Steal | takes 10% of a rival's coins (at most 100) | 15% |
| Slip | -20 coins, never below 0 | 4% |

  From a streak of 3 the rare outcomes (+80 and the jackpot) are exactly twice as
  likely, from 6 three times; the others share what is left in their usual
  proportions. A **steal** takes from a random player ranked above you by coins
  (the runner-up when you lead). A shield blocks it and is used up; both of you are
  told. In friendly mode, or when there is nobody to steal from, a steal that comes
  up is replaced by coins. A player's coins never go below 0.
- **End.** When the time is up (the `endsAt` of frame 22, on the server's clock)
  the podium; the host ends the game. Results are stored once, at the end, for the
  bank's results list (`mode: "quest"`, per player coins, answered, correct, best
  streak). Nothing about a student is kept beyond that, and it expires like any
  game's results (90 days).

## An example (one player)

```
← [20,1793931348000,"Fractions",24,12,300,0]        lobby, 12 players, 5 minutes
→ [5,4]                                             I choose character 4
← [22,1793931360000,1793931660000]                  running until endsAt
← [24,0,"1/2 + 1/2 = ?",["1","2","0","1/4"],30,1,30000]
→ [6,1,1]                                           option 1
← [25,1,1,19,1,0,19,0]                              right: +19 coins, streak 1
→ [7,1,2]                                           open the third chest
← [26,1,2,0,20,39,[5,20,0,40,0,20],null,0]          +20 coins; the others held a slip and +40
← [24,2,"Pick the primes",[...],30,2,30000]         the next question
→ [6,2,2]                                           wrong
← [25,2,0,0,0,0,39,5,2500,"2 and 3 are prime"]     the right options, the explanation, a 2.5 s pause
← [24,...,3,30000]                                  by itself, 2.5 s later
← [28,[["Binh",2,130],["An",4,39],...],2,39]        the standings, every few seconds
← [27,0,"Binh",12,27]                               Binh stole 12 of my coins
```

## What a player costs

Measured by `quiz-bench --scenario quest` (see the README) with 300 simulated
players answering every 6 to 10 seconds (70% right, a chest opened after 1 to 2.5
s) over a steady window of 3 minutes:

| per player | measured | budget |
|---|---:|---:|
| received per second | 75 B | 400 B |
| frames received per second | 0.62 | 1.2 |
| sent per answer (the answer, and the chest pick when it was right) | 23 B | 30 B |
| frames sent per second | 0.17 | |
| standings strips received per second | 0.32 (one every 3 s) | |

The host's board goes out 2.0 times a second at most (272 B at the biggest, about
530 B/s), the service used 0.5% of one core, and its memory went from 7.3 MiB idle
to 10.0 MiB with 300 players in a game. A question is about 250 B, a verdict
25 B (100 B with an explanation), a chest result about 45 B, the standings strip
about 150 B.
