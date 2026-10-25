<!-- pohunek-changelog:start -->
### Added

- Tailscale can carry the connection between hosts as an opt-in overlay. ([#12](https://github.com/example/app/issues/12))

### Changed

- **Breaking:** `pohunek attach` attaches read-only by default. ([#15](https://github.com/example/app/issues/15))
- The daemon starts faster on cold boot. ([#20](https://github.com/example/app/issues/20))

### Removed

- **Breaking:** The `--legacy-attach` flag is gone; use `pohunek attach`. ([#15](https://github.com/example/app/issues/15))

### Fixed

- `pohunek attach --dump` no longer cuts the last screen line. ([#9](https://github.com/example/app/issues/9))
<!-- pohunek-changelog:end -->
