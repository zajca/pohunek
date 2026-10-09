---
type: Concept
id: concept/upstream-staging
title: Upstream staging
description: How cargo xtask compat stage-upstream installs the pinned upstream runtime releases (Pi, Codex, Claude Code) from a committed npm lockfile, the lock fields and manifests that pin every byte, the offline verify-stage check and its POSIX equivalent, and why a lock edit changes attestation digests.
source_kind: manual
intents: [debug, help, project]
---

# Upstream staging

The official runtime packages are tested against the real upstream runtime.
`cargo xtask compat stage-upstream` installs exactly the release a runtime's
`compat/<runtime>/compatibility-lock.json` pins, with network access, into a
directory that can later be verified and used offline (for example inside a
network-isolated archive smoke run). Implemented in
`crates/xtask/src/upstream_stage.rs`; it knows no runtime by name, so a new
`runtime-packages/<runtime>` is staged by adding its lock fields and npm project.

## What the lock pins

Besides the supported range, the `upstream` object of an npm-based lock carries:

| Field | Meaning |
| --- | --- |
| `npm`, `release` | The npm package and the exact release. |
| `integrity` | The `sha512-...` SRI of the root tarball (`dist.integrity` of the registry). It must equal the integrity of the root entry in the npm lockfile and of the installed package. |
| `binary` | The executable name; `bin/<binary>` of the stage. |
| `version_output` | The `--version` banner with a single `{release}` placeholder (`codex-cli {release}`, `{release} (Claude Code)`, `{release}`). |
| `scripts` | Optional, default `false`. `true` runs install scripts (Claude Code's postinstall links the native binary); the lockfile must then show the locked package as the only one with an install script. |
| `sha256` | Optional map from `<os>-<arch>` (`linux-x64`) to the SHA-256 of the native binary `bin/<binary>` resolves to; a host without an entry is refused. |

`compat/<runtime>/npm/package.json` depends on exactly the locked release (no
ranges, no other dependency kind) and `compat/<runtime>/npm/package-lock.json`
(lockfile version 3) pins the whole tree: every package needs a `sha512`
integrity and a `resolved` URL on one `https` origin, which becomes the
registry npm is told to use. Regenerate it with `npm install
--package-lock-only --ignore-scripts` after changing the release. A lock that
is not an npm release (another `schema`, no `upstream.npm`, such as the Hermes
lock) is refused as an unsupported shape.

Lock bytes are hashed into every attestation (`upstream_lock_digest`), so any
edit of a lock file, including adding these fields, changes the digests of the
attestations made afterwards.

## Staging

`cargo xtask compat stage-upstream --runtime <rt> --out <dir>` writes
`<dir>/<rt>/` as an npm prefix: `lib/` is the npm project (`package.json`,
`package-lock.json`, `node_modules/`) and `bin/<binary>` is a relative link into
it. It runs `npm ci` with a cleared environment (only `PATH`, which must reach
`node`, plus private `HOME`, cache and empty user and global npmrc files, so no
token or registry setting of the host reaches npm), `--ignore-scripts` unless the
lock says otherwise, and the registry of the lockfile. Then it checks the
installed release and root integrity, the optional native digest, writes the
manifests, runs `<binary> --version` in a private `HOME` and requires the lock's
banner, and re-checks the manifests so a runtime that writes into its own
install is refused. A non-empty destination is refused; a failed run leaves
nothing behind. Node is not staged: the runner provides it.

The manifests at the stage root pin every byte:

- `STAGE.sha256`: `sha256sum -c` format, one line per regular file, relative
  paths in byte order.
- `STAGE.links`: tab-separated lines `link<TAB>path<TAB>target` for every
  symbolic link and `exec<TAB>path` for every file whose owner-execute bit is
  set. Links must be relative and stay inside the stage; names with control
  characters or backslashes are refused.

## Verification

`cargo xtask compat verify-stage --runtime <rt> --dir <dir>` is offline. It
recomputes every file digest, link target and executable bit from disk and
refuses an added, removed or modified file, a file replaced by a link, a
retargeted link or a changed executable bit (typed error naming the path, never
the content). It also requires `lib/package.json` and `lib/package-lock.json` to
equal the committed npm project and re-checks the installed release, integrity
and native digest against the lock. The manifests guard against drift between
staging and use; they are not a signature, so the committed lock and npm
project remain the root of trust.

Where no Rust tooling exists (the isolated namespace) run this POSIX `sh`
verifier with the stage directory as its argument. It needs `sha256sum`,
`find`, `sed`, `sort`, `grep` and `readlink` (coreutils or busybox) and fails on
every drift `verify-stage` refuses, except the checks against the committed
project and the lock. Keep the script in the repository or the runner, not in
the stage, which could otherwise only verify itself:

```sh
set -eu
cd "$1"
for manifest in STAGE.sha256 STAGE.links; do
  [ -f "$manifest" ] && [ ! -h "$manifest" ] || exit 1
done
sha256sum -c STAGE.sha256 >/dev/null
tab=$(printf '\t')
files=$(sed 's/^[0-9a-f]\{64\}  //' STAGE.sha256)
links=$(while IFS="$tab" read -r kind path target; do
  if [ "$kind" = link ]; then printf '%s\n' "$path"; fi
done < STAGE.links)
execs=$(while IFS="$tab" read -r kind path target; do
  if [ "$kind" = exec ]; then printf '%s\n' "$path"; fi
done < STAGE.links)
expected=$({ printf '%s\n' "$files"; printf '%s\n' "$links"; } | sed '/^$/d' | LC_ALL=C sort)
actual=$(find . -path ./STAGE.sha256 -prune -o -path ./STAGE.links -prune -o ! -type d -print | sed 's|^\./||' | LC_ALL=C sort)
[ "$expected" = "$actual" ]
printf '%s\n' "$files" | while IFS= read -r path; do
  [ -f "$path" ] && [ ! -h "$path" ] || exit 1
  if printf '%s\n' "$execs" | grep -Fxq -- "$path"; then
    [ -x "$path" ] || exit 1
  else
    [ ! -x "$path" ] || exit 1
  fi
done
while IFS="$tab" read -r kind path target; do
  case $kind in
    link) [ -h "$path" ] && [ "$(readlink "$path")" = "$target" ] || exit 1 ;;
    exec) ;;
    *) exit 1 ;;
  esac
done < STAGE.links
```
