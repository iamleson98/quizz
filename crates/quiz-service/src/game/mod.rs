//! The live game: rooms, frames and scoring.

pub mod frames;
pub mod quest;
pub mod registry;
pub mod room;
pub mod scoring;
pub mod store;

pub use registry::{Deps, Rooms};

#[cfg(test)]
mod tests;
