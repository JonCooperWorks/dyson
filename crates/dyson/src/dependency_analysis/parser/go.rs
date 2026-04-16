// Go — go.sum (preferred) + go.mod.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct GoParser;

static GO_SUM_LINE: OnceLock<Regex> = OnceLock::new();
static GO_MOD_REQUIRE: OnceLock<Regex> = OnceLock::new();

impl ManifestParser for GoParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let text = utf8(path, bytes)?;
        if path.file_name().is_some_and(|n| n == "go.sum") {
            parse_go_sum(path, text)
        } else {
            parse_go_mod(path, text)
        }
    }
}

fn parse_go_sum(path: &Path, text: &str) -> Result<Parsed, ParseError> {
    let re = GO_SUM_LINE.get_or_init(|| Regex::new(r"^(\S+)\s+(\S+?)(?:/go\.mod)?\s+h1:").unwrap());
    let mut parsed = Parsed::default();
    // Dedupe: go.sum has both the module hash and the go.mod hash.
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        if let Some(c) = re.captures(line) {
            let name = c[1].to_string();
            let version = c[2].to_string();
            if seen.insert((name.clone(), version.clone())) {
                parsed
                    .deps
                    .push(dep(name, Some(version), Ecosystem::Go, path));
            }
        }
    }
    Ok(parsed)
}

fn parse_go_mod(path: &Path, text: &str) -> Result<Parsed, ParseError> {
    let re = GO_MOD_REQUIRE.get_or_init(|| Regex::new(r"^\s*(\S+)\s+(v\S+)").unwrap());
    let mut parsed = Parsed::default();
    let mut in_require = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("require (") {
            in_require = true;
            continue;
        }
        if in_require && t == ")" {
            in_require = false;
            continue;
        }
        let probe = if let Some(rest) = t.strip_prefix("require ") {
            rest
        } else if in_require {
            t
        } else {
            continue;
        };
        if let Some(c) = re.captures(probe) {
            let mut d = dep(&c[1], Some(c[2].to_string()), Ecosystem::Go, path);
            d.direct = true;
            parsed.deps.push(d);
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_sum_dedupes_gomod_hash() {
        let text = "\
github.com/pkg/errors v0.9.1 h1:AAAA...
github.com/pkg/errors v0.9.1/go.mod h1:BBBB...
golang.org/x/sys v0.0.0-20220715151400-c0bba94af5f8 h1:CCCC...
";
        let parsed = GoParser
            .parse(Path::new("go.sum"), text.as_bytes())
            .unwrap();
        assert_eq!(parsed.deps.len(), 2);
    }

    #[test]
    fn go_mod_require_block() {
        let text = "\
module example.com/me
require (
    github.com/pkg/errors v0.9.1
    golang.org/x/sys v0.10.0
)
require github.com/spf13/cobra v1.7.0
";
        let parsed = GoParser
            .parse(Path::new("go.mod"), text.as_bytes())
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"github.com/pkg/errors"));
        assert!(names.contains(&"github.com/spf13/cobra"));
    }
}
