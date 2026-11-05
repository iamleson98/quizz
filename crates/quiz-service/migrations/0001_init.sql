-- Quiz service schema, tuned for the SQLite `begin-concurrent` branch.
--
-- Design notes (see third_party/sqlite/doc/begin_concurrent.md):
--  * All primary keys are RANDOM 62-bit integers assigned by the application.
--    Monotonically increasing rowids concentrate concurrent inserts on the
--    same database page and cause SQLITE_BUSY_SNAPSHOT conflicts under
--    BEGIN CONCURRENT.
--  * Timestamps are unix epoch milliseconds (INTEGER) - compact, sortable.
--  * Secondary indexes avoid clustering on "current time" keys where
--    practical; the leaderboard index is the accepted hotspot (documented
--    tradeoff, mitigated by transaction retry).

CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE quizzes (
    id             INTEGER PRIMARY KEY,
    creator_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title          TEXT NOT NULL,
    description    TEXT NOT NULL DEFAULT '',
    is_public      INTEGER NOT NULL DEFAULT 1,
    time_limit_sec INTEGER,
    max_attempts   INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX idx_quizzes_public ON quizzes(is_public, created_at DESC);

CREATE TABLE questions (
    id          INTEGER PRIMARY KEY,
    quiz_id     INTEGER NOT NULL REFERENCES quizzes(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('single', 'multi', 'boolean')),
    text        TEXT NOT NULL,
    points      REAL NOT NULL DEFAULT 1 CHECK (points > 0),
    explanation TEXT
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

CREATE TABLE attempts (
    id           INTEGER PRIMARY KEY,
    quiz_id      INTEGER NOT NULL REFERENCES quizzes(id) ON DELETE CASCADE,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status       TEXT NOT NULL DEFAULT 'in_progress'
                 CHECK (status IN ('in_progress', 'graded')),
    started_at   INTEGER NOT NULL,
    submitted_at INTEGER,
    score        REAL,
    max_score    REAL
);
-- Leaderboard: graded attempts for a quiz, best score first, earliest first.
CREATE INDEX idx_attempts_leaderboard
    ON attempts(quiz_id, status, score DESC, submitted_at ASC);
CREATE INDEX idx_attempts_user ON attempts(user_id, quiz_id);

CREATE TABLE answers (
    id          INTEGER PRIMARY KEY,
    attempt_id  INTEGER NOT NULL REFERENCES attempts(id) ON DELETE CASCADE,
    question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
    selected    TEXT NOT NULL,      -- JSON array of chosen option ids
    is_correct  INTEGER NOT NULL,
    awarded     REAL NOT NULL,
    possible    REAL NOT NULL
);
CREATE INDEX idx_answers_attempt ON answers(attempt_id);
