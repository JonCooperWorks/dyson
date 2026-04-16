// GitHub Actions — extract `uses: owner/repo@ref` from job steps and
// reusable-workflow calls.  Local (`./…`) and docker images are skipped.

use std::path::Path;

use serde::Deserialize;

use super::{ManifestParser, dep};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct GithubActionsParser;

#[derive(Deserialize)]
struct Workflow {
    #[serde(default)]
    jobs: std::collections::BTreeMap<String, Job>,
}

#[derive(Deserialize)]
struct Job {
    #[serde(default)]
    steps: Vec<Step>,
    #[serde(default)]
    uses: Option<String>,
}

#[derive(Deserialize)]
struct Step {
    #[serde(default)]
    uses: Option<String>,
}

impl ManifestParser for GithubActionsParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let doc: Workflow = serde_yaml_ng::from_slice(bytes)
            .map_err(|e| ParseError::malformed(path, format!("workflow decode: {e}")))?;
        let mut parsed = Parsed::default();
        for (_jobname, job) in doc.jobs {
            if let Some(uses) = &job.uses {
                push_uses(&mut parsed, path, uses);
            }
            for step in job.steps {
                if let Some(uses) = step.uses {
                    push_uses(&mut parsed, path, &uses);
                }
            }
        }
        Ok(parsed)
    }
}

fn push_uses(parsed: &mut Parsed, path: &Path, uses: &str) {
    if let Some((name, version)) = split_uses(uses) {
        let mut d = dep(name, Some(version), Ecosystem::GitHubActions, path);
        d.direct = true;
        parsed.deps.push(d);
    }
}

fn split_uses(uses: &str) -> Option<(String, String)> {
    let uses = uses.trim();
    if uses.starts_with("./") || uses.starts_with("docker://") {
        return None;
    }
    let (name, version) = uses.split_once('@')?;
    // owner/repo/path@ref → owner/repo (OSV tracks the repo as a whole).
    let repo = name.split('/').take(2).collect::<Vec<_>>().join("/");
    Some((repo, version.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_step_uses_skipping_local() {
        let doc = br#"
jobs:
  build:
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v3
      - uses: ./local-action
"#;
        let parsed = GithubActionsParser
            .parse(Path::new(".github/workflows/ci.yml"), doc)
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(parsed.deps.len(), 2);
        assert!(names.contains(&"actions/checkout"));
    }

    #[test]
    fn parses_reusable_workflow() {
        let doc = br#"
jobs:
  call:
    uses: myorg/reusable/.github/workflows/build.yml@v1
"#;
        let parsed = GithubActionsParser
            .parse(Path::new(".github/workflows/ci.yml"), doc)
            .unwrap();
        assert_eq!(parsed.deps[0].name, "myorg/reusable");
    }
}
