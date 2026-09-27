//! `interjectd` internals, exposed as a library so integration tests can drive
//! the real daemon rather than a mock of it.

pub mod api;
pub mod daemon;
pub mod notify;
pub mod store;
pub mod triage;
pub mod types;
