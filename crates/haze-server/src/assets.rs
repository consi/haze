use std::sync::{Arc, OnceLock};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use rust_embed::RustEmbed;

// Path is resolved relative to CARGO_MANIFEST_DIR (this crate's directory).
#[derive(RustEmbed)]
#[folder = "../../frontend/build"]
struct Assets;

/// Token baked into the frontend bundle by `SvelteKit`'s `kit.paths.base`.
/// The handler rewrites every occurrence with the normalized
/// `HAZE_BASE_URL` (empty string in root mode) at serve time so the same
/// compiled bundle can deploy under any URL path.
///
/// The search needle includes the leading slash so that root-mode
/// substitution produces clean absolute paths (`"/foo"`) rather than
/// protocol-relative ones (`"//foo"`). `SvelteKit` always emits the base
/// with its leading slash, so this is universally safe.
///
/// Because the placeholder is baked into every emitted asset URL, the
/// rewrite has to happen in root mode too - there we replace it with the
/// empty string. The pre-compressed `.br`/`.gz` siblings produced at build
/// time also contain the placeholder and therefore can't be reused;
/// `build_app` wraps the asset branch with `CompressionLayer` to cover
/// on-the-wire compression in both modes instead.
pub const BASE_PLACEHOLDER: &str = "/__HAZE_BASE__";

/// Cache of `(asset_path, base)` → rewritten bytes. Populated lazily on
/// first request. Memory bound: roughly the size of `frontend/build`
/// (a few MB).
static REWRITE_CACHE: OnceLock<DashMap<RewriteKey, Arc<Vec<u8>>>> = OnceLock::new();

#[derive(Clone, PartialEq, Eq, Hash)]
struct RewriteKey {
    path: String,
    base: String,
}

fn cache() -> &'static DashMap<RewriteKey, Arc<Vec<u8>>> {
    REWRITE_CACHE.get_or_init(DashMap::new)
}

// `async` is required so the closure that captures `base` in `build_app`
// satisfies axum's `Handler` trait; there's nothing to await internally.
#[allow(clippy::unused_async)]
pub async fn handler(req: Request, base: String) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(resp) = serve(path, &base) {
        return resp;
    }

    // SPA fallback: non-asset GETs that accept HTML get index.html so client routing works.
    if accepts_html(&req)
        && let Some(resp) = serve("index.html", &base)
    {
        return resp;
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn serve(path: &str, base: &str) -> Option<Response> {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let bytes = if is_text_like(mime.as_ref()) {
        get_or_rewrite(path, base)?
    } else {
        let f = Assets::get(path)?;
        Arc::new(f.data.into_owned())
    };

    let mut resp = Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        .body(Body::from(bytes.as_ref().clone()))
        .ok()?;

    let cache_value = if path == "index.html" {
        // index.html is the SPA shell. `no-store` is intentionally stronger
        // than `no-cache`: it prevents the browser disk/memory cache AND
        // the back-forward cache from serving an old shell, which would
        // otherwise pin clients to stale hashed-bundle URLs and stale
        // client-routing logic across deploys. Pragma+Expires are belt-
        // and-braces for misbehaving proxies that ignore Cache-Control.
        HeaderValue::from_static("no-store, no-cache, must-revalidate, max-age=0")
    } else {
        // All other emitted assets are content-hashed by Vite, so they're
        // safe to cache aggressively.
        HeaderValue::from_static("public, max-age=31536000, immutable")
    };
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, cache_value);
    if path == "index.html" {
        resp.headers_mut()
            .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
        resp.headers_mut()
            .insert(header::EXPIRES, HeaderValue::from_static("0"));
    }
    Some(resp)
}

fn get_or_rewrite(path: &str, base: &str) -> Option<Arc<Vec<u8>>> {
    let key = RewriteKey {
        path: path.to_owned(),
        base: base.to_owned(),
    };
    if let Some(hit) = cache().get(&key) {
        return Some(hit.clone());
    }
    let f = Assets::get(path)?;
    let raw = f.data.into_owned();
    let rewritten = if contains(&raw, BASE_PLACEHOLDER.as_bytes()) {
        match std::str::from_utf8(&raw) {
            Ok(s) => s.replace(BASE_PLACEHOLDER, base).into_bytes(),
            // Asset claims a text mime but isn't valid UTF-8 - serve the
            // raw bytes rather than refusing. This shouldn't happen in
            // practice with anything Vite emits.
            Err(_) => raw,
        }
    } else {
        raw
    };
    let arc = Arc::new(rewritten);
    cache().insert(key, arc.clone());
    Some(arc)
}

/// Substring search. Asset bodies are at most a few hundred KB and the
/// rewrite is one-time per asset (cached afterwards), so a naive scan is
/// fine and saves a dep on `memchr`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// MIME types whose bodies are text and may contain the `__HAZE_BASE__`
/// placeholder. Anything not on this list is served byte-for-byte.
fn is_text_like(mime: &str) -> bool {
    matches!(
        mime,
        "text/html"
            | "text/css"
            | "text/plain"
            | "application/javascript"
            | "text/javascript"
            | "application/json"
            | "application/manifest+json"
            | "image/svg+xml"
    )
}

fn accepts_html(req: &Request) -> bool {
    req.headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("text/html") || s.contains("*/*"))
}
