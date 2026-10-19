//! `forgevcs` binary entry point: errors go to stderr with a non-zero exit status.
//!
//! All behavior lives in the library so integration tests drive the identical command path.

#[tokio::main]
async fn main() {
    let args = std::env::args().skip(1).collect();
    if let Err(error) = forgevcs_cli::run(args).await {
        eprintln!("forgevcs: {error:#}");
        std::process::exit(1);
    }
}
