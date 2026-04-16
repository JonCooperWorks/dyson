// RubyGems — Gemfile.lock.  Extract `  name (version)` lines under `specs:`.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct RubyGemsParser;

static SPEC_LINE: OnceLock<Regex> = OnceLock::new();

impl ManifestParser for RubyGemsParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let text = utf8(path, bytes)?;
        let re = SPEC_LINE
            .get_or_init(|| Regex::new(r"^\s{4}([A-Za-z0-9_\-]+)\s+\(([^\)]+)\)").unwrap());
        let mut parsed = Parsed::default();
        let mut in_specs = false;
        for line in text.lines() {
            let trimmed = line.trim_end();
            if trimmed == "  specs:" {
                in_specs = true;
                continue;
            }
            if trimmed.is_empty() {
                in_specs = false;
                continue;
            }
            if !in_specs {
                continue;
            }
            if let Some(c) = re.captures(line) {
                parsed.deps.push(dep(
                    &c[1],
                    Some(c[2].to_string()),
                    Ecosystem::RubyGems,
                    path,
                ));
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gemfile_lock() {
        let text = "\
GEM
  specs:
    rails (7.1.0)
      actionpack (= 7.1.0)
    actionpack (7.1.0)

PLATFORMS
  ruby
";
        let parsed = RubyGemsParser
            .parse(Path::new("Gemfile.lock"), text.as_bytes())
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"rails"));
        assert!(names.contains(&"actionpack"));
    }
}
