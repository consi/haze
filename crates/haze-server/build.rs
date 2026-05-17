// Ensure frontend/build/ exists with at least a stub index.html so rust-embed
// can compile against a non-empty folder before the SvelteKit UI has been built.
// `just release` runs `npm run build` first and overwrites this placeholder.

use std::path::PathBuf;

fn main() {
    let manifest_dir: PathBuf = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR")
        .into();
    let frontend_build = manifest_dir
        .join("..")
        .join("..")
        .join("frontend")
        .join("build");
    let index = frontend_build.join("index.html");

    if !index.exists() {
        std::fs::create_dir_all(&frontend_build).expect("create frontend/build");
        std::fs::write(&index, PLACEHOLDER).expect("write placeholder index.html");
    }

    println!("cargo:rerun-if-changed={}", frontend_build.display());
}

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Haze</title></head>
<body style="font-family: -apple-system, system-ui, sans-serif; padding: 2rem;">
<h1>Haze</h1>
<p>Placeholder. Run <code>npm install &amp;&amp; npm run build</code> in <code>frontend/</code>, then rebuild.</p>
</body>
</html>
"#;
