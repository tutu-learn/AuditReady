//! Outbound remote shell tunnel.
//!
//! The agent dials the broker over a single WebSocket. The broker can open
//! multiple independent shell channels over that socket; each channel gets its
//! own PTY on the agent.

mod client;
mod pty;

pub use client::run;
