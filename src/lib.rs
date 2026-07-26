//! herdr-board internals, exposed as a library so integration tests can drive a
//! full sync cycle against recorded fixtures.

pub mod cli;
pub mod config;
pub mod db;
pub mod dispatch;
pub mod gc;
pub mod herdr;
pub mod log;
pub mod model;
pub mod sources;
pub mod sync;
pub mod ui;
