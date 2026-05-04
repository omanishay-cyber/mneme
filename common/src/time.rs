use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub DateTime<Utc>);

impl Timestamp {
    pub fn now() -> Self {
        Timestamp(Utc::now())
    }

    pub fn as_unix_millis(&self) -> i64 {
        self.0.timestamp_millis()
    }

    /// BUG-A2-040 fix: log a warn when the input is invalid before
    /// silently substituting "now". Callers who care about the
    /// difference should use [`Self::try_from_unix_millis`] which
    /// returns `Option<Self>`.
    pub fn from_unix_millis(ms: i64) -> Self {
        match DateTime::from_timestamp_millis(ms) {
            Some(dt) => Timestamp(dt),
            None => {
                tracing::warn!(
                    ms,
                    "Timestamp::from_unix_millis received out-of-range millis; substituting now"
                );
                Timestamp(Utc::now())
            }
        }
    }

    /// Fallible variant of [`Self::from_unix_millis`]. Returns `None`
    /// for out-of-range input instead of substituting "now".
    pub fn try_from_unix_millis(ms: i64) -> Option<Self> {
        DateTime::from_timestamp_millis(ms).map(Timestamp)
    }

    /// Render as a directory-name-safe string: `2026-04-23-14-30-00`.
    pub fn as_dirname(&self) -> String {
        self.0.format("%Y-%m-%d-%H-%M-%S").to_string()
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.to_rfc3339())
    }
}

impl Default for Timestamp {
    fn default() -> Self {
        Self::now()
    }
}
