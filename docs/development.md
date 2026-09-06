# Development and releases

Use Rust 1.98.0 (`rust-toolchain.toml`) and Node 24.20.0
(`frontend/.node-version`). `just setup-tools` installs the pinned developer
tools, including git-cliff 2.13.1.

## Meaningful changelogs

Use Conventional Commit subjects describing the observable change:
`fix(history): retain responding transit hops when a trace times out`.
Use the body to explain the trigger, resulting behavior, and relevant limitations.
Mark incompatible changes with `!` and a `BREAKING CHANGE:` footer.
Historical descriptive subjects and bodies remain included; unknown subjects
appear under maintenance instead of silently disappearing.

Run `just release-notes` to preview unreleased commits and `just changelog` to
regenerate `CHANGELOG.md` before tagging. Include the regenerated file in the
release preparation commit. A tag-triggered release fetches complete history
and publishes only that tag's git-cliff notes. It does not rewrite old releases
or push generated commits back to the repository.

## Dependency vetting

The upgrade cutoff was **2026-08-30 09:03:11 UTC**, seven days before the
recorded start of this update. `dependency-vetting.json` records registry
publication evidence for every locked Rust/npm package. The independent CI
check fetches current authoritative metadata and enforces a full 168 hours
for all direct, transitive, development, optional, and platform-specific
lockfile entries. Registry failures and unknown sources fail the check.

`frontend/.npmrc` also applies npm's seven-day minimum release age when
resolving updates. Cargo updates must be followed by `just dependency-age`;
replace any too-young versions with eligible versions using `cargo update
-p PACKAGE --precise VERSION`, then check again. Tightly coupled packages
such as wasm-bindgen must be resolved together. CI/builds use lockfiles.

`tooling-vetting.json` records reviewed upstream publication evidence for
build helpers, toolchains, immutable action revisions, and container digests.
Update the evidence when changing a pin. `scripts/check_tooling.py` rejects
unrecorded pins, missing sources, and publication dates younger than a week.
Unlike the application lockfile check, tooling evidence is reviewed metadata;
the checker does not infer publication dates from Git commit timestamps or
image creation dates. Distribution packages installed by apt/apk remain under
their distribution's security-update policy.

Compatibility decisions:

- SQLx moves to 0.9. Its dynamic-SQL safety requirement is handled only after
  reviewing the existing constant column lists and generated placeholders;
  all external values continue to use bind parameters.
- TypeScript stays on 6.0.3: the eligible TypeScript 7 package cannot supply
  the compiler API used by svelte-check 4.7.6. Node types track the Node 24
  runtime rather than adopting Node 26-only APIs.
- SvelteKit's cookie dependency is overridden to the compatible patched 0.7
  series for [GHSA-pxg6-pf52-xh8x](https://github.com/advisories/GHSA-pxg6-pf52-xh8x).
- Rust 1.99 and several newer transitive releases were excluded by age.
  An older, eligible Rust 1.98 container digest is pinned for the same reason.

## UI checks

The shared modal owns focus containment, Escape, focus restoration and body
scroll locking. Use its `contained` option for desktop layouts whose child
panes own scrolling. On mobile the body remains scrollable.

Keep small glyphs inside `.icon-button`: 32px targets on desktop, 44px on
touch/mobile. Allocate actual layout space; do not overlap neighboring hit
areas. Tables may scroll locally, while pages and dialogs must fit their
viewport. Scrollbars use native thin fallbacks where exact pixel widths are
not supported.

Run `scripts/route_history_ui_test.py` with Python Playwright and Chromium.
Set `HAZE_TEST_URL` to the running app (for Vite preview, include
`/__HAZE_BASE__`). Optionally set `HAZE_AXE_PATH` to an installed axe-core
`axe.min.js` to scan the main routes. Test both themes, narrow/short screens,
keyboard access, and long route names; automated checks do not replace manual
assistive-technology testing.
