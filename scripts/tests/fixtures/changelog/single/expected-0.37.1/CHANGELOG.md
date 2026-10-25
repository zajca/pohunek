# Changelog

User-facing changes of example/app.

## 0.37.1 - 2026-10-25

No user-facing changes.

## 0.37.0 - 2026-10-24

### Added

- Tailscale can carry the connection between hosts as an opt-in overlay. ([#12](https://github.com/example/app/issues/12))

### Changed

- **Breaking:** `pohunek attach` attaches read-only by default. ([#15](https://github.com/example/app/issues/15))
- The daemon starts faster on cold boot. ([#20](https://github.com/example/app/issues/20))

### Removed

- **Breaking:** The `--legacy-attach` flag is gone; use `pohunek attach`. ([#15](https://github.com/example/app/issues/15))

### Fixed

- `pohunek attach --dump` no longer cuts the last screen line. ([#9](https://github.com/example/app/issues/9))

## 0.36.0 - 2026-10-23

### Fixed

- Old fix. ([#3](https://github.com/example/app/issues/3))
