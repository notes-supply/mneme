//! One JSON object per line on standard error. Never log a token or page text.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// Writes `event` with `fields` and the time, in milliseconds since the Unix epoch.
pub fn emit(event: &str, fields: Value) {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let mut line = Map::new();
    line.insert("time".to_owned(), json!(millis));
    line.insert("event".to_owned(), json!(event));
    if let Value::Object(fields) = fields {
        line.extend(fields);
    }
    eprintln!("{}", Value::Object(line));
}
