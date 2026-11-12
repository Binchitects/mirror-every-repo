//! The web panel: three static files compiled into the binary. It loads
//! nothing from anywhere else, so it works inside the air gap.
use crate::Reply;

const INDEX: &str = include_str!("ui/index.html");
const APP_JS: &str = include_str!("ui/app.js");
const APP_CSS: &str = include_str!("ui/app.css");

pub(crate) fn serve(path: &str) -> Reply {
    let (content_type, body) = match path {
        "" | "index.html" => ("text/html; charset=utf-8", INDEX),
        "app.js" => ("text/javascript; charset=utf-8", APP_JS),
        "app.css" => ("text/css; charset=utf-8", APP_CSS),
        _ => return Reply::text(404, "not found\n"),
    };
    Reply::bytes(200, content_type, body.as_bytes().to_vec())
        // Scripts and styles only from this server, no inline script, no
        // framing by other sites.
        .with("Content-Security-Policy", "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")
        .with("X-Frame-Options", "DENY")
        .with("Referrer-Policy", "no-referrer")
        .with("Cache-Control", "no-cache")
}
