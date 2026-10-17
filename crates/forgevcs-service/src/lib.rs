//! ForgeVCS service library: the pieces an integration test or another bin can reuse.
//!
//! The HTTP wiring stays in `main.rs` (W1 served it from a single binary). Only the
//! environment configuration lives here, because W3 has to drive the *same*
//! [`config::Config::s3_config`] the deployed service uses — a test that re-implements the
//! variable mapping would prove nothing about the service.

pub mod config;
