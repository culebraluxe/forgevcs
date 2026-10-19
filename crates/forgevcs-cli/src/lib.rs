//! `forgevcs` — the human CLI for the ForgeVCS REST façade.
//!
//! This crate is deliberately thin: it parses arguments, stores a bearer token locally (0600),
//! and calls the routes in [`forgevcs_service::http`]'s router. It has no ForgeVCS business
//! logic, no Git wire protocol and no deploy/publish path; every operation is one manifest call
//! the service already implements.
//!
//! The library/binary split exists so the command layer is testable against an in-process HTTP
//! server without spawning a process.

pub mod args;
pub mod client;
pub mod commands;
pub mod settings;
pub mod token;

use anyhow::{Context, Result};

use crate::args::Parsed;

/// Run one invocation, printing the pretty JSON result on success.
pub async fn run(args: Vec<String>) -> Result<()> {
    match args::parse(args)? {
        Parsed::Help(text) => {
            print!("{text}");
            Ok(())
        }
        Parsed::Run(global, command) => {
            let value = commands::execute(global, command).await?;
            let text = serde_json::to_string_pretty(&value).context("formatting the response")?;
            println!("{text}");
            Ok(())
        }
    }
}
