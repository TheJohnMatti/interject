//! `interjectd` internals, exposed as a library so integration tests can drive
//! the real HTTP stack rather than a mock of it.

pub mod api;
pub mod store;
pub mod types;
