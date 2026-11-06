//! The live game: rooms, frames, scoring and lesson-time spans.

pub mod frames;
pub mod registry;
pub mod room;
pub mod scoring;
pub mod spans;
pub mod store;

pub use registry::{Deps, Rooms};

#[cfg(test)]
mod tests;
