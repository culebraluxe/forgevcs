//! ForgeVCS service library: the pieces an integration test or another bin can reuse.
//!
//! The engine wiring ([`engine`]), the HTTP surface ([`http`]) and the business façade
//! ([`facade`]) live here rather than in the binary, so tests drive the identical router and
//! `Config` the deployed service uses — a test that re-implemented the variable mapping or the
//! routes would prove nothing about the service. `main.rs` keeps startup, shutdown and the
//! `axum::serve` call.

pub mod authz;
pub mod config;
pub mod db;
pub mod engine;
pub mod escalations;
pub mod facade;
pub mod gc;
pub mod http;
pub mod workspace;
