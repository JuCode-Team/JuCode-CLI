# Releasing JuCode CLI

JuCode CLI follows the same distribution shape as Codex CLI:

- GitHub Releases are the canonical native artifacts.
- npm is a convenience install channel for the native binaries.
- The public npm entrypoint is `@jucode/cli`.

## Package layout

- `npm/cli`: meta package with the `jucode` launcher.
- `npm/cli-win32-x64`: Windows x64 native binary.
- `npm/cli-linux-x64`: Linux x64 native binary.
- `npm/cli-darwin-x64`: macOS Intel native binary.
- `npm/cli-darwin-arm64`: macOS Apple Silicon native binary.

## One-time setup

1. Create the npm scope and grant publish access for `@jucode`.
2. Add `NPM_TOKEN` to the GitHub repository secrets.
3. Confirm the repository has permission to create releases with `GITHUB_TOKEN`.

## Release flow

1. Bump the version in `Cargo.toml`.
2. Trigger the release workflow with `release_version=X.Y.Z`, or create and push a git tag in the form `vX.Y.Z`.
3. GitHub Actions builds all native binaries, packs the npm packages, creates the GitHub Release, and publishes to npm when `NPM_TOKEN` is present.

## Local verification

1. `cargo build --release --target x86_64-pc-windows-msvc`
2. Copy or build each target binary into `artifacts/<rust-target>/`.
3. `npm run release:sync-version`
4. `npm run release:stage-binaries -- --source-root artifacts`
5. `npm run release:pack`

The generated npm tarballs are written to `dist/npm/`.
