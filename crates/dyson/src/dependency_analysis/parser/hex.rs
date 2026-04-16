// Hex (Elixir / Erlang) — mix.lock.  Extract `{:hex, :name, "version"…}`.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct HexParser;

static HEX_LINE: OnceLock<Regex> = OnceLock::new();

impl ManifestParser for HexParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let text = utf8(path, bytes)?;
        let re = HEX_LINE
            .get_or_init(|| Regex::new(r#"\{:hex,\s*:([A-Za-z0-9_]+),\s*"([^"]+)""#).unwrap());
        let mut parsed = Parsed::default();
        for c in re.captures_iter(text) {
            parsed
                .deps
                .push(dep(&c[1], Some(c[2].to_string()), Ecosystem::Hex, path));
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mix_lock() {
        let text = r#"%{
  "phoenix": {:hex, :phoenix, "1.7.10", "h", [:mix], [], "", [], ""},
  "telemetry": {:hex, :telemetry, "1.2.1", "h", [:rebar3], [], "", ""}
}"#;
        let parsed = HexParser
            .parse(Path::new("mix.lock"), text.as_bytes())
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"phoenix"));
        assert!(names.contains(&"telemetry"));
    }
}
