use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "haze",
    version,
    about = "HAZE - Single-binary network latency monitor",
    long_about = "HAZE - Single-binary network latency monitor.\n\n\
                  Run `haze` with no arguments to start the server. Migrations run\n\
                  automatically on startup, and the first server boot with an empty\n\
                  database creates an admin user with a random password printed to logs."
)]
struct Cli {
    #[arg(long, env = "HAZE_DATA_DIR", default_value = "./data", global = true)]
    data_dir: PathBuf,

    #[arg(long, env = "HAZE_LOG", default_value = "info", global = true)]
    log: String,

    #[arg(long, env = "HAZE_BIND", default_value = "127.0.0.1:4420")]
    bind: String,

    /// Public origin URL the browser sees, e.g. `<https://haze.example.com>`.
    /// Required for `WebAuthn` passkeys; if omitted, passkeys are disabled.
    #[arg(long, env = "HAZE_ORIGIN")]
    origin: Option<String>,

    /// URL path prefix to deploy under, e.g. `/haze`. Empty/unset means
    /// the app is served at root `/`. The prefix must be a path only -
    /// scheme, host, query or fragment are rejected.
    #[arg(long, env = "HAZE_BASE_URL", default_value = "")]
    base_url: String,

    /// Alias for `--base-url`. Accepted because "base path" is the more
    /// natural framing for this knob even though the env var keeps the
    /// `URL` suffix for symmetry with the existing `HAZE_*` family.
    #[arg(long, conflicts_with = "base_url")]
    base_path: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log);

    haze_server::run(haze_server::Config {
        bind: cli.bind,
        data_dir: cli.data_dir,
        origin: cli.origin,
        base_url: cli.base_path.unwrap_or(cli.base_url),
    })
    .await
    .context("server exited with error")
}

fn init_tracing(directive: &str) {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter = EnvFilter::try_new(directive).unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = fmt::layer().with_target(false);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}
