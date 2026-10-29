## Unreleased (2026-10-29, tolerant package enums, #1338)

- A client of an older build reads a package origin, fault, selection block reason, install status, profile bind status and doctor finding kind that a newer daemon adds as `unknown`. An unknown origin keeps the official-downgrade guard of `plugin update` engaged, an unknown bind status never skips the consent prompt of `plugin profile migrate`, and an unknown finding kind is kept so `plugin doctor` still exits non-zero. `PackageTrust` and the package registry stay strict.
