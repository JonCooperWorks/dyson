// Maven — pom.xml (regex over <dependency> blocks) + gradle.lockfile
// (line-based `group:artifact:version=…`).  Property substitution
// (`${foo.version}`, `$(…)`) and <dependencyManagement> resolution are
// intentionally skipped with a warning — use gradle.lockfile or an SBOM
// for accurate versions.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct MavenParser;

impl ManifestParser for MavenParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let text = utf8(path, bytes)?;
        if path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("gradle.lockfile"))
        {
            parse_gradle_lock(path, text)
        } else {
            parse_pom(path, text)
        }
    }
}

static DEPENDENCY_BLOCK: OnceLock<Regex> = OnceLock::new();
static GROUP_TAG: OnceLock<Regex> = OnceLock::new();
static ARTIFACT_TAG: OnceLock<Regex> = OnceLock::new();
static VERSION_TAG: OnceLock<Regex> = OnceLock::new();

fn extract_tag(re: &Regex, body: &str) -> Option<String> {
    re.captures(body)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
}

fn parse_pom(path: &Path, text: &str) -> Result<Parsed, ParseError> {
    let block = DEPENDENCY_BLOCK
        .get_or_init(|| Regex::new(r"(?s)<dependency\b[^>]*>(.*?)</dependency>").unwrap());
    // Separate regexes per tag: the `regex` crate doesn't support backrefs.
    let group_re =
        GROUP_TAG.get_or_init(|| Regex::new(r"(?s)<groupId\b[^>]*>(.*?)</groupId>").unwrap());
    let artifact_re = ARTIFACT_TAG
        .get_or_init(|| Regex::new(r"(?s)<artifactId\b[^>]*>(.*?)</artifactId>").unwrap());
    let version_re =
        VERSION_TAG.get_or_init(|| Regex::new(r"(?s)<version\b[^>]*>(.*?)</version>").unwrap());

    let mut parsed = Parsed::default();
    let mut has_placeholder = false;
    for cap in block.captures_iter(text) {
        let body = &cap[1];
        let (Some(g), Some(a)) = (extract_tag(group_re, body), extract_tag(artifact_re, body))
        else {
            continue;
        };
        let mut version = extract_tag(version_re, body);
        if version.as_deref().is_some_and(|v| v.contains("${")) {
            has_placeholder = true;
            version = None;
        }
        let mut d = dep(format!("{g}:{a}"), version, Ecosystem::Maven, path);
        d.direct = true;
        parsed.deps.push(d);
    }
    if has_placeholder {
        parsed.warnings.push(format!(
            "{}: skipped property-substituted versions; prefer gradle.lockfile or SBOM",
            path.display()
        ));
    }
    Ok(parsed)
}

fn parse_gradle_lock(path: &Path, text: &str) -> Result<Parsed, ParseError> {
    let mut parsed = Parsed::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let left = line.split('=').next().unwrap_or(line);
        let parts: Vec<&str> = left.splitn(3, ':').collect();
        if parts.len() == 3 {
            parsed.deps.push(dep(
                format!("{}:{}", parts[0], parts[1]),
                Some(parts[2].to_string()),
                Ecosystem::Maven,
                path,
            ));
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pom_with_concrete_versions() {
        let text = r#"<project><dependencies>
<dependency>
  <groupId>com.fasterxml.jackson.core</groupId>
  <artifactId>jackson-databind</artifactId>
  <version>2.13.0</version>
</dependency>
</dependencies></project>"#;
        let parsed = MavenParser
            .parse(Path::new("pom.xml"), text.as_bytes())
            .unwrap();
        assert_eq!(parsed.deps.len(), 1);
        assert_eq!(
            parsed.deps[0].name,
            "com.fasterxml.jackson.core:jackson-databind"
        );
        assert_eq!(parsed.deps[0].version.as_deref(), Some("2.13.0"));
    }

    #[test]
    fn pom_flags_property_substitution() {
        let text = r#"<project><dependencies>
<dependency>
  <groupId>org.example</groupId>
  <artifactId>foo</artifactId>
  <version>${foo.version}</version>
</dependency>
</dependencies></project>"#;
        let parsed = MavenParser
            .parse(Path::new("pom.xml"), text.as_bytes())
            .unwrap();
        assert!(!parsed.warnings.is_empty());
        assert!(parsed.deps[0].version.is_none());
    }

    #[test]
    fn parses_gradle_lockfile() {
        let text = "# gradle-generated\ncom.fasterxml.jackson.core:jackson-databind:2.13.0=compileClasspath\n";
        let parsed = MavenParser
            .parse(Path::new("gradle.lockfile"), text.as_bytes())
            .unwrap();
        assert_eq!(parsed.deps[0].version.as_deref(), Some("2.13.0"));
    }
}
