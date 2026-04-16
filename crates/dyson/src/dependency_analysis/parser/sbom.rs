// SBOM — CycloneDX + SPDX JSON.  PURLs are the canonical OSV query
// shape, so one parser covers every ecosystem for repos that ship an
// SBOM — including Linux distros we otherwise refuse to guess about.

use std::path::Path;

use serde::Deserialize;

use super::{ManifestParser, dep};
use crate::dependency_analysis::types::{Dependency, Ecosystem, ParseError, Parsed};

pub struct SbomParser;

impl ManifestParser for SbomParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let head: SbomHead = serde_json::from_slice(bytes)
            .map_err(|e| ParseError::malformed(path, format!("not valid JSON: {e}")))?;
        if head.bom_format.as_deref() == Some("CycloneDX") || head.components.is_some() {
            parse_cyclonedx(path, bytes)
        } else if head.spdx_version.is_some() {
            parse_spdx(path, bytes)
        } else {
            Err(ParseError::malformed(
                path,
                "JSON file is not a recognised CycloneDX or SPDX document",
            ))
        }
    }
}

#[derive(Deserialize)]
struct SbomHead {
    #[serde(rename = "bomFormat")]
    bom_format: Option<String>,
    #[serde(rename = "spdxVersion")]
    spdx_version: Option<String>,
    components: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CycloneDx {
    #[serde(default)]
    components: Vec<CdxComponent>,
}

#[derive(Deserialize)]
struct CdxComponent {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    purl: Option<String>,
}

fn parse_cyclonedx(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let doc: CycloneDx = serde_json::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("CycloneDX decode: {e}")))?;
    let mut parsed = Parsed::default();
    for c in doc.components {
        push_purl(&mut parsed, path, c.name, c.version, c.purl);
    }
    Ok(parsed)
}

#[derive(Deserialize)]
struct Spdx {
    #[serde(default, rename = "packages")]
    packages: Vec<SpdxPackage>,
}

#[derive(Deserialize)]
struct SpdxPackage {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "versionInfo")]
    version: String,
    #[serde(default, rename = "externalRefs")]
    external_refs: Vec<SpdxExternalRef>,
}

#[derive(Deserialize)]
struct SpdxExternalRef {
    #[serde(default, rename = "referenceType")]
    ty: String,
    #[serde(default, rename = "referenceLocator")]
    locator: String,
}

fn parse_spdx(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let doc: Spdx = serde_json::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("SPDX decode: {e}")))?;
    let mut parsed = Parsed::default();
    for p in doc.packages {
        let purl = p
            .external_refs
            .into_iter()
            .find(|r| r.ty.eq_ignore_ascii_case("purl"))
            .map(|r| r.locator);
        push_purl(&mut parsed, path, p.name, p.version, purl);
    }
    Ok(parsed)
}

fn push_purl(
    parsed: &mut Parsed,
    path: &Path,
    name: String,
    version: String,
    purl: Option<String>,
) {
    let Some(purl) = purl else {
        parsed
            .warnings
            .push(format!("{name}: no PURL; skipping"));
        return;
    };
    let Some((eco, _, _)) = split_purl(&purl) else {
        parsed.warnings.push(format!("unrecognised PURL: {purl}"));
        return;
    };
    let d = Dependency {
        purl: Some(purl),
        ..dep(name, (!version.is_empty()).then_some(version), eco, path)
    };
    parsed.deps.push(d);
}

/// Parse a PURL minimally — `pkg:<type>/<ns-and-name>@<version>[?#]`.
/// We only need `type` (for ecosystem), not the full spec; OSV validates
/// the rest server-side.
fn split_purl(purl: &str) -> Option<(Ecosystem, String, Option<String>)> {
    let rest = purl.strip_prefix("pkg:")?;
    let (ty, after) = rest.split_once('/')?;
    let (name_and_ns, ver) = match after.split_once('@') {
        Some((n, v)) => (n, Some(v.split(['?', '#']).next()?.to_string())),
        None => (after, None),
    };
    let name = name_and_ns.split(['?', '#']).next()?.to_string();
    let eco = Ecosystem::from_purl_type(ty).unwrap_or_else(|| Ecosystem::Other(ty.to_string()));
    Some((eco, name, ver))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_purl_cargo() {
        let (eco, name, ver) = split_purl("pkg:cargo/serde@1.0.0").unwrap();
        assert_eq!(eco, Ecosystem::CratesIo);
        assert_eq!(name, "serde");
        assert_eq!(ver.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn split_purl_with_qualifiers() {
        let (_, _, ver) = split_purl("pkg:deb/debian/curl@7.74.0?distro=bullseye").unwrap();
        assert_eq!(ver.as_deref(), Some("7.74.0"));
    }

    #[test]
    fn unknown_json_rejected() {
        assert!(SbomParser.parse(Path::new("x.json"), b"{}").is_err());
    }

    #[test]
    fn cyclonedx_component_parsed() {
        let doc = br#"{"bomFormat":"CycloneDX","components":[
            {"name":"serde","version":"1.0.0","purl":"pkg:cargo/serde@1.0.0"}
        ]}"#;
        let parsed = SbomParser.parse(Path::new("bom.json"), doc).unwrap();
        assert_eq!(parsed.deps[0].ecosystem, Ecosystem::CratesIo);
    }
}
