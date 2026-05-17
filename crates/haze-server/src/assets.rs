use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

// Path is resolved relative to CARGO_MANIFEST_DIR (this crate's directory).
#[derive(RustEmbed)]
#[folder = "../../frontend/build"]
struct Assets;

pub async fn handler(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    let accept = req
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Some(resp) = serve(path, accept) {
        return resp;
    }

    // SPA fallback: non-asset GETs that accept HTML get index.html so client routing works.
    if accepts_html(&req) {
        if let Some(resp) = serve("index.html", accept) {
            return resp;
        }
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn serve(path: &str, accept_encoding: &str) -> Option<Response> {
    // Pre-compressed sibling negotiation: prefer brotli, then gzip, then identity.
    let (data, encoding) = if accept_encoding.contains("br")
        && let Some(f) = Assets::get(&format!("{path}.br"))
    {
        (f.data, Some("br"))
    } else if accept_encoding.contains("gzip")
        && let Some(f) = Assets::get(&format!("{path}.gz"))
    {
        (f.data, Some("gzip"))
    } else {
        let f = Assets::get(path)?;
        (f.data, None)
    };

    let mime = mime_guess::from_path(path).first_or_octet_stream();

    let mut resp = Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        .body(Body::from(data.into_owned()))
        .ok()?;

    if let Some(enc) = encoding {
        resp.headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static(enc));
        resp.headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }

    let cache = if path == "index.html" {
        HeaderValue::from_static("no-cache")
    } else {
        HeaderValue::from_static("public, max-age=31536000, immutable")
    };
    resp.headers_mut().insert(header::CACHE_CONTROL, cache);
    Some(resp)
}

fn accepts_html(req: &Request) -> bool {
    req.headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("text/html") || s.contains("*/*"))
}
