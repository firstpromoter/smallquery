---
paths:
  - "Dockerfile"
  - ".github/workflows/**"
  - "Cargo.toml"
  - "Cargo.lock"
---

# Images and releases

- `.github/workflows/image.yml`: `cargo test`, then one native build per
  architecture (`ubuntu-24.04` amd64, `ubuntu-24.04-arm` arm64) pushed by digest,
  then a merged manifest. Don't move arm64 to QEMU: the bundled DuckDB C++ build
  takes hours emulated.
- PRs build without pushing. `main` publishes `:latest` and `:sha-<short>`; a
  `vX.Y.Z` tag publishes `:X.Y.Z` and `:X.Y`. Images are public, so a push to `main`
  is a release to everyone who uses `:latest`.
- Bump `version` in `Cargo.toml` with each tag. Builds use `--locked`; commit
  `Cargo.lock` changes with the change that needs them.
- The Dockerfile builds dependencies in their own layer (dummy `main.rs`), so a
  source-only change reuses the DuckDB build. Keep that split when editing it.
- Pin third-party actions to a major version at least; never add a step that prints
  secrets or uploads build context outside GitHub.
