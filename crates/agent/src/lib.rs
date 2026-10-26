//! The agent turn loop.
//!
//! This crate is the reason the workspace is split the way it is. It holds the
//! orchestration - when to call the model, when to run a tool, when to stop -
//! and it holds no implementation of any of those things. It depends on
//! `openslacktag-core` and nothing else from the workspace.
//!
//! The practical consequence: the loop in [`turn::Agent::run`] is the same
//! code whether it is driving a real provider against a real filesystem or a
//! scripted fake against a temp directory. The tests below and the headless
//! harness in `openslacktag-devkit` both exercise the shipping path, not a
//! parallel one written for testing.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod event;
pub mod steer;
pub mod turn;

pub use event::{AgentEvent, StopReason};
pub use steer::{Closing, Steer};
pub use turn::{Agent, AgentConfig, Turn, DOOM_LOOP_KEY, LOOP_THRESHOLD, MAX_STEPS, STEER_NOTE};
