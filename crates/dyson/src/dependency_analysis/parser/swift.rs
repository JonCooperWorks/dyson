// SwiftPM — Package.resolved (v1/v2/v3).

use std::path::Path;

use serde::Deserialize;

use super::{ManifestParser, dep};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct SwiftParser;

#[derive(Deserialize)]
struct Resolved {
    #[serde(default)]
    pins: Vec<Pin>,
    #[serde(default)]
    object: Option<ResolvedV1>,
}

#[derive(Deserialize)]
struct ResolvedV1 {
    #[serde(default)]
    pins: Vec<Pin>,
}

#[derive(Deserialize)]
struct Pin {
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default, rename = "repositoryURL")]
    repository_url: Option<String>,
    #[serde(default)]
    state: Option<PinState>,
}

#[derive(Deserialize)]
struct PinState {
    #[serde(default)]
    version: Option<String>,
}

impl ManifestParser for SwiftParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let doc: Resolved = serde_json::from_slice(bytes)
            .map_err(|e| ParseError::malformed(path, format!("Package.resolved decode: {e}")))?;
        let pins = if !doc.pins.is_empty() {
            doc.pins
        } else {
            doc.object.map(|o| o.pins).unwrap_or_default()
        };
        let mut parsed = Parsed::default();
        for pin in pins {
            let name = pin
                .identity
                .or(pin.location)
                .or(pin.repository_url)
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let version = pin.state.and_then(|s| s.version);
            parsed
                .deps
                .push(dep(name, version, Ecosystem::SwiftURL, path));
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v1_and_v2() {
        let v2 = br#"{"pins":[{"identity":"swift-log","state":{"version":"1.5.3"}}],"version":2}"#;
        assert_eq!(
            SwiftParser
                .parse(Path::new("Package.resolved"), v2)
                .unwrap()
                .deps[0]
                .name,
            "swift-log"
        );
        let v1 = br#"{"object":{"pins":[{"identity":"alamofire","state":{"version":"5.7.0"}}]},"version":1}"#;
        assert_eq!(
            SwiftParser
                .parse(Path::new("Package.resolved"), v1)
                .unwrap()
                .deps
                .len(),
            1
        );
    }
}
