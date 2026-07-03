# Split Release Artifacts Design

Date: 2026-07-03

## Goal

Keep the existing single release cadence and shared workspace version, but publish
separate downloadable archives for the CLI, daemon, and GUI.

## Assumptions

- The Git tag remains `vX.Y.Z`, created by `scripts/release`.
- The workspace keeps one `[workspace.package] version`; component-specific
  semantic versions are out of scope.
- One GitHub Release page contains all component archives for a tag.
- Each component archive should be usable without downloading the other
  components.
- Release archives continue to include `README.md`, `LICENSE`, and offline docs.

## Success Criteria

- Pushing `vX.Y.Z` produces separate archives for each supported target:
  - `pohunek-cli-X.Y.Z-<target>.tar.gz`
  - `pohunek-daemon-X.Y.Z-<target>.tar.gz`
  - `pohunek-gui-X.Y.Z-<target>.tar.gz`
- Each archive contains exactly one binary:
  - CLI archive: `pohunek`
  - daemon archive: `pohunekd`
  - GUI archive: `pohunek-gui`
- Each archive also contains `README.md`, `LICENSE`, `docs/offline/`, and
  `docs/manifest.json`.
- Each archive has its own `.sha256` checksum attached to the same GitHub
  Release.
- Documentation no longer describes the release artifact as one archive
  containing all three binaries.
- `cargo xtask docs check` passes after documentation updates.

## Chosen Approach

Use a component-aware packaging matrix in `.github/workflows/release.yml`.

The release workflow still runs one quality gate for the tagged commit and still
uses the same target matrix. The build step can continue compiling all three
binaries for a target in one command. The packaging step becomes component-aware:
for each target and component, it stages one binary plus common release extras,
creates a component-specific archive, creates a checksum, and attaches those
files to the GitHub Release.

This keeps release cadence simple while giving the operator smaller, clearer
downloads.

## Workflow Shape

The release build matrix gains a `component` dimension with:

- `cli`: package name `pohunek-cli`, binary `pohunek`
- `daemon`: package name `pohunek-daemon`, binary `pohunekd`
- `gui`: package name `pohunek-gui`, binary `pohunek-gui`

The build step remains:

```sh
cargo build --release --locked --target "$target" \
  --bin pohunek --bin pohunekd --bin pohunek-gui
```

The package step uses the component metadata to create:

```text
dist/pohunek-cli-X.Y.Z-<target>.tar.gz
dist/pohunek-daemon-X.Y.Z-<target>.tar.gz
dist/pohunek-gui-X.Y.Z-<target>.tar.gz
```

Each staging directory is named the same as its archive without `.tar.gz`.

## Documentation Changes

Update user-facing release text in:

- `README.md`
- `docs/knowledge/guides/gui.md`
- `docs/knowledge/runbooks/update-after-release.md`

The update-after-release runbook should state that an operator downloads the
component archive for the binary they are updating.

The source map already lists release packaging files, so it should only need a
check unless a new tracked path is introduced.

## Out Of Scope

- Separate component versions.
- Separate component tags.
- Separate GitHub Release pages.
- Package manager artifacts such as `.deb`, `.rpm`, or AppImage.
- Backward compatibility for old combined release archives.

## Verification

- Run `cargo xtask docs check`.
- Validate the release workflow YAML structure locally if a YAML-aware tool is
  available.
- Inspect the package shell step to confirm each component archive includes one
  binary and the common release extras.
