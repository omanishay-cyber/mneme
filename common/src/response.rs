use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::DbError;
use crate::layer::DbLayer;

/// Uniform envelope returned by every mneme storage operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<DbErrorWire>,
    pub meta: ResponseMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMeta {
    pub latency_ms: u64,
    pub cache_hit: bool,
    pub source_db: DbLayer,
    pub query_id: Uuid,
    pub schema_version: u32,
}

/// Wire-friendly error representation (DbError is not Serialize-friendly
/// because of the `From<rusqlite::Error>` impl carrying live data).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbErrorWire {
    pub kind: String,
    pub message: String,
    pub detail: Option<String>,
}

impl<T> Response<T> {
    pub fn ok(data: T, meta: ResponseMeta) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            meta,
        }
    }

    pub fn err(e: DbError, meta: ResponseMeta) -> Self {
        let wire = DbErrorWire {
            kind: variant_name(&e).to_string(),
            message: e.to_string(),
            // BUG-A2-041 fix: populate `detail` with variant-specific
            // structured info instead of always `None`. Pre-fix, callers
            // saw `null` for every error and had to text-parse `message`
            // to recover holder/since/expected/etc.
            detail: variant_detail(&e),
        };
        Self {
            success: false,
            data: None,
            error: Some(wire),
            meta,
        }
    }
}

fn variant_name(e: &DbError) -> &'static str {
    match e {
        DbError::NotFound => "not_found",
        DbError::Corrupted { .. } => "corrupted",
        DbError::Locked { .. } => "locked",
        DbError::Timeout { .. } => "timeout",
        DbError::SchemaMismatch { .. } => "schema_mismatch",
        DbError::SerializationFailure => "serialization_failure",
        DbError::DiskFull { .. } => "disk_full",
        DbError::PermissionDenied => "permission_denied",
        DbError::InternalPanic { .. } => "internal_panic",
        DbError::Sqlite(_) => "sqlite",
    }
}

/// BUG-A2-041 helper: render a JSON-style structured detail for the
/// variants that carry meaningful payload. Returns `None` for variants
/// whose name+message are already complete.
fn variant_detail(e: &DbError) -> Option<String> {
    match e {
        DbError::Corrupted { detail } => Some(format!("{{\"detail\":{}}}", json_str(detail))),
        DbError::Locked { holder, since } => Some(format!(
            "{{\"holder\":{},\"since\":{}}}",
            json_str(holder),
            json_str(&since.to_string())
        )),
        DbError::Timeout { elapsed_ms } => {
            Some(format!("{{\"elapsed_ms\":{}}}", elapsed_ms))
        }
        DbError::SchemaMismatch { expected, found } => Some(format!(
            "{{\"expected\":{},\"found\":{}}}",
            expected, found
        )),
        DbError::DiskFull { available_bytes } => {
            Some(format!("{{\"available_bytes\":{}}}", available_bytes))
        }
        DbError::InternalPanic { backtrace } => {
            Some(format!("{{\"backtrace\":{}}}", json_str(backtrace)))
        }
        DbError::Sqlite(s) => Some(format!("{{\"sqlite\":{}}}", json_str(s))),
        DbError::NotFound
        | DbError::SerializationFailure
        | DbError::PermissionDenied => None,
    }
}

/// Tiny JSON string escaper covering the four characters that matter for
/// the variant_detail emitter. Avoids pulling serde_json::to_string just
/// for this helper.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
