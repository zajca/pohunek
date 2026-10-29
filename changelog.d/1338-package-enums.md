---
kind: changed
---
`pohunek plugin` no longer fails on a package origin, fault, selection block reason, install status, profile bind status or doctor finding kind that a newer daemon adds: the value reads as `unknown`. The official-downgrade guard of `plugin update` and the consent prompt of `plugin profile migrate` stay on for an unknown value, and `plugin doctor` still exits non-zero for an unknown finding.
