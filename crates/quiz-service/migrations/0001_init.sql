-- Quiz service schema, tuned for the SQLite `begin-concurrent` branch.
--
-- Design notes:
--  * Row ids are RANDOM 62-bit integers assigned by the application.
--    Monotonically increasing rowids concentrate concurrent inserts on the
--    same database page and cause SQLITE_BUSY_SNAPSHOT conflicts under
--    BEGIN CONCURRENT.
--  * Owner and player ids are TEXT: they are the Go server's ids, opaque to
--    this service. There are no accounts here.
--  * Timestamps are unix epoch milliseconds (INTEGER).
--  * Nothing lasts for ever: every quiz carries `expires_at` and every ended
--    game carries `expires_at`; a sweep deletes what has expired.

CREATE TABLE quizzes (
    id         INTEGER PRIMARY KEY,
    owner_id   TEXT NOT NULL,
    title      TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    kept       INTEGER NOT NULL,
    -- last use + 90 days when kept, + 7 days when not (see retention.rs)
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_quizzes_owner  ON quizzes(owner_id, created_at DESC);
CREATE INDEX idx_quizzes_expiry ON quizzes(expires_at);

CREATE TABLE questions (
    id             INTEGER PRIMARY KEY,
    quiz_id        INTEGER NOT NULL REFERENCES quizzes(id) ON DELETE CASCADE,
    position       INTEGER NOT NULL,
    kind           TEXT NOT NULL CHECK (kind IN ('single', 'multi', 'boolean')),
    text           TEXT NOT NULL,
    time_limit_sec INTEGER NOT NULL CHECK (time_limit_sec BETWEEN 5 AND 120),
    explanation    TEXT
);
CREATE INDEX idx_questions_quiz ON questions(quiz_id, position);

CREATE TABLE options (
    id          INTEGER PRIMARY KEY,
    question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    text        TEXT NOT NULL,
    is_correct  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_options_question ON options(question_id, position);

CREATE TABLE games (
    id         INTEGER PRIMARY KEY,
    quiz_id    INTEGER NOT NULL REFERENCES quizzes(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    class_id   TEXT NOT NULL,
    host_id    TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at   INTEGER,
    -- ended_at + 90 days; NULL while the game runs
    expires_at INTEGER
);
CREATE INDEX idx_games_quiz   ON games(quiz_id, started_at DESC);
CREATE INDEX idx_games_expiry ON games(expires_at) WHERE expires_at IS NOT NULL;

CREATE TABLE players (
    id            INTEGER PRIMARY KEY,
    game_id       INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    -- NULL once the person is forgotten (their name is blanked too)
    user_id       TEXT,
    name          TEXT NOT NULL,
    joined_at     INTEGER NOT NULL,
    score         INTEGER NOT NULL DEFAULT 0,
    total_ms      INTEGER NOT NULL DEFAULT 0,
    correct_count INTEGER NOT NULL DEFAULT 0,
    UNIQUE (game_id, user_id)
);
CREATE INDEX idx_players_user ON players(user_id) WHERE user_id IS NOT NULL;

CREATE TABLE answers (
    id          INTEGER PRIMARY KEY,
    game_id     INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    player_id   INTEGER NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
    selected    TEXT NOT NULL,      -- JSON array of chosen option ids
    correct     INTEGER NOT NULL,
    credit      REAL NOT NULL,      -- 0..1, partial credit for multiple choice
    points      INTEGER NOT NULL,   -- includes the streak bonus
    bonus       INTEGER NOT NULL DEFAULT 0,
    time_ms     INTEGER NOT NULL
);
CREATE INDEX idx_answers_game   ON answers(game_id, question_id);
CREATE INDEX idx_answers_player ON answers(player_id);

-- Connected stretches of running games (a host's, each player's), waiting to
-- be pushed to the Go server, which counts them as lesson time. A row stays
-- until the push that carried its end has been acknowledged and it is
-- closed; a host's open row is rewritten with a later `to_ms` every minute.
CREATE TABLE spans_outbox (
    game_id    INTEGER NOT NULL,
    user_id    TEXT NOT NULL,
    session_id TEXT NOT NULL,
    role       TEXT NOT NULL CHECK (role IN ('host', 'player')),
    from_ms    INTEGER NOT NULL,
    to_ms      INTEGER NOT NULL,
    closed     INTEGER NOT NULL DEFAULT 0,
    -- the `to_ms` the Go server has acknowledged; -1 when none
    pushed_ms  INTEGER NOT NULL DEFAULT -1,
    PRIMARY KEY (game_id, user_id, from_ms)
) WITHOUT ROWID;
