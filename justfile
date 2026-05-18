# Haze - dev + release orchestration.
#
# Install just: `cargo install just` or via your distro package manager.
# `just --list` shows every recipe with its doc comment.

set dotenv-load := true
set shell := ["bash", "-cu"]

# Env defaults for dev. Override via .env or `HAZE_BIND=... just backend`.
export HAZE_BIND := env_var_or_default("HAZE_BIND", "127.0.0.1:4420")
export HAZE_DATA_DIR := env_var_or_default("HAZE_DATA_DIR", "./data")
export HAZE_LOG := env_var_or_default("HAZE_LOG", "haze=debug,info")

# Default target: print help.
default:
    @just --list

# ─── Setup ──────────────────────────────────────────────────────────────────

# One-time setup after fresh clone: fetch cargo deps, install npm deps.
setup:
    cargo fetch
    cd frontend && npm ci

# Install rust toolchain components + a couple of cargo dev tools.
setup-tools:
    rustup component add rustfmt clippy
    cargo install --locked cargo-deny cargo-audit cargo-nextest cargo-zigbuild

# ─── Dev loop (two terminals) ───────────────────────────────────────────────

# Backend in dev mode (auto-reloads with cargo-watch if installed; else plain run).
backend:
    @ulimit -n 65536 2>/dev/null || ulimit -n 10240; \
    if command -v cargo-watch >/dev/null; then \
        cargo watch -q -c -x 'run -p haze-cli'; \
    else \
        echo "Tip: cargo install cargo-watch for auto-reload"; \
        cargo run -p haze-cli; \
    fi

# Frontend in dev mode. Vite on :5173, proxies /api to the backend.
frontend:
    cd frontend && npm run dev

# Print the two commands to run side-by-side. Use `just backend` / `just frontend`
# in separate terminals - just doesn't run parallel recipes natively.
dev:
    @echo "Terminal 1:  just backend"
    @echo "Terminal 2:  just frontend"
    @echo "Then open:   http://127.0.0.1:5173"
    @echo ""
    @echo "Sub-path testing (deploy under e.g. /haze):"
    @echo "  Terminal 1:  HAZE_BASE_URL=/haze just backend"
    @echo "  Terminal 2:  just frontend"
    @echo "  Then open:   http://127.0.0.1:5173/__HAZE_BASE__/  (Vite proxy strips the sentinel)"
    @echo "             or http://127.0.0.1:4420/haze/         (talking to the backend directly)"

# ─── Build ──────────────────────────────────────────────────────────────────

# Build the frontend, then a release binary with embedded assets.
release:
    cd frontend && npm ci && npm run build
    cargo build --release

# Build only the backend (dev profile, no embedded frontend rebuild).
build:
    cargo build --workspace

# Cross-compile static Linux binaries via cargo-zigbuild (matches release CI).
release-all:
    cd frontend && npm ci && npm run build
    cargo zigbuild --release --target x86_64-unknown-linux-musl -p haze-cli
    cargo zigbuild --release --target aarch64-unknown-linux-musl -p haze-cli

# Build just the frontend (no Rust).
frontend-build:
    cd frontend && npm run build

# ─── Quality gates ──────────────────────────────────────────────────────────

# `cargo check` over the whole workspace including tests.
check:
    cargo check --workspace --all-targets

# Format Rust + frontend (frontend formatter is optional).
fmt:
    cargo fmt --all
    cd frontend && (npm run format 2>/dev/null || true)

# Strict clippy + svelte-check.
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cd frontend && npm run check

# Auto-apply clippy suggestions (use with care).
lint-fix:
    cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged -- -D warnings

# All tests (uses cargo-nextest if available, else plain cargo test).
test:
    @if command -v cargo-nextest >/dev/null; then \
        cargo nextest run --workspace; \
    else \
        cargo test --workspace; \
    fi

# Property tests on the storage engine (long).
test-store:
    PROPTEST_CASES=1024 cargo test -p haze-store --release

# Run the full local CI sequence: fmt-check, lint, test, audit.
ci: fmt-check lint test audit

# Fail if anything is unformatted.
fmt-check:
    cargo fmt --all -- --check

# ─── Security / supply chain ────────────────────────────────────────────────

# License + advisory + source check via cargo-deny.
deny:
    cargo deny check

# Audit advisories only (faster than full cargo deny).
audit:
    cargo audit

# ─── Data lifecycle ─────────────────────────────────────────────────────────

# Drop everything in $HAZE_DATA_DIR (sqlite + chunk files). Destructive.
# Migrations run automatically on next `just backend`, and the first boot
# with an empty DB provisions an `admin` user with a logged random password.
clean-data:
    rm -rf "$HAZE_DATA_DIR"

# ─── Operations ─────────────────────────────────────────────────────────────

# Dump the OpenAPI spec to openapi.json (for frontend codegen and CI checks).
# Requires a running server (the CLI no longer exports OpenAPI - the schema
# is only available through the live `/api/openapi.json` endpoint).
openapi:
    @mkdir -p frontend/src/lib/api
    curl -sf http://127.0.0.1:4420/api/openapi.json > frontend/src/lib/api/openapi.json
    @echo "Wrote frontend/src/lib/api/openapi.json"

# Regenerate the typed TypeScript API client from the OpenAPI spec.
gen-api: openapi
    cd frontend && npx openapi-typescript src/lib/api/openapi.json -o src/lib/api/schema.d.ts
    @echo "Wrote frontend/src/lib/api/schema.d.ts"

# ─── Clean ──────────────────────────────────────────────────────────────────

# Remove cargo build artifacts.
clean:
    cargo clean

# Nuke everything (cargo target, frontend build + node_modules, data dir).
clean-all: clean clean-data
    rm -rf frontend/build frontend/node_modules frontend/.svelte-kit
