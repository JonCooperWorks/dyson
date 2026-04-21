// ===========================================================================
// Embedded prototype assets — bundled into the dyson binary.
//
// The HTTP controller serves the web UI from these embedded bytes by
// default, so `dyson listen` works from anywhere on disk.  A controller
// config of `"webroot": "..."` overrides this with a disk path (handy
// for editing the prototype without recompiling — point it at
// `crates/dyson/src/controller/http/web`).
//
// Paths are relative to this source file:
//   src/controller/http/assets.rs   →  web/...
// ===========================================================================

/// `(url path, content bytes, content-type)` for every file the prototype
/// loads.  Looked up by path on every request (~14 entries — linear scan
/// is faster than a HashMap at this size).
pub const ASSETS: &[(&str, &[u8], &str)] = &[
    (
        "prototype.html",
        include_bytes!("web/prototype.html"),
        "text/html; charset=utf-8",
    ),
    (
        "styles/tokens.css",
        include_bytes!("web/styles/tokens.css"),
        "text/css; charset=utf-8",
    ),
    (
        "styles/layout.css",
        include_bytes!("web/styles/layout.css"),
        "text/css; charset=utf-8",
    ),
    (
        "styles/turns.css",
        include_bytes!("web/styles/turns.css"),
        "text/css; charset=utf-8",
    ),
    (
        "styles/panels.css",
        include_bytes!("web/styles/panels.css"),
        "text/css; charset=utf-8",
    ),
    (
        "js/data.js",
        include_bytes!("web/js/data.js"),
        "application/javascript; charset=utf-8",
    ),
    (
        "js/bridge.js",
        include_bytes!("web/js/bridge.js"),
        "application/javascript; charset=utf-8",
    ),
    (
        "components/icons.jsx",
        include_bytes!("web/components/icons.jsx"),
        "text/babel; charset=utf-8",
    ),
    (
        "components/panels.jsx",
        include_bytes!("web/components/panels.jsx"),
        "text/babel; charset=utf-8",
    ),
    (
        "components/turns.jsx",
        include_bytes!("web/components/turns.jsx"),
        "text/babel; charset=utf-8",
    ),
    (
        "components/views.jsx",
        include_bytes!("web/components/views.jsx"),
        "text/babel; charset=utf-8",
    ),
    (
        "components/app.jsx",
        include_bytes!("web/components/app.jsx"),
        "text/babel; charset=utf-8",
    ),
];

/// Look up an embedded asset by URL path.  Returns `(bytes, content-type)`
/// or `None`.
pub fn lookup(path: &str) -> Option<(&'static [u8], &'static str)> {
    let key = if path == "/" { "prototype.html" } else { path.trim_start_matches('/') };
    for (p, bytes, ct) in ASSETS {
        if *p == key {
            return Some((bytes, ct));
        }
    }
    None
}
