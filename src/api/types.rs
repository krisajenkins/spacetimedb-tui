//! SpacetimeDB API response types.
//!
//! All types implement `serde::Deserialize` so they can be decoded directly
//! from the JSON payloads returned by the SpacetimeDB HTTP and WebSocket APIs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// SQL query response
// ---------------------------------------------------------------------------

/// A single column descriptor returned in a SQL query response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchemaElement {
    /// Column name.
    pub name: String,
    /// SpacetimeDB algebraic type tag (e.g. `"String"`, `"U64"`, …).
    pub algebraic_type: Value,
}

/// The full result of a `POST /v1/sql/<database>` request.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueryResult {
    /// Column descriptors, in the same order as each row.
    pub schema: Vec<SchemaElement>,
    /// Data rows.  Each inner `Vec` has one entry per column.
    pub rows: Vec<Vec<Value>>,
    /// Server-side execution time in microseconds.
    #[serde(default)]
    pub total_duration_micros: u64,
}

impl QueryResult {
    /// Returns the number of rows in this result.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Returns the number of columns in this result.
    pub fn column_count(&self) -> usize {
        self.schema.len()
    }

    /// Returns column names as a `Vec<&str>`.
    pub fn column_names(&self) -> Vec<&str> {
        self.schema.iter().map(|s| s.name.as_str()).collect()
    }
}

// ---------------------------------------------------------------------------
// SpacetimeDB special "newtype" types
// ---------------------------------------------------------------------------
//
// SpacetimeDB encodes several primitive-like types (Timestamp, TimeDuration,
// Identity, …) as single-field product "newtypes" tagged by a magic field
// name. In the wire format a column's algebraic type therefore looks like
// `{"Product":{"elements":[{"name":{"some":"__timestamp_micros_since_unix_epoch__"},
// "algebraic_type":{"I64":[]}}]}}`, and the value arrives as a one-element
// array (e.g. a Timestamp is `[1780864718837447]`). Without special-casing,
// the UI shows the raw number or the bare tag `Product`.

/// If `algebraic_type` is a special SpacetimeDB newtype product, return a
/// friendly label (`"Timestamp"`, `"TimeDuration"`, …); otherwise `None`.
pub fn special_type_label(algebraic_type: &Value) -> Option<&'static str> {
    let elements = algebraic_type
        .get("Product")
        .and_then(|p| p.get("elements"))
        .and_then(Value::as_array)?;
    if elements.len() != 1 {
        return None;
    }
    let name = elements[0].get("name")?;
    let field = name
        .get("some")
        .and_then(Value::as_str)
        .or_else(|| name.as_str())?;
    Some(match field {
        "__timestamp_micros_since_unix_epoch__" => "Timestamp",
        "__time_duration_micros__" => "TimeDuration",
        "__identity__" => "Identity",
        "__connection_id__" => "ConnectionId",
        _ => return None,
    })
}

/// Extract the single integer payload from a SpacetimeDB newtype value.
///
/// The SQL/JSON wire format wraps the inner field in a one-element array
/// (e.g. a Timestamp arrives as `[1780864718837447]`), but a bare number or a
/// single-field `{magic: n}` object are accepted too.
pub fn newtype_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Array(a) if a.len() == 1 => a[0].as_i64(),
        Value::Number(_) => value.as_i64(),
        Value::Object(o) if o.len() == 1 => o.values().next().and_then(Value::as_i64),
        _ => None,
    }
}

/// Format a micros-since-Unix-epoch timestamp as a readable UTC string.
pub fn format_timestamp_micros(micros: i64) -> Option<String> {
    let secs = micros.div_euclid(1_000_000);
    let nsecs = (micros.rem_euclid(1_000_000) * 1_000) as u32;
    DateTime::<Utc>::from_timestamp(secs, nsecs)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
}

/// Format a signed micros TimeDuration as a compact `±HhMmS.mmms` string.
pub fn format_duration_micros(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let total_us = micros.unsigned_abs();
    let secs = total_us / 1_000_000;
    let millis = (total_us % 1_000_000) / 1_000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{sign}{h}h{m:02}m{s:02}.{millis:03}s")
    } else if m > 0 {
        format!("{sign}{m}m{s:02}.{millis:03}s")
    } else {
        format!("{sign}{s}.{millis:03}s")
    }
}

/// If `algebraic_type` is a special newtype with a renderable value
/// (Timestamp / TimeDuration), format `value` accordingly. Returns `None` to
/// fall back to the generic JSON renderer.
pub fn format_special_value(value: &Value, algebraic_type: &Value) -> Option<String> {
    match special_type_label(algebraic_type)? {
        "Timestamp" => newtype_i64(value).and_then(format_timestamp_micros),
        "TimeDuration" => newtype_i64(value).map(format_duration_micros),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Schema / catalog types
// ---------------------------------------------------------------------------

/// All system tables in SpacetimeDB 2.0.
pub const SYSTEM_TABLES: &[&str] = &[
    "st_table",
    "st_column",
    "st_index",
    "st_constraint",
    "st_sequence",
    "st_client",
    "st_scheduled",
    "st_module",
    "st_var",
    "st_view",
];

/// A single table entry inside a schema response (SpacetimeDB v9 format).
///
/// Not deserialized directly via serde — constructed manually in
/// `client::parse_schema_response` so that column info can be resolved
/// from the shared typespace.
#[derive(Debug, Clone, Serialize)]
pub struct TableInfo {
    /// Human-readable table name (JSON field: `"name"`).
    pub table_name: String,
    /// Index into `typespace.types` that holds this table's column product type.
    pub product_type_ref: u32,
    /// `"user"` or `"system"` (derived from `{"User":[]}` / `{"System":[]}`).
    pub table_type: String,
    /// `"public"` or `"private"` (derived from `{"Public":[]}` / `{"Private":[]}`).
    pub table_access: String,
    /// Resolved column definitions (populated from typespace after parsing).
    pub columns: Vec<ColumnInfo>,
    /// Primary-key column ids as reported by the server
    /// (JSON field: `"primary_key": [u16, …]`). Empty for tables
    /// without a PK. Populated by `parse_schema_response` from the
    /// raw v9 wire format — not something `resolve_columns` can set.
    #[serde(default)]
    pub primary_key_cols: Vec<u16>,
    /// Raw index definitions.
    #[serde(default)]
    pub indexes: Vec<Value>,
    /// Raw constraint definitions.
    #[serde(default)]
    pub constraints: Vec<Value>,
}

/// Metadata for a single column inside a `TableInfo`.
///
/// Resolved from the typespace `Product.elements` list.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    /// Zero-based column position.
    pub col_id: u32,
    /// Column name (resolved from `{"some": "name"}` pattern).
    pub col_name: String,
    /// Algebraic type as a raw JSON value.
    pub col_type: Value,
    /// Whether a sequence auto-increments this column.
    pub is_autoinc: bool,
}

/// Metadata for a single index inside a `TableInfo`.
///
/// Available for future use in the module inspector's index display.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IndexInfo {
    pub index_id: u32,
    pub index_name: String,
    pub index_type: String,
    /// Column positions covered by this index.
    #[serde(default)]
    pub columns: Vec<u32>,
    #[serde(default)]
    pub is_unique: bool,
}

/// Metadata for a single reducer.
///
/// Not deserialized directly — constructed manually in `parse_schema_response`
/// to handle the v9 `{"elements":[…]}` param format and `{"some":"name"}` names.
#[derive(Debug, Clone, Serialize)]
pub struct ReducerInfo {
    pub name: String,
    pub params: Vec<ReducerParam>,
}

/// A single parameter of a reducer.
#[derive(Debug, Clone, Serialize)]
pub struct ReducerParam {
    pub name: String,
    pub algebraic_type: Value,
}

/// The full schema response for a database.
///
/// Constructed manually by `client::parse_schema_response` from the raw v9
/// JSON to correctly resolve typespace references.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaResponse {
    /// The algebraic type registry shared by all tables/reducers.
    pub typespace: Value,
    /// All tables in the database.
    pub tables: Vec<TableInfo>,
    /// All reducers exposed by the database module.
    pub reducers: Vec<ReducerInfo>,
}

/// A convenience alias used throughout the codebase.
pub type Schema = SchemaResponse;

// ---------------------------------------------------------------------------
// Log entry
// ---------------------------------------------------------------------------

/// Severity level of a log message.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Panic,
    #[serde(other)]
    Unknown,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Panic => "PANIC",
            LogLevel::Unknown => "?",
        };
        write!(f, "{s}")
    }
}

impl LogLevel {
    /// Cycle to the next minimum-level filter, in increasing severity.
    /// Wraps around: Panic → Trace.
    pub fn next_filter(self) -> Self {
        match self {
            LogLevel::Trace => LogLevel::Debug,
            LogLevel::Debug => LogLevel::Info,
            LogLevel::Info => LogLevel::Warn,
            LogLevel::Warn => LogLevel::Error,
            LogLevel::Error => LogLevel::Panic,
            LogLevel::Panic => LogLevel::Trace,
            LogLevel::Unknown => LogLevel::Trace,
        }
    }
}

/// A single log line emitted by a SpacetimeDB module.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogEntry {
    /// When the message was produced (may be absent in older server versions).
    ///
    /// Newer SpacetimeDB builds emit `ts` as a `u64` **microseconds
    /// since the Unix epoch** (e.g. `1775679485454488`), while older
    /// builds emitted RFC 3339 strings. The custom deserializer
    /// below accepts either form so we don't blow up the Logs tab
    /// when the server format shifts under us.
    #[serde(default, deserialize_with = "deserialize_log_timestamp")]
    pub ts: Option<DateTime<Utc>>,
    /// Log level.
    pub level: LogLevel,
    /// The human-readable log message.
    pub message: String,
    /// Target / module path (optional).
    #[serde(default)]
    pub target: Option<String>,
    /// Filename (optional).
    #[serde(default)]
    pub filename: Option<String>,
    /// Line number inside the file (optional).
    #[serde(default)]
    pub line_number: Option<u32>,
}

/// Deserialize a SpacetimeDB log-line timestamp.
///
/// Accepts three on-the-wire forms:
/// - `null` / missing → `None`
/// - `u64` microseconds since epoch → `Some(DateTime)`
/// - RFC 3339 string → `Some(DateTime)` (legacy format)
fn deserialize_log_timestamp<'de, D>(d: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v: Option<Value> = Option::deserialize(d)?;
    let Some(v) = v else {
        return Ok(None);
    };
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => {
            let micros = n
                .as_u64()
                .or_else(|| n.as_i64().map(|i| i.max(0) as u64))
                .ok_or_else(|| D::Error::custom("ts: not an integer"))?;
            let secs = (micros / 1_000_000) as i64;
            let nsecs = ((micros % 1_000_000) * 1_000) as u32;
            DateTime::<Utc>::from_timestamp(secs, nsecs)
                .map(Some)
                .ok_or_else(|| D::Error::custom("ts: microseconds out of range"))
        }
        Value::String(s) => DateTime::parse_from_rfc3339(&s)
            .map(|dt| Some(dt.with_timezone(&Utc)))
            .map_err(|e| D::Error::custom(format!("ts: not RFC 3339: {e}"))),
        other => Err(D::Error::custom(format!(
            "ts: expected null / integer / string, got {other:?}"
        ))),
    }
}

impl LogEntry {
    /// Format the entry as a single display line.
    #[allow(dead_code)]
    pub fn display_line(&self) -> String {
        let ts = self
            .ts
            .map(|t| t.format("%H:%M:%S%.3f").to_string())
            .unwrap_or_else(|| "??:??:??".to_string());
        format!("[{}] {} {}", ts, self.level, self.message)
    }
}

#[cfg(test)]
mod special_type_tests {
    use super::*;
    use serde_json::json;

    fn timestamp_type() -> Value {
        json!({"Product": {"elements": [
            {"name": {"some": "__timestamp_micros_since_unix_epoch__"},
             "algebraic_type": {"I64": []}}
        ]}})
    }

    #[test]
    fn labels_timestamp_and_friends() {
        assert_eq!(special_type_label(&timestamp_type()), Some("Timestamp"));
        let dur = json!({"Product": {"elements": [
            {"name": {"some": "__time_duration_micros__"}, "algebraic_type": {"I64": []}}
        ]}});
        assert_eq!(special_type_label(&dur), Some("TimeDuration"));
        let ident = json!({"Product": {"elements": [
            {"name": {"some": "__identity__"}, "algebraic_type": {"U256": []}}
        ]}});
        assert_eq!(special_type_label(&ident), Some("Identity"));
    }

    #[test]
    fn ignores_plain_and_multi_field_products() {
        assert_eq!(special_type_label(&json!({"U64": []})), None);
        let plain = json!({"Product": {"elements": [
            {"name": {"some": "id"}, "algebraic_type": {"U64": []}},
            {"name": {"some": "name"}, "algebraic_type": {"String": []}}
        ]}});
        assert_eq!(special_type_label(&plain), None);
    }

    #[test]
    fn extracts_newtype_payload_from_array() {
        // The shape the SQL endpoint actually emits.
        assert_eq!(
            newtype_i64(&json!([1780864718837447i64])),
            Some(1780864718837447)
        );
        assert_eq!(newtype_i64(&json!(42)), Some(42));
        assert_eq!(newtype_i64(&json!([1, 2])), None);
    }

    #[test]
    fn formats_timestamp_value_via_column_type() {
        // Real value/type pair pulled from the `booking` table.
        let out = format_special_value(&json!([1780864718837447i64]), &timestamp_type());
        assert_eq!(out.as_deref(), Some("2026-06-07 20:38:38 UTC"));
    }

    #[test]
    fn formats_durations_compactly() {
        assert_eq!(format_duration_micros(1_000_000), "1.000s");
        assert_eq!(format_duration_micros(90_500_000), "1m30.500s");
        assert_eq!(format_duration_micros(3_661_000_000), "1h01m01.000s");
        assert_eq!(format_duration_micros(-2_000_000), "-2.000s");
    }
}

#[cfg(test)]
mod log_entry_tests {
    use super::*;

    #[test]
    fn log_entry_parses_integer_microsecond_ts() {
        // Shape that newer SpacetimeDB builds actually emit —
        // the exact integer from the user-reported regression.
        let json = r#"{
            "level": "Info",
            "ts": 1775679485454488,
            "target": "alice_swarm_stdb",
            "filename": "src/lib.rs",
            "line_number": 179,
            "function": "client_connected",
            "message": "Client connected"
        }"#;
        let entry: LogEntry = serde_json::from_str(json).expect("ts integer parses");
        let ts = entry.ts.expect("ts present");
        // Microseconds → (secs, nanos) round-trip.
        assert_eq!(ts.timestamp(), 1_775_679_485);
        assert_eq!(ts.timestamp_subsec_micros(), 454_488);
    }

    #[test]
    fn log_entry_parses_rfc3339_string_ts() {
        // Legacy RFC 3339 format still works.
        let json = r#"{
            "level": "Warn",
            "ts": "2024-01-02T03:04:05.123Z",
            "message": "hello"
        }"#;
        let entry: LogEntry = serde_json::from_str(json).expect("ts string parses");
        let ts = entry.ts.expect("ts present");
        assert_eq!(ts.timestamp(), 1_704_164_645);
    }

    #[test]
    fn log_entry_tolerates_missing_ts() {
        let json = r#"{ "level": "Info", "message": "no ts" }"#;
        let entry: LogEntry = serde_json::from_str(json).expect("missing ts parses");
        assert!(entry.ts.is_none());
    }

    #[test]
    fn log_entry_tolerates_null_ts() {
        let json = r#"{ "level": "Info", "ts": null, "message": "null ts" }"#;
        let entry: LogEntry = serde_json::from_str(json).expect("null ts parses");
        assert!(entry.ts.is_none());
    }
}

// ---------------------------------------------------------------------------
// WebSocket / subscription message types
// ---------------------------------------------------------------------------

/// The type of a WebSocket message received from SpacetimeDB.
///
/// SpacetimeDB's `v1.json.spacetimedb` subprotocol uses SATS externally
/// tagged enums — i.e. `{"IdentityToken": {...}}` rather than
/// `{"type": "IdentityToken", ...}`. Every field with `#[serde(default)]`
/// is tolerated so that field renames in newer server versions don't break
/// the decoder entirely.
///
/// Messages we don't recognise (e.g. new variants added by a future server
/// version) fail deserialisation and are surfaced as [`super::types::…`]'s
/// `RawText` event by `decode_subscription_frame`, which is a safe no-op
/// for the UI.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum WsServerMessage {
    /// Initial data snapshot after subscribing to a query.
    InitialSubscription(InitialSubscriptionPayload),
    /// Incremental update pushed by the server.
    TransactionUpdate(TransactionUpdatePayload),
    /// Server acknowledges an identity.
    IdentityToken(IdentityTokenPayload),
}

/// Payload of [`WsServerMessage::InitialSubscription`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InitialSubscriptionPayload {
    #[serde(default)]
    pub database_update: DatabaseUpdate,
    #[serde(default)]
    pub request_id: u32,
    /// Server-side execution time. Newer servers use
    /// `total_host_execution_duration` (nanos as i64); older ones used
    /// `total_host_execution_duration_micros`. We don't need the value
    /// directly, so leave it untyped.
    #[serde(default)]
    pub total_host_execution_duration: Option<Value>,
}

/// Payload of [`WsServerMessage::TransactionUpdate`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TransactionUpdatePayload {
    #[serde(default)]
    pub status: Option<TransactionStatus>,
    #[serde(default)]
    pub database_update: DatabaseUpdate,
    /// Other fields (timestamp, caller identity, energy usage, …) are
    /// preserved as raw JSON so future server additions don't break
    /// decoding. The UI only needs `database_update` today.
    #[serde(flatten, default)]
    pub extra: std::collections::HashMap<String, Value>,
}

/// Payload of [`WsServerMessage::IdentityToken`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IdentityTokenPayload {
    #[serde(default)]
    pub identity: Option<Value>,
    #[serde(default)]
    pub token: Option<String>,
    /// Newer SpacetimeDB versions use `connection_id`, some older ones
    /// used `address`. Accept either by flattening the rest of the payload.
    #[serde(flatten, default)]
    pub extra: std::collections::HashMap<String, Value>,
}

/// Status of a committed transaction.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    Committed,
    Failed,
    OutOfEnergy,
}

/// A collection of table row updates within a transaction.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DatabaseUpdate {
    #[serde(default)]
    pub tables: Vec<TableUpdate>,
}

/// Row-level inserts/deletes for a single table.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TableUpdate {
    pub table_id: u32,
    pub table_name: String,
    #[serde(default)]
    pub num_rows: u64,
    #[serde(default)]
    pub inserts: Vec<Value>,
    #[serde(default)]
    pub deletes: Vec<Value>,
}
