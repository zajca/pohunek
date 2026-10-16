## Unreleased (2026-10-16, pinned profiles and stopped sessions move to a newer official package version, #1199)

- `service upgrade` repins every host profile that is pinned to an older version
  of the release's official package. The plan lists it as `repin`, and the
  applied report as `repinned`, with both versions and digests. A pinned profile
  it does not move is listed as `kept` with a reason, and does not refuse the
  plan. The reasons are a newer version, a non-official package, another
  package, or an unknown version. See
  [the update runbook](runbooks/update-after-release.md).
- A stopped session launched from an official package resumes and forks through
  a strictly newer, selected official version of the same package. The newer
  version must keep the integration, the native reference check and the
  launch the session froze. A session launched from a profile moves only when
  that profile pins the newer version. Otherwise the session stays on its exact
  digest, and a live session is never moved. This covers Pi as well. A Pi
  profile is not repinned by `service upgrade`; move it with `plugin profile
  migrate`. A package installed by explicit digest or linked never adopts a
  session. The relaunch records the newer digest. The upgrade preflight reports
  such a session as `adoptable` with code `adoptable_by_newer_package`.
- A stopped session that the newer version does not adopt still resumes from
  its frozen digest after the upgrade repinned its profile.
- `plugin update` refuses an official archive whose version is lower than the
  selected official version (`plugin_update_downgrade`, naming both versions)
  before anything is written. It also refuses a version that cannot be ordered
  (`plugin_update_version_unordered`). An archive counts as official by its
  catalog, or when the registry records its digest as official. `plugin select`
  stays the explicit rollback.
- When the selected official package is newer than the release's, the upgrade
  plan says that the upgrade re-selects the release's version.
- "Official" means a record from a catalog that the host's single trust anchor
  verified; which root or key authorized it is not recorded yet (#1200).
