//! Static asset bundle for `wyd web`. The dashboard HTML and JS are embedded
//! as `&'static [u8]` so the binary does not depend on any filesystem layout
//! at runtime — nothing on disk is exposed.

pub const INDEX_HTML: &[u8] = include_bytes!("../../web/index.html");

pub fn lookup(path: &str) -> Option<(&'static str, &'static [u8])> {
    match path {
        "/assets/app.js" => Some((
            "application/javascript; charset=utf-8",
            include_bytes!("../../web/app.js"),
        )),
        "/assets/styles.css" => Some((
            "text/css; charset=utf-8",
            include_bytes!("../../web/styles.css"),
        )),
        _ => None,
    }
}
