//! The REST client: one method per HTTP verb the façade uses.
//!
//! The CLI is a thin wrapper, so this file contains no ForgeVCS business logic. It attaches the
//! bearer token, sends JSON, and turns a non-2xx response into an error carrying the façade's
//! own `code`/`message` envelope — a `403 permission_denied` from the service reaches the human
//! as exactly that, not as "request failed".

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// A client for one service base URL and (optionally) one bearer token.
pub struct Client {
    base_url: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Client {
    /// Build a client. `base_url` must already be trimmed of a trailing slash.
    pub fn new(base_url: &str, token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("forgevcs-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            base_url: base_url.to_owned(),
            token,
            http,
        })
    }

    /// The base URL this client talks to.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether a bearer token is attached.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// `GET path?query` and parse the JSON response.
    pub async fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value> {
        let builder = self.request(reqwest::Method::GET, path)?;
        let builder = if query.is_empty() {
            builder
        } else {
            builder.query(query)
        };
        self.send(builder).await
    }

    /// `POST path` with a JSON body and parse the JSON response.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let builder = self.request(reqwest::Method::POST, path)?.json(body);
        self.send(builder).await
    }

    /// `DELETE path` and parse the JSON response.
    pub async fn delete(&self, path: &str) -> Result<Value> {
        let builder = self.request(reqwest::Method::DELETE, path)?;
        self.send(builder).await
    }

    fn request(&self, method: reqwest::Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let mut builder = self.http.request(method, &url);
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }
        Ok(builder)
    }

    async fn send(&self, builder: reqwest::RequestBuilder) -> Result<Value> {
        let response = builder.send().await.context("sending the request")?;
        let status = response.status();
        let body = response.text().await.context("reading the response body")?;
        // A non-JSON body (a proxy error page, say) is still reported verbatim.
        let value: Value = match serde_json::from_str(&body) {
            Ok(value) => value,
            Err(_) => Value::String(body),
        };
        if !status.is_success() {
            let code = value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("http_error");
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| value.to_string(), str::to_owned);
            bail!("HTTP {status}: {code}: {message}");
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_client_is_built_without_network_and_reports_its_token() {
        let client = Client::new("http://127.0.0.1:9", Some("tok".to_owned())).expect("client");
        assert_eq!(client.base_url(), "http://127.0.0.1:9");
        assert!(client.has_token());
        let anonymous = Client::new("http://127.0.0.1:9", None).expect("client");
        assert!(!anonymous.has_token());
    }
}
