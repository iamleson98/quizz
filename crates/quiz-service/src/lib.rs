//! quiz-service: the live quiz game and the teacher's quiz bank, on a
//! custom-built SQLite (sqlite/sqlite `begin-concurrent` branch) linked into
//! sqlx.

pub mod auth;
pub mod clock;
pub mod config;
pub mod db;
pub mod error;
pub mod game;
pub mod grading;
pub mod ids;
pub mod model;
pub mod push;
pub mod ratelimit;
pub mod repo;
pub mod rest;
pub mod retention;
pub mod server;
pub mod snapshot;
pub mod state;
pub mod telemetry;
pub mod tx;
pub mod ws;

pub use error::AppError;
pub use state::AppState;
