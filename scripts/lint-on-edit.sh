#!/usr/bin/env bash
# Post-edit clippy helper. Reads JSON {"tool_input":{"file_path":"..."}} on
# stdin, runs `cargo clippy` against the workspace if the touched file is
# Rust/Cargo, and prints diagnostics. Always exits 0 — advisory only.

set -u

INPUT=$(cat)

# Extract the edited file_path from the tool input.
FILE=$(printf '%s' "$INPUT" | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
    print(d.get("tool_input", {}).get("file_path", ""))
except Exception:
    pass
' 2>/dev/null)

# Resolve project root (this script lives in scripts/).
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

case "$FILE" in
    "$ROOT"/crates/*.rs|"$ROOT"/crates/*/build.rs|"$ROOT"/Cargo.toml|"$ROOT"/crates/*/Cargo.toml)
        ;;
    *)
        exit 0
        ;;
esac

cd "$ROOT" || exit 0

# Cargo command output. --no-deps avoids linting third-party crates;
# --quiet suppresses "Checking ..." noise; --message-format short keeps
# diagnostics one-per-line.
OUTPUT=$(cargo clippy --workspace --no-deps --quiet --message-format short 2>&1)
EXIT=$?

if [ "$EXIT" -ne 0 ] || [ -n "$OUTPUT" ]; then
    echo "::: clippy (exit $EXIT) :::"
    echo "$OUTPUT" | head -60
fi

exit 0
