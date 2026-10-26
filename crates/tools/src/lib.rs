//! Tools: the registry that runs them and the schemas that describe them.
//!
//! The registry is the interesting part. It is the only way to invoke a tool,
//! which is what makes the permission check unskippable rather than merely
//! conventional.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::needless_pass_by_value
    )
)]

pub mod registry;
pub mod schema;
pub mod truncate;

pub use registry::{shared, Limits, Registry};
