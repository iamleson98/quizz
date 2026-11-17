-- The games being played right now, one row per room, so that a restart (a
-- deploy, an eviction, an out-of-memory kill) puts them back as they were.
--
--  * `snapshot` is the game's state, versioned by `version`; a row of another
--    version is refused at startup and its game is closed.
--  * A row lives only while its game does: it is deleted when the game ends
--    or its room closes, and the expiry sweep deletes the rows that were not
--    written for a day (see retention.rs).

CREATE TABLE running_games (
    room       TEXT PRIMARY KEY,
    game_id    INTEGER NOT NULL,
    -- 0 race, 1 quest
    kind       INTEGER NOT NULL,
    version    INTEGER NOT NULL,
    snapshot   BLOB NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_running_games_game ON running_games(game_id);
CREATE INDEX idx_running_games_updated ON running_games(updated_at);

-- A game that is put back replays the close of the question it was in; with
-- this, an answer that had already been written is not written twice (an
-- insert that meets the same answer id or the same player and question is
-- ignored). Any duplicate there may already be goes first, so the index can
-- always be built; ids are random, so which of the duplicates stays is
-- arbitrary (the lowest id).
DELETE FROM answers WHERE id NOT IN (SELECT MIN(id) FROM answers GROUP BY player_id, question_id);
CREATE UNIQUE INDEX idx_answers_player_question ON answers(player_id, question_id);
