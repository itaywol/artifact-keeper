//! PyPI upstream metadata: extract per-version publish times for the min-age
//! curation gate.
//!
//! Source is the PEP 691 JSON simple index (`Accept:
//! application/vnd.pypi.simple.v1+json`) that the proxy already fetches for the
//! `/simple/<name>/` listing. Each file object carries an `upload-time`; a
//! version's publish time is the *earliest* upload-time across its files (when
//! the version first appeared upstream — where the cooldown clock starts).
//!
//! Parsing is pure and unit-tested; the HTTP fetch + caching live on the proxy
//! path (M6).

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::formats::pypi::PypiHandler;

/// Parse a PEP 691 simple JSON body into `version -> earliest upload time`.
///
/// Files without a parseable version or without an `upload-time` are skipped.
/// A version absent from the result means its publish time is unknown upstream
/// (the gate treats that as `Unavailable` → fail mode applies).
pub fn parse_upload_times(body: &str) -> HashMap<String, DateTime<Utc>> {
    let mut out: HashMap<String, DateTime<Utc>> = HashMap::new();

    let json: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return out,
    };
    let files = match json.get("files").and_then(|f| f.as_array()) {
        Some(f) => f,
        None => return out,
    };

    for file in files {
        let filename = match file.get("filename").and_then(|n| n.as_str()) {
            Some(n) => n,
            None => continue,
        };
        let upload_time = match file.get("upload-time").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
        };
        let ts = match DateTime::parse_from_rfc3339(upload_time) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        let version = match PypiHandler::parse_filename(filename) {
            Ok(info) => match info.version {
                Some(v) => v,
                None => continue,
            },
            Err(_) => continue,
        };

        out.entry(version)
            .and_modify(|cur| {
                if ts < *cur {
                    *cur = ts;
                }
            })
            .or_insert(ts);
    }

    out
}

/// Age in days of an upload relative to `now`. Negative clamps to 0.
pub fn age_days(upload_time: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    let secs = (now - upload_time).num_seconds();
    (secs as f64 / 86_400.0).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{
      "meta": {"api-version": "1.0"},
      "name": "sampleproj",
      "files": [
        {"filename": "sampleproj-1.0-py3-none-any.whl", "upload-time": "2023-01-10T12:00:00Z"},
        {"filename": "sampleproj-1.0.tar.gz",           "upload-time": "2023-01-09T08:00:00Z"},
        {"filename": "sampleproj-2.0-py3-none-any.whl", "upload-time": "2023-06-01T00:00:00Z"}
      ]
    }"#;

    #[test]
    fn earliest_upload_time_per_version() {
        let m = parse_upload_times(BODY);
        // 1.0 has two files; earliest (sdist, Jan 9) wins.
        assert_eq!(
            m.get("1.0").unwrap().to_rfc3339(),
            "2023-01-09T08:00:00+00:00"
        );
        assert_eq!(
            m.get("2.0").unwrap().to_rfc3339(),
            "2023-06-01T00:00:00+00:00"
        );
    }

    #[test]
    fn files_without_upload_time_are_skipped() {
        let body = r#"{"files":[{"filename":"foo-1.0-py3-none-any.whl"}]}"#;
        assert!(parse_upload_times(body).is_empty());
    }

    #[test]
    fn unparseable_filename_skipped() {
        let body = r#"{"files":[{"filename":"garbage","upload-time":"2023-01-01T00:00:00Z"}]}"#;
        assert!(parse_upload_times(body).is_empty());
    }

    #[test]
    fn malformed_json_returns_empty() {
        assert!(parse_upload_times("not json").is_empty());
        assert!(parse_upload_times("{}").is_empty());
    }

    #[test]
    fn bad_timestamp_skipped() {
        let body =
            r#"{"files":[{"filename":"foo-1.0-py3-none-any.whl","upload-time":"yesterday"}]}"#;
        assert!(parse_upload_times(body).is_empty());
    }

    #[test]
    fn age_days_computes_and_clamps() {
        let up = DateTime::parse_from_rfc3339("2023-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let now = DateTime::parse_from_rfc3339("2023-01-11T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(age_days(up, now), 10.0);
        // Upload in the future clamps to 0.
        let future = DateTime::parse_from_rfc3339("2023-02-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(age_days(future, now), 0.0);
    }
}
