//! One shared Postgres pool builder for ForgeVCS-owned tables (workspaces, A/B decisions).
//!
//! The vendored Scion store owns its own pool; this one is only for tables that did not exist
//! upstream. Neon requires TLS, so the webpki roots from `scion-store-crdb` are reused.

use std::time::Duration;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Build a small pool for ForgeVCS-owned tables. The URL must be a Postgres/Neon URL; TLS is
/// mandatory because Neon rejects plaintext.
pub(crate) fn build_pool(url: &str) -> Result<Pool, String> {
    let pg: tokio_postgres::Config = url
        .parse()
        .map_err(|error| format!("invalid database url: {error}"))?;
    let tls = scion_store_crdb::dialect::tls_client_config()
        .ok_or_else(|| "no TLS client configuration for Neon".to_owned())?;
    let manager = Manager::from_config(
        pg,
        MakeRustlsConnect::new((*tls).clone()),
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(4)
        .runtime(Runtime::Tokio1)
        .create_timeout(Some(Duration::from_secs(10)))
        .recycle_timeout(Some(Duration::from_secs(5)))
        .build()
        .map_err(|error| error.to_string())
}
