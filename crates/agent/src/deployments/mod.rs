//! Dedicated WebSocket channel for receiving server deployments.

pub mod client;
mod protocol;
mod runner;

pub use client::run;
