## Unreleased (2026-10-20, newer-version adoption trust and single resolution, #1203, #1204)

A release package whose digest the registry records under another trust source
(for example `explicit_digest`) is no longer treated as the official newer pin
or adopter: the upgrade preflight now agrees with resume, which refuses such a
profile with `agent_profile_changed`. Resume and fork resolve the recovery
definition once and build the profile check on it, so a profile repinned
while an operation runs cannot record a revision that describes another
package version than the one launched. See
[update after release](runbooks/update-after-release.md).
