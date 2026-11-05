//! quiz-service: a high performance GraphQL quiz backend on a custom-built
//! SQLite (sqlite/sqlite `begin-concurrent` branch) linked into sqlx.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod grading;
pub mod graphql;
pub mod ids;
pub mod model;
pub mod repo;
pub mod state;
pub mod telemetry;
pub mod tx;

pub use error::AppError;
pub use state::AppState;

/// JSON body helpers used by tests and the loadgen.
pub mod util {
    pub fn graphql_body(query: &str) -> String {
        serde_json::json!({ "query": query }).to_string()
    }

    pub fn graphql_body_with_vars(query: &str, variables: serde_json::Value) -> String {
        serde_json::json!({ "query": query, "variables": variables }).to_string()
    }
}
