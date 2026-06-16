//! HTTP client for the SpacetimeDB REST API.
//!
//! [`SpacetimeClient`] wraps a [`reqwest::Client`] and exposes typed methods
//! for every endpoint used by the TUI.  All methods are `async` and return
//! `anyhow::Result<T>` so that callers can use the `?` operator freely.

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, header};
use serde_json::Value;
use tracing::{debug, instrument, warn};

use super::types::{LogEntry, QueryResult, Schema, SchemaElement, SchemaResponse};

// ---------------------------------------------------------------------------
// Client struct
// ---------------------------------------------------------------------------

/// A thin, cheaply-cloneable HTTP client for SpacetimeDB.
///
/// All methods accept a `database` name and hit the appropriate endpoint on
/// the configured `base_url`.
#[derive(Debug, Clone)]
pub struct SpacetimeClient {
    /// Base URL, e.g. `http://localhost:3000`.
    base_url: String,
    /// Underlying HTTP client (connection-pooled, cheaply cloned).
    http: Client,
    /// Optional authentication token.
    auth_token: Option<String>,
}

impl SpacetimeClient {
    /// Create a new client pointing at `base_url`.
    ///
    /// # Errors
    /// Returns an error if `reqwest::Client` cannot be built (e.g. invalid
    /// TLS configuration).
    pub fn new(base_url: impl Into<String>, auth_token: Option<String>) -> Result<Self> {
        let base_url = base_url.into();
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );

        let http = Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            base_url,
            http,
            auth_token,
        })
    }

    /// Convenience constructor using host + port.
    #[allow(dead_code)]
    pub fn from_host_port(host: &str, port: u16, auth_token: Option<String>) -> Result<Self> {
        let base_url = format!("http://{}:{}", host, port);
        Self::new(base_url, auth_token)
    }

    /// The WebSocket base URL derived from the HTTP base URL.
    #[allow(dead_code)]
    pub fn ws_base_url(&self) -> String {
        self.base_url
            .replacen("http://", "ws://", 1)
            .replacen("https://", "wss://", 1)
    }

    /// Attach (or replace) the bearer auth token.
    #[allow(dead_code)]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Build a `GET` request, attaching the auth token when present.
    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        let req = self.http.get(url);
        self.maybe_auth(req)
    }

    /// Build a `POST` request, attaching the auth token when present.
    fn post(&self, url: &str) -> reqwest::RequestBuilder {
        let req = self.http.post(url);
        self.maybe_auth(req)
    }

    /// Build a `DELETE` request, attaching the auth token when present.
    fn delete(&self, url: &str) -> reqwest::RequestBuilder {
        let req = self.http.delete(url);
        self.maybe_auth(req)
    }

    fn maybe_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth_token {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }

    /// Send a request and deserialise the JSON body into `T`.
    #[allow(dead_code)]
    async fn send_json<T>(&self, req: reqwest::RequestBuilder) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let resp = req.send().await.context("HTTP request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("HTTP {status}: {body}");
        }
        resp.json::<T>()
            .await
            .context("Failed to decode JSON response")
    }

    // ------------------------------------------------------------------
    // Public API methods
    // ------------------------------------------------------------------

    /// Execute a SQL statement against `database` and return the result set.
    ///
    /// SpacetimeDB endpoint: `POST /v1/sql/<database>`
    #[instrument(skip(self, sql), fields(db = %database))]
    pub async fn query_sql(&self, database: &str, sql: &str) -> Result<QueryResult> {
        let url = format!("{}/v1/database/{}/sql", self.base_url, database);
        debug!("SQL query: {}", sql);

        let resp = self
            .post(&url)
            .body(sql.to_owned())
            .header(header::CONTENT_TYPE, "text/plain")
            .send()
            .await
            .context("SQL query request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("SQL query HTTP {status}: {body}");
        }

        // SpacetimeDB returns an array of result sets; we take the first.
        let raw: Value = resp.json().await.context("Failed to decode SQL response")?;
        parse_query_result(raw)
    }

    /// Fetch the full schema (tables, reducers, typespace) for `database`.
    ///
    /// SpacetimeDB endpoint: `GET /v1/database/<database>/schema?version=9`
    ///
    /// If version 9 returns a 500/501/503 (which happens on
    /// databases that were published with a newer module format the
    /// server knows about but our client doesn't), we surface a
    /// clearer error message rather than leaking the raw body — and
    /// still bail, because the rest of the UI can't function without
    /// a schema anyway.
    #[instrument(skip(self), fields(db = %database))]
    pub async fn get_schema(&self, database: &str) -> Result<Schema> {
        let url = format!("{}/v1/database/{}/schema", self.base_url, database);
        debug!("Fetching schema");

        let resp = self
            .get(&url)
            .query(&[("version", "9")])
            .send()
            .await
            .context("Schema request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // Surface "paused" as a typed error so the caller can flag
            // the database in the UI, not just show a one-off message.
            if is_paused_response(status.as_u16(), &body) {
                return Err(DatabasePaused {
                    database: database.to_string(),
                }
                .into());
            }
            bail!(schema_error_message(database, status.as_u16(), &body));
        }

        let raw: Value = resp
            .json()
            .await
            .context("Failed to decode schema response")?;
        parse_schema_response(raw)
    }

    /// Invoke a reducer (or procedure) in `database` with the supplied
    /// JSON-encoded arguments.
    ///
    /// SpacetimeDB endpoint:
    ///   `POST /v1/database/<database>/call/<reducer>`
    /// Body: a JSON array `[arg0, arg1, …]` whose elements are the
    /// already-encoded values of each parameter, in declaration order.
    ///
    /// On success returns the response body as a JSON `Value` so the
    /// caller can surface whatever the server reported (transaction
    /// id, energy used, error string, …) without us having to keep
    /// up with the server's response schema.
    #[instrument(skip(self, args), fields(db = %database, reducer = %reducer))]
    pub async fn call_reducer(
        &self,
        database: &str,
        reducer: &str,
        args: &[Value],
    ) -> Result<Value> {
        let url = format!(
            "{}/v1/database/{}/call/{}",
            self.base_url, database, reducer
        );
        debug!("Calling reducer with {} args", args.len());

        let body =
            serde_json::to_string(args).context("Failed to encode reducer arguments as JSON")?;

        let resp = self
            .post(&url)
            .body(body)
            .header(header::CONTENT_TYPE, "application/json")
            .send()
            .await
            .context("Reducer call request failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let snip: String = text.chars().take(200).collect();
            if status.as_u16() == 404 {
                bail!(
                    "Reducer '{reducer}' not found in '{database}' (HTTP 404). \
                     Check the spelling and that the module is published."
                );
            }
            if status.as_u16() == 400 {
                bail!(
                    "Reducer '{reducer}' rejected the call (HTTP 400). \
                     Argument count or types are likely wrong. Server said: {snip}"
                );
            }
            bail!("Reducer call HTTP {status}: {snip}");
        }
        // The server may return an empty body on success — fall back to Null.
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("Failed to decode reducer response")
    }

    /// Attach a new name (alias) to an existing database.
    ///
    /// SpacetimeDB endpoint: `POST /v1/database/<database>/names`
    /// Body: a bare JSON string containing the new name.
    ///
    /// The docs use the word "domain" for what we call an alias —
    /// a database can have any number of human-readable names and
    /// every `GET /schema` / `POST /sql` call works with any of them.
    #[instrument(skip(self), fields(db = %database, alias = %alias))]
    pub async fn add_database_alias(&self, database: &str, alias: &str) -> Result<()> {
        let url = format!("{}/v1/database/{}/names", self.base_url, database);
        debug!("Adding alias '{alias}' to {database}");

        // The endpoint expects a bare JSON string in the body, e.g. `"foo"`.
        let body = serde_json::to_string(alias).context("Failed to encode alias as JSON string")?;

        let resp = self
            .post(&url)
            .body(body)
            .header(header::CONTENT_TYPE, "application/json")
            .send()
            .await
            .context("Add-alias request failed")?;

        let status = resp.status();
        if status.is_success() {
            // The server returns `{"Success": {...}}` on happy path
            // but may return `{"PermissionDenied": {...}}` on refusal.
            // We don't surface the body; the status is authoritative.
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        let snip: String = body.chars().take(200).collect();
        match status.as_u16() {
            401 | 403 => bail!(
                "Adding alias to '{database}' rejected (HTTP {status}). \
                 The current token does not own this database."
            ),
            404 => bail!("Database '{database}' not found (HTTP 404)"),
            409 => bail!(
                "Alias '{alias}' already exists (HTTP 409). Pick a \
                 different name or reuse the existing one."
            ),
            _ => bail!("Add-alias HTTP {status}: {snip}"),
        }
    }

    /// List every alias / human name this database can be reached
    /// under. Used by the sidebar to show the full list alongside
    /// the selected identity.
    ///
    /// SpacetimeDB endpoint: `GET /v1/database/<database>/names`
    /// Response: `{"names": ["alias1", "alias2", ...]}`
    #[instrument(skip(self), fields(db = %database))]
    pub async fn get_database_names(&self, database: &str) -> Result<Vec<String>> {
        let url = format!("{}/v1/database/{}/names", self.base_url, database);
        let resp = self
            .get(&url)
            .send()
            .await
            .context("get-names request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snip: String = body.chars().take(200).collect();
            bail!("get-names HTTP {status}: {snip}");
        }
        let raw: Value = resp
            .json()
            .await
            .context("Failed to decode names response")?;
        // Response wraps the list: `{"names":[...]}`. Tolerate a
        // bare array too in case that ever changes.
        let list = match raw {
            Value::Object(ref o) => o
                .get("names")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            Value::Array(arr) => arr,
            _ => Vec::new(),
        };
        Ok(list
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect())
    }

    /// Permanently delete a database.
    ///
    /// SpacetimeDB endpoint: `DELETE /v1/database/<database>`
    /// Requires the bearer token to belong to the database's owner;
    /// anonymous requests are rejected with HTTP 401 / 403.
    ///
    /// On the wire the server returns an empty body on success, so we
    /// don't try to decode anything — only the HTTP status matters.
    #[instrument(skip(self), fields(db = %database))]
    pub async fn delete_database(&self, database: &str) -> Result<()> {
        let url = format!("{}/v1/database/{}", self.base_url, database);
        debug!("Deleting database");

        let resp = self
            .delete(&url)
            .send()
            .await
            .context("Delete database request failed")?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        let body_snip: String = body.chars().take(200).collect();
        match status.as_u16() {
            401 | 403 => bail!(
                "Delete '{database}' rejected (HTTP {status}). The current \
                 token does not own this database — try `spacetime login` \
                 with the owner identity, or pass `--token` explicitly."
            ),
            404 => bail!("Database '{database}' not found (HTTP 404)"),
            _ => bail!("Delete database HTTP {status}: {body_snip}"),
        }
    }

    /// Retrieve the last `num_lines` log lines for `database`.
    ///
    /// SpacetimeDB endpoint: `GET /v1/database/<database>/logs`
    ///
    /// When `follow` is `true` the server streams logs; this method collects
    /// the whole stream until EOF and returns all lines.  For live streaming
    /// use [`crate::api::ws::WsClient`] instead.
    #[instrument(skip(self), fields(db = %database))]
    pub async fn get_logs(
        &self,
        database: &str,
        num_lines: u32,
        follow: bool,
    ) -> Result<Vec<LogEntry>> {
        let url = format!("{}/v1/database/{}/logs", self.base_url, database);
        debug!(num_lines, follow, "Fetching logs");

        let resp = self
            .get(&url)
            .query(&[
                ("num_lines", num_lines.to_string()),
                ("follow", follow.to_string()),
            ])
            .send()
            .await
            .context("Logs request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Logs HTTP {status}: {body}");
        }

        // The server returns newline-delimited JSON (one object per line).
        let text = resp.text().await.context("Failed to read log body")?;
        parse_ndjson_logs(&text)
    }

    /// List all databases visible to the authenticated identity.
    ///
    /// SpacetimeDB 2.0 endpoint: `GET /v1/identity/{hex_identity}/databases`
    ///
    /// The identity is extracted directly from the JWT bearer token without
    /// making an additional network request.  The response contains database
    /// *identities* (hex strings), which are valid database references for all
    /// other API endpoints.
    #[instrument(skip(self))]
    pub async fn list_databases(&self) -> Result<Vec<String>> {
        // Extract the caller's identity from the JWT payload.
        let identity = self
            .auth_token
            .as_deref()
            .and_then(extract_identity_from_jwt)
            .ok_or_else(|| {
                anyhow!(
                    "No auth token configured or cannot parse identity from JWT.\n\
                     Run `spacetime server login` or pass --token."
                )
            })?;

        let url = format!("{}/v1/identity/{}/databases", self.base_url, identity);
        debug!(
            "Listing databases for identity {}…",
            &identity[..identity.len().min(12)]
        );

        let resp = self
            .get(&url)
            .send()
            .await
            .context("List databases request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("List databases HTTP {status}: {body}");
        }

        let raw: Value = resp
            .json()
            .await
            .context("Failed to decode databases response")?;

        // Response: {"identities": ["hex1", "hex2", ...]}
        let identities: Vec<String> = raw
            .get("identities")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();

        // Resolve each identity to its friendly name(s) via
        // GET /v1/database/:identity/names → {"names": ["db-name"]}
        // Fall back to the raw identity string if the endpoint fails.
        let mut names: Vec<String> = Vec::new();
        for id in &identities {
            match self.get_database_names(id).await {
                Ok(db_names) if !db_names.is_empty() => names.extend(db_names),
                _ => names.push(id.clone()),
            }
        }

        Ok(names)
    }

    /// Ping the server and return `true` if it responds.
    pub async fn ping(&self) -> bool {
        let url = format!("{}/v1/ping", self.base_url);
        self.get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// Fetch server metrics (Prometheus format).
    pub async fn get_metrics(&self) -> Result<String> {
        let url = format!("{}/metrics", self.base_url);
        let resp = self
            .get(&url)
            .send()
            .await
            .context("Metrics request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Metrics HTTP {status}: {body}");
        }
        resp.text().await.context("Failed to read metrics body")
    }

    /// Return the configured base URL.
    #[allow(dead_code)]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

// ---------------------------------------------------------------------------
// JWT helpers
// ---------------------------------------------------------------------------

/// Extract the `hex_identity` field from a JWT bearer token.
///
/// Splits the token on `.`, base64url-decodes the payload (middle part), and
/// parses the resulting JSON without requiring any external JWT crate.
pub fn extract_identity_from_jwt(token: &str) -> Option<String> {
    let mut parts = token.splitn(3, '.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    let payload_bytes = base64url_decode(payload_b64)?;
    let json: Value = serde_json::from_slice(&payload_bytes).ok()?;

    // Legacy self-hosted SpacetimeDB tokens embed the hex identity directly.
    if let Some(hex) = json.get("hex_identity").and_then(Value::as_str) {
        return Some(hex.to_string());
    }

    // OIDC tokens (e.g. SpacetimeDB maincloud, issued by auth.spacetimedb.com)
    // carry the standard `iss`/`sub` claims instead; the identity is derived
    // from them.
    let issuer = json.get("iss").and_then(Value::as_str)?;
    let subject = json.get("sub").and_then(Value::as_str)?;
    Some(identity_from_claims(issuer, subject))
}

/// Derive a SpacetimeDB identity (64-char hex string) from a token's issuer
/// and subject claims, matching SpacetimeDB's `Identity::from_claims`:
///
/// 1. `id_hash  = blake3("{issuer}|{subject}")[..26]`
/// 2. `checksum = blake3([0xc2, 0x00] ++ id_hash)[..4]`
/// 3. `identity = [0xc2, 0x00] ++ checksum ++ id_hash`  (32 bytes, big-endian)
///
/// The leading `0xc2, 0x00` bytes are why every identity renders with a
/// `c200` prefix.
fn identity_from_claims(issuer: &str, subject: &str) -> String {
    let input = format!("{issuer}|{subject}");
    let id_hash = blake3::hash(input.as_bytes());
    let id_hash = &id_hash.as_bytes()[..26];

    let mut checksum_input = [0u8; 28];
    checksum_input[0] = 0xc2;
    checksum_input[1] = 0x00;
    checksum_input[2..].copy_from_slice(id_hash);
    let checksum = blake3::hash(&checksum_input);

    let mut bytes = [0u8; 32];
    bytes[0] = 0xc2;
    bytes[1] = 0x00;
    bytes[2..6].copy_from_slice(&checksum.as_bytes()[..4]);
    bytes[6..].copy_from_slice(id_hash);

    let mut hex = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Decode a base64url-encoded string (no padding required) into raw bytes.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    // Translate base64url alphabet → standard base64.
    let mut s = input.replace('-', "+").replace('_', "/");
    // Restore padding.
    match s.len() % 4 {
        2 => s.push_str("=="),
        3 => s.push('='),
        _ => {}
    }
    base64_decode(s.as_bytes())
}

/// Minimal, allocation-efficient standard base64 decoder.
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let decode_byte = |c: u8| -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            b'=' => Some(0), // padding — value is discarded
            _ => None,
        }
    };

    let mut result = Vec::with_capacity(input.len() * 3 / 4);
    let mut i = 0;

    while i + 4 <= input.len() {
        let a = decode_byte(input[i])?;
        let b = decode_byte(input[i + 1])?;
        let c = decode_byte(input[i + 2])?;
        let d = decode_byte(input[i + 3])?;

        result.push((a << 2) | (b >> 4));
        if input[i + 2] != b'=' {
            result.push((b << 4) | (c >> 2));
        }
        if input[i + 3] != b'=' {
            result.push((c << 6) | d);
        }
        i += 4;
    }

    Some(result)
}

/// Whether a non-success response means "this database is paused".
///
/// Maincloud suspends inactive databases and then returns
/// `503 database is paused` on *every* endpoint (schema, SQL, even the
/// WebSocket subscribe upgrade). It is server-side state, not a
/// protocol/version mismatch, and the client cannot resume it — the
/// database must be woken from the SpacetimeDB dashboard (or republished).
fn is_paused_response(status: u16, body: &str) -> bool {
    status == 503 && body.to_lowercase().contains("paused")
}

/// A paused database, surfaced as a distinct error type so callers can
/// flag the database in the UI (and clear the flag when it resumes)
/// rather than only showing a transient message. Its `Display` is the
/// user-facing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabasePaused {
    pub database: String,
}

impl std::fmt::Display for DatabasePaused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Database '{}' is paused. SpacetimeDB Maincloud suspends \
             inactive databases; resume it from the dashboard at \
             https://spacetimedb.com (or republish it), then reconnect.",
            self.database
        )
    }
}

impl std::error::Error for DatabasePaused {}

/// Build a human-readable error for a non-success `GET /schema` response.
///
/// Kept as a pure function (status code + body in, message out) so the
/// classification can be unit-tested without standing up an HTTP server.
/// The paused case is handled separately via [`DatabasePaused`].
fn schema_error_message(database: &str, status: u16, body: &str) -> String {
    let body_snip: String = body.chars().take(200).collect();
    match status {
        500 => format!(
            "Schema HTTP 500 for '{database}'. The server could not \
             serialise its schema — usually this means the database \
             was published with a module format this client doesn't \
             understand, or the module crashed on the server side. \
             Server body: {body_snip}"
        ),
        404 => format!(
            "Schema HTTP 404 — database '{database}' does not exist \
             or is not visible to the current identity"
        ),
        _ => format!("Schema HTTP {status}: {body_snip}"),
    }
}

// ---------------------------------------------------------------------------
// Response parsers
// ---------------------------------------------------------------------------

/// Parse the raw SQL response value into a [`QueryResult`].
///
/// Handles two schema formats returned by different SpacetimeDB versions:
/// - v1 (legacy): `"schema": [{"name": "col", "algebraic_type": ...}]`
/// - v9 (current): `"schema": {"elements": [{"name": {"some": "col"}, "algebraic_type": ...}]}`
fn parse_query_result(raw: Value) -> Result<QueryResult> {
    // The server may return either an array of result sets or a single object.
    let obj = match raw {
        Value::Array(mut arr) if !arr.is_empty() => arr.swap_remove(0),
        Value::Object(_) => raw,
        Value::Array(_) => {
            // Empty result set.
            return Ok(QueryResult {
                schema: Vec::new(),
                rows: Vec::new(),
                total_duration_micros: 0,
            });
        }
        Value::Null => {
            // Some mutation responses come back as a bare `null`.
            return Ok(QueryResult {
                schema: Vec::new(),
                rows: Vec::new(),
                total_duration_micros: 0,
            });
        }
        other => bail!("Unexpected SQL response shape: {other}"),
    };

    // schema can be an array (v1) or {"elements": [...]} object (v9).
    // Mutation statements (INSERT / UPDATE / DELETE) don't return a
    // schema at all — fall back to an empty result instead of bailing,
    // so the TUI's write-op pipeline can treat them as a success.
    let Some(schema_val) = obj.get("schema") else {
        let total_duration_micros = obj
            .get("total_duration_micros")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        return Ok(QueryResult {
            schema: Vec::new(),
            rows: Vec::new(),
            total_duration_micros,
        });
    };
    let elements: &[Value] = if let Some(arr) = schema_val.as_array() {
        arr
    } else if let Some(arr) = schema_val.get("elements").and_then(|e| e.as_array()) {
        arr
    } else {
        bail!("SQL response 'schema' has unexpected format: {schema_val}");
    };

    let schema_elements: Vec<SchemaElement> = elements
        .iter()
        .map(|col| {
            // name can be a plain string (v1) or {"some": "col_name"} (v9).
            let name = col
                .get("name")
                .and_then(|n| {
                    if let Some(some_val) = n.get("some") {
                        some_val.as_str().map(str::to_string)
                    } else {
                        n.as_str().map(str::to_string)
                    }
                })
                .unwrap_or_default();
            let algebraic_type = col.get("algebraic_type").cloned().unwrap_or(Value::Null);
            SchemaElement {
                name,
                algebraic_type,
            }
        })
        .collect();

    let rows: Vec<Vec<Value>> = obj
        .get("rows")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|row| row.as_array().cloned().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default();

    let total_duration_micros = obj
        .get("total_duration_micros")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    Ok(QueryResult {
        schema: schema_elements,
        rows,
        total_duration_micros,
    })
}

/// Parse the raw v9 schema response into a [`SchemaResponse`].
///
/// The v9 format stores column definitions in a shared `typespace` rather
/// than inline in each table, so we resolve `product_type_ref` references
/// manually after parsing the table list.
fn parse_schema_response(raw: Value) -> Result<SchemaResponse> {
    // ── Typespace ──────────────────────────────────────────────────────────
    let typespace = raw.get("typespace").cloned().unwrap_or(Value::Null);
    let typespace_types: Vec<Value> = typespace
        .get("types")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    // ── Tables ─────────────────────────────────────────────────────────────
    let tables_raw = raw
        .get("tables")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    let mut tables: Vec<crate::api::types::TableInfo> = Vec::new();
    for t in &tables_raw {
        let table_name = t
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if table_name.is_empty() {
            continue;
        }

        let product_type_ref = t
            .get("product_type_ref")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        // table_type: {"User": []} → "user", {"System": []} → "system"
        let table_type = extract_enum_tag(t.get("table_type"), "user");
        // table_access: {"Public": []} → "public", {"Private": []} → "private"
        let table_access = extract_enum_tag(t.get("table_access"), "public");

        // Resolve columns from typespace via product_type_ref.
        let columns = resolve_columns(&typespace_types, product_type_ref as usize);

        let indexes = t
            .get("indexes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let constraints = t
            .get("constraints")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Primary-key columns come through as `"primary_key": [u16, ...]`
        // in the v9 wire format (see `RawTableDefV9.g.cs`). Empty list
        // for PK-less tables. Tolerate both a bare number and a wrapped
        // object form defensively — future server versions may shift.
        let primary_key_cols: Vec<u16> = t
            .get("primary_key")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|n| n.as_u64().map(|u| u as u16))
                    .collect()
            })
            .unwrap_or_default();

        // Discover autoinc columns via the `sequences` array. Each
        // sequence targets one column (`column` field is the ColId),
        // and any such column is effectively `is_autoinc = true` from
        // the TUI's point of view.
        let autoinc_cols: Vec<u16> = t
            .get("sequences")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("column").and_then(|c| c.as_u64()).map(|u| u as u16))
                    .collect()
            })
            .unwrap_or_default();

        // Back-fill the `is_autoinc` flag on the already-resolved
        // column list.
        let columns: Vec<crate::api::types::ColumnInfo> = columns
            .into_iter()
            .map(|mut c| {
                if autoinc_cols.contains(&(c.col_id as u16)) {
                    c.is_autoinc = true;
                }
                c
            })
            .collect();

        tables.push(crate::api::types::TableInfo {
            table_name,
            product_type_ref,
            table_type,
            table_access,
            columns,
            primary_key_cols,
            indexes,
            constraints,
            is_view: false,
        });
    }

    // ── Views ──────────────────────────────────────────────────────────────
    // Views live in `misc_exports`, not `tables`. Each entry looks like
    // `{"View": {"name": "my_view", "is_public": true, "is_anonymous": false,
    //   "params": {...}, "return_type": {"Array": {"Ref": N}}}}`. The `name`
    // is the SQL-queryable name and `return_type.Array.Ref` indexes the
    // typespace for the view's row type, so columns resolve just like tables.
    let views = raw
        .get("misc_exports")
        .and_then(|m| m.as_array())
        .map(|exports| parse_views(exports, &typespace_types))
        .unwrap_or_default();

    // ── Reducers ───────────────────────────────────────────────────────────
    let reducers_raw = raw
        .get("reducers")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    let reducers: Vec<crate::api::types::ReducerInfo> = reducers_raw
        .iter()
        .filter_map(|r| {
            let name = r.get("name")?.as_str()?.to_string();
            // params can be {"elements": [...]} (v9) or a flat array (legacy).
            let params = extract_params(r.get("params"));
            Some(crate::api::types::ReducerInfo { name, params })
        })
        .collect();

    Ok(SchemaResponse {
        typespace,
        tables,
        views,
        reducers,
    })
}

/// Parse the `View` entries out of a schema's `misc_exports` array into
/// `TableInfo`s with `is_view = true`.
///
/// `misc_exports` may also hold non-view exports in future server versions;
/// anything without a `"View"` key is skipped. A view's columns are resolved
/// from the typespace via `return_type.Array.Ref`, the same mechanism tables
/// use, so the data grid and module inspector can show them identically.
fn parse_views(exports: &[Value], typespace_types: &[Value]) -> Vec<crate::api::types::TableInfo> {
    exports
        .iter()
        .filter_map(|export| {
            let view = export.get("View")?;
            let name = view.get("name").and_then(Value::as_str)?.to_string();

            // return_type is `{"Array": {"Ref": N}}`; N indexes the typespace.
            let type_ref = view
                .get("return_type")
                .and_then(|rt| rt.get("Array"))
                .and_then(|arr| arr.get("Ref"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;

            // Public/private mirrors a table's access so the same glyphs apply.
            let is_public = view
                .get("is_public")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let table_access = if is_public { "public" } else { "private" };

            let columns = resolve_columns(typespace_types, type_ref as usize);

            Some(crate::api::types::TableInfo {
                table_name: name,
                product_type_ref: type_ref,
                table_type: "user".to_string(),
                table_access: table_access.to_string(),
                columns,
                primary_key_cols: Vec::new(),
                indexes: Vec::new(),
                constraints: Vec::new(),
                is_view: true,
            })
        })
        .collect()
}

/// Extract the lowercase string key from a SpacetimeDB enum value.
///
/// Handles `{"User": []}`, `"user"` (plain string), and missing values.
fn extract_enum_tag(val: Option<&Value>, default: &str) -> String {
    match val {
        Some(Value::String(s)) => s.to_lowercase(),
        Some(Value::Object(o)) => o
            .keys()
            .next()
            .map(|k| k.to_lowercase())
            .unwrap_or_else(|| default.to_string()),
        _ => default.to_string(),
    }
}

/// Resolve column definitions for a table by looking up `product_type_ref`
/// in the shared typespace types array.
///
/// Expected typespace entry shape:
/// ```json
/// {"Product": {"elements": [{"name": {"some": "col"}, "algebraic_type": {...}}]}}
/// ```
fn resolve_columns(
    typespace_types: &[Value],
    type_ref: usize,
) -> Vec<crate::api::types::ColumnInfo> {
    let type_val = match typespace_types.get(type_ref) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let elements = type_val
        .get("Product")
        .and_then(|p| p.get("elements"))
        .and_then(|e| e.as_array());

    let Some(elements) = elements else {
        return Vec::new();
    };

    elements
        .iter()
        .enumerate()
        .map(|(i, elem)| {
            // name is {"some": "col_name"} or a plain string.
            let col_name = elem
                .get("name")
                .and_then(|n| {
                    if let Some(some_val) = n.get("some") {
                        some_val.as_str().map(str::to_string)
                    } else {
                        n.as_str().map(str::to_string)
                    }
                })
                .unwrap_or_else(|| format!("col_{i}"));

            let col_type = elem.get("algebraic_type").cloned().unwrap_or(Value::Null);
            crate::api::types::ColumnInfo {
                col_id: i as u32,
                col_name,
                col_type,
                is_autoinc: false,
            }
        })
        .collect()
}

/// Extract reducer parameters from a v9 `params` field.
///
/// v9: `"params": {"elements": [{"name": {"some": "p"}, "algebraic_type": {...}}]}`
/// Legacy: `"params": [{"name": "p", "algebraic_type": {...}}]`
fn extract_params(params_val: Option<&Value>) -> Vec<crate::api::types::ReducerParam> {
    let elements: &[Value] = match params_val {
        Some(Value::Array(arr)) => arr,
        Some(obj @ Value::Object(_)) => match obj.get("elements").and_then(|e| e.as_array()) {
            Some(arr) => arr,
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };

    elements
        .iter()
        .filter_map(|elem| {
            let name = elem
                .get("name")
                .and_then(|n| {
                    if let Some(some_val) = n.get("some") {
                        some_val.as_str().map(str::to_string)
                    } else {
                        n.as_str().map(str::to_string)
                    }
                })
                .unwrap_or_default();
            if name.is_empty() {
                return None;
            }
            let algebraic_type = elem.get("algebraic_type").cloned().unwrap_or(Value::Null);
            Some(crate::api::types::ReducerParam {
                name,
                algebraic_type,
            })
        })
        .collect()
}

/// Parse a newline-delimited JSON log stream into `Vec<LogEntry>`.
fn parse_ndjson_logs(text: &str) -> Result<Vec<LogEntry>> {
    let mut entries = Vec::new();
    for (line_num, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<LogEntry>(trimmed) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                warn!(
                    "Failed to parse log line {}: {} — {:?}",
                    line_num + 1,
                    e,
                    trimmed
                );
            }
        }
    }
    Ok(entries)
}

/// Extract a flat list of database names from a JSON value.
///
/// Handles several shapes returned by different SpacetimeDB versions:
/// - `["name1", "name2"]`
/// - `{"databases": ["name1", ...]}`
/// - `{"databases": [{"database_identity": "...", "database_name": "name"}, ...]}`
///
/// Currently used in unit tests; available for future use in database listing.
#[cfg_attr(not(test), allow(dead_code))]
fn extract_database_names(raw: Value) -> Result<Vec<String>> {
    let arr = match raw {
        Value::Array(a) => a,
        Value::Object(o) => {
            let inner = o
                .get("databases")
                .or_else(|| o.get("names"))
                .cloned()
                .unwrap_or(Value::Null);
            match inner {
                Value::Array(a) => a,
                _ => return Ok(Vec::new()),
            }
        }
        _ => return Ok(Vec::new()),
    };

    let names = arr
        .into_iter()
        .filter_map(|item| match item {
            Value::String(s) => Some(s),
            Value::Object(ref _map) => {
                // Try common field names.
                item.get("database_name")
                    .or_else(|| item.get("name"))
                    .or_else(|| item.get("database_identity"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            }
            _ => None,
        })
        .collect();

    Ok(names)
}

// ---------------------------------------------------------------------------
// Unit tests (no network required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // JWT payloads below are base64url(JSON) with throwaway header/signature.
    // The OIDC payload decodes to:
    //   {"iss":"https://auth.spacetimedb.com","sub":"test-subject-0001"}
    const OIDC_PAYLOAD: &str =
        "eyJpc3MiOiJodHRwczovL2F1dGguc3BhY2V0aW1lZGIuY29tIiwic3ViIjoidGVzdC1zdWJqZWN0LTAwMDEifQ";
    // {"hex_identity":"deadbeef"}
    const LEGACY_PAYLOAD: &str = "eyJoZXhfaWRlbnRpdHkiOiJkZWFkYmVlZiJ9";
    // Golden identity for the OIDC payload. Cross-checked against the real
    // identity that `spacetime login show` reports for a maincloud token, so
    // this pins the exact SpacetimeDB `from_claims` derivation.
    const OIDC_GOLDEN: &str = "c2005855e0ffa65fe854d934cff22cd1c9c60c2070d562b65f07f2c5f20dd8c1";

    #[test]
    fn identity_from_claims_matches_golden() {
        let id = identity_from_claims("https://auth.spacetimedb.com", "test-subject-0001");
        assert_eq!(id, OIDC_GOLDEN);
        // Every SpacetimeDB identity renders with the c200 prefix and is 32
        // bytes (64 hex chars).
        assert!(id.starts_with("c200"));
        assert_eq!(id.len(), 64);
    }

    #[test]
    fn extract_identity_derives_from_oidc_claims() {
        let token = format!("hdr.{OIDC_PAYLOAD}.sig");
        assert_eq!(
            extract_identity_from_jwt(&token).as_deref(),
            Some(OIDC_GOLDEN)
        );
    }

    #[test]
    fn extract_identity_prefers_legacy_hex_identity() {
        // When the legacy claim is present it wins, no derivation needed.
        let token = format!("hdr.{LEGACY_PAYLOAD}.sig");
        assert_eq!(
            extract_identity_from_jwt(&token).as_deref(),
            Some("deadbeef")
        );
    }

    #[test]
    fn extract_identity_none_for_garbage() {
        assert!(extract_identity_from_jwt("not-a-jwt").is_none());
        assert!(extract_identity_from_jwt("hdr.@@@.sig").is_none());
    }

    #[test]
    fn test_parse_query_result_array_wrapper() {
        let raw = json!([{
            "schema": [{"name": "id", "algebraic_type": "U64"}],
            "rows": [[1], [2]],
            "total_duration_micros": 100
        }]);
        let result = parse_query_result(raw).unwrap();
        assert_eq!(result.schema.len(), 1);
        assert_eq!(result.schema[0].name, "id");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.total_duration_micros, 100);
    }

    #[test]
    fn test_parse_query_result_empty_array() {
        let raw = json!([]);
        let result = parse_query_result(raw).unwrap();
        assert_eq!(result.row_count(), 0);
    }

    #[test]
    fn test_parse_query_result_mutation_response_no_schema() {
        // Regression guard: UPDATE / INSERT / DELETE responses don't
        // include a `schema` field. Before this fix, the parser bailed
        // with "SQL response missing 'schema'", which surfaced in the
        // spreadsheet edit mode as a WriteOpError even though the
        // mutation had actually committed on the server side.
        let raw = json!({
            "total_duration_micros": 42
        });
        let result = parse_query_result(raw).expect("mutation response parses");
        assert_eq!(result.schema.len(), 0);
        assert_eq!(result.rows.len(), 0);
        assert_eq!(result.total_duration_micros, 42);
    }

    #[test]
    fn test_parse_query_result_mutation_response_array_wrapper() {
        // Same idea but wrapped in the `[...]` envelope the server
        // sometimes uses for SELECT results. The first element has
        // no schema → still treated as a successful mutation.
        let raw = json!([{
            "total_duration_micros": 17,
            "rows": []
        }]);
        let result = parse_query_result(raw).expect("wrapped mutation parses");
        assert_eq!(result.schema.len(), 0);
        assert_eq!(result.rows.len(), 0);
        assert_eq!(result.total_duration_micros, 17);
    }

    #[test]
    fn test_parse_query_result_bare_null_is_empty() {
        // Tolerate a bare JSON null — observed on some endpoints.
        let raw = json!(null);
        let result = parse_query_result(raw).expect("null parses");
        assert_eq!(result.schema.len(), 0);
        assert_eq!(result.rows.len(), 0);
    }

    #[test]
    fn test_extract_database_names_flat_array() {
        let raw = json!(["alpha", "beta", "gamma"]);
        let names = extract_database_names(raw).unwrap();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn test_extract_database_names_wrapped() {
        let raw = json!({"databases": ["alpha", "beta"]});
        let names = extract_database_names(raw).unwrap();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn test_extract_database_names_object_array() {
        let raw = json!({"databases": [
            {"database_name": "alpha"},
            {"name": "beta"}
        ]});
        let names = extract_database_names(raw).unwrap();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn test_parse_ndjson_logs_valid() {
        let text = r#"{"level":"info","message":"started"}
{"level":"error","message":"boom"}"#;
        let entries = parse_ndjson_logs(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, "started");
        assert_eq!(entries[1].message, "boom");
    }

    #[test]
    fn test_parse_ndjson_logs_skips_bad_lines() {
        let text = "not json\n{\"level\":\"info\",\"message\":\"ok\"}";
        let entries = parse_ndjson_logs(text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "ok");
    }

    // ── SpacetimeDB v9 format tests ─────────────────────────────────────

    #[test]
    fn test_parse_query_result_v9_schema_elements() {
        // v9 format: schema is {"elements": [...]} and names are {"some": "col"}
        let raw = json!([{
            "schema": {
                "elements": [
                    {"name": {"some": "table_id"}, "algebraic_type": {"U32": []}},
                    {"name": {"some": "table_name"}, "algebraic_type": {"String": []}}
                ]
            },
            "rows": [[1, "st_table"], [2, "st_column"]],
            "total_duration_micros": 42,
            "stats": {}
        }]);
        let result = parse_query_result(raw).unwrap();
        assert_eq!(result.schema.len(), 2);
        assert_eq!(result.schema[0].name, "table_id");
        assert_eq!(result.schema[1].name, "table_name");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.total_duration_micros, 42);
    }

    #[test]
    fn test_parse_schema_response_v9() {
        // Real v9 schema format with typespace column resolution
        let raw = json!({
            "typespace": {
                "types": [
                    {
                        "Product": {
                            "elements": [
                                {"name": {"some": "agent_id"}, "algebraic_type": {"U64": []}},
                                {"name": {"some": "session_id"}, "algebraic_type": {"U64": []}}
                            ]
                        }
                    }
                ]
            },
            "tables": [
                {
                    "name": "agent_activity",
                    "product_type_ref": 0,
                    "primary_key": [0],
                    "indexes": [],
                    "constraints": [],
                    "sequences": [],
                    "table_type": {"User": []},
                    "table_access": {"Public": []}
                }
            ],
            "reducers": [
                {
                    "name": "set_status",
                    "params": {
                        "elements": [
                            {"name": {"some": "agent_id"}, "algebraic_type": {"U64": []}}
                        ]
                    }
                }
            ],
            "types": [],
            "misc_exports": []
        });

        let schema = parse_schema_response(raw).unwrap();

        assert_eq!(schema.tables.len(), 1);
        let tbl = &schema.tables[0];
        assert_eq!(tbl.table_name, "agent_activity");
        assert_eq!(tbl.table_type, "user");
        assert_eq!(tbl.table_access, "public");
        assert_eq!(tbl.columns.len(), 2);
        assert_eq!(tbl.columns[0].col_name, "agent_id");
        assert_eq!(tbl.columns[1].col_name, "session_id");

        assert_eq!(schema.reducers.len(), 1);
        assert_eq!(schema.reducers[0].name, "set_status");
        assert_eq!(schema.reducers[0].params.len(), 1);
        assert_eq!(schema.reducers[0].params[0].name, "agent_id");
    }

    #[test]
    fn test_parse_schema_response_extracts_views() {
        // Views arrive in `misc_exports` (not `tables`), each as
        // `{"View": {...}}` with a `return_type.Array.Ref` into the typespace.
        let raw = json!({
            "typespace": {
                "types": [
                    // 0: a real table's row type
                    {"Product": {"elements": [
                        {"name": {"some": "id"}, "algebraic_type": {"U64": []}}
                    ]}},
                    // 1: the view's row type
                    {"Product": {"elements": [
                        {"name": {"some": "board_id"}, "algebraic_type": {"U64": []}},
                        {"name": {"some": "title"}, "algebraic_type": {"String": []}}
                    ]}}
                ]
            },
            "tables": [
                {
                    "name": "board",
                    "product_type_ref": 0,
                    "table_type": {"User": []},
                    "table_access": {"Private": []}
                }
            ],
            "reducers": [],
            "misc_exports": [
                {"View": {
                    "name": "my_boards",
                    "index": 0,
                    "is_public": true,
                    "is_anonymous": false,
                    "params": {"elements": []},
                    "return_type": {"Array": {"Ref": 1}}
                }},
                {"View": {
                    "name": "secret_view",
                    "index": 1,
                    "is_public": false,
                    "is_anonymous": false,
                    "params": {"elements": []},
                    "return_type": {"Array": {"Ref": 1}}
                }}
            ]
        });

        let schema = parse_schema_response(raw).unwrap();

        // Real tables and views are kept in separate lists.
        assert_eq!(schema.tables.len(), 1);
        assert_eq!(schema.views.len(), 2);

        let v0 = &schema.views[0];
        assert_eq!(v0.table_name, "my_boards");
        assert!(v0.is_view);
        assert_eq!(v0.table_access, "public");
        // Columns resolved from the typespace via return_type.Array.Ref.
        assert_eq!(v0.columns.len(), 2);
        assert_eq!(v0.columns[0].col_name, "board_id");
        assert_eq!(v0.columns[1].col_name, "title");

        // is_public:false maps to the private glyph.
        assert_eq!(schema.views[1].table_access, "private");
    }

    #[test]
    fn test_extract_enum_tag_object() {
        assert_eq!(
            extract_enum_tag(Some(&json!({"User": []})), "unknown"),
            "user"
        );
        assert_eq!(
            extract_enum_tag(Some(&json!({"System": []})), "unknown"),
            "system"
        );
        assert_eq!(
            extract_enum_tag(Some(&json!({"Public": []})), "unknown"),
            "public"
        );
        assert_eq!(
            extract_enum_tag(Some(&json!({"Private": []})), "unknown"),
            "private"
        );
    }

    #[test]
    fn test_extract_enum_tag_string_fallback() {
        assert_eq!(extract_enum_tag(Some(&json!("User")), "unknown"), "user");
        assert_eq!(extract_enum_tag(None, "default"), "default");
    }

    #[test]
    fn test_resolve_columns_some_name_pattern() {
        let types = vec![json!({
            "Product": {
                "elements": [
                    {"name": {"some": "id"}, "algebraic_type": {"U64": []}},
                    {"name": {"some": "name"}, "algebraic_type": {"String": []}}
                ]
            }
        })];
        let cols = resolve_columns(&types, 0);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].col_name, "id");
        assert_eq!(cols[0].col_id, 0);
        assert_eq!(cols[1].col_name, "name");
        assert_eq!(cols[1].col_id, 1);
    }

    #[test]
    fn test_resolve_columns_out_of_range() {
        let types: Vec<Value> = vec![];
        let cols = resolve_columns(&types, 99);
        assert!(cols.is_empty());
    }

    // ── Paused-database detection ───────────────────────────────────────

    #[test]
    fn paused_response_detects_503_with_paused_body() {
        // Maincloud returns `503 database is paused` for suspended
        // databases. Casing varies, so match case-insensitively.
        assert!(is_paused_response(503, "database is paused"));
        assert!(is_paused_response(503, "Database Is Paused"));
    }

    #[test]
    fn paused_response_ignores_other_503s_and_codes() {
        // A 503 that isn't a pause (e.g. genuine outage) is not paused.
        assert!(!is_paused_response(503, "upstream timeout"));
        // The same body on a different status is not paused either.
        assert!(!is_paused_response(500, "database is paused"));
    }

    #[test]
    fn database_paused_display_is_actionable() {
        // The typed error's Display is the user-facing message: it must
        // name the database, say it's paused, and say how to resume.
        let msg = DatabasePaused {
            database: "space-dungeon".to_string(),
        }
        .to_string();
        assert!(msg.contains("space-dungeon"));
        assert!(msg.to_lowercase().contains("paused"));
        assert!(msg.contains("dashboard"));
        assert!(!msg.contains("HTTP"));
    }

    // ── Schema error classification ─────────────────────────────────────

    #[test]
    fn schema_error_generic_503_falls_through() {
        // A non-paused 503 keeps the generic message (paused is handled
        // separately via DatabasePaused before this is reached).
        let msg = schema_error_message("db", 503, "upstream timeout");
        assert!(msg.contains("Schema HTTP 503"));
        assert!(msg.contains("upstream timeout"));
    }

    #[test]
    fn schema_error_404_and_500_messages() {
        let m404 = schema_error_message("db", 404, "");
        assert!(m404.contains("404"));
        assert!(m404.contains("does not exist"));

        let m500 = schema_error_message("db", 500, "boom");
        assert!(m500.contains("500"));
        assert!(m500.contains("boom"));
    }

    #[test]
    fn schema_error_body_is_truncated() {
        // Generic branch caps the server body at 200 chars so a huge
        // response can't blow up the error line.
        let long = "x".repeat(500);
        let msg = schema_error_message("db", 502, &long);
        assert!(msg.contains("Schema HTTP 502"));
        assert!(msg.matches('x').count() <= 200);
    }
}
