// ===========================================================================
// Embedded frontend bundle.
//
// The HTTP controller serves the React UI from these bytes by default, so
// `dyson listen` works from anywhere on disk.
//
// `crates/dyson/build.rs` walks `web/dist/` after `npm run build`, embeds one
// entry per file, and emits a match-based lookup function.  Hashed asset
// filenames from Vite land here verbatim; `index.html` at the root of the
// bundle is what `/` serves.
// ===========================================================================

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// Look up an embedded asset by URL path.  Returns `(bytes, content-type)`
/// or `None`.
pub fn lookup(path: &str) -> Option<(&'static [u8], &'static str)> {
    let key = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    lookup_asset(key)
}

#[cfg(test)]
mod tests {
    use super::lookup;

    #[test]
    fn root_normalizes_to_index_html() {
        let root = lookup("/").expect("root asset");
        let index = lookup("index.html").expect("index asset");
        assert_eq!(root.0.as_ptr(), index.0.as_ptr());
        assert_eq!(root.1, index.1);
    }

    #[test]
    fn missing_asset_returns_none() {
        assert!(lookup("/does-not-exist").is_none());
    }
}
