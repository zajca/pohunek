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
`find`, `sed`, `sort`, `grep`, `readlink` (with `-e`), `iconv`, `tr` and `wc` and fails on
every drift `verify-stage` refuses, except the checks against the committed
project and the lock. Keep the script in the repository or the runner, not in
the stage, which could otherwise only verify itself:

```sh
set -eu
[ -d "$1" ] && [ ! -h "$1" ] || exit 1
cd "$1"
root=$(pwd -P && printf x) || exit 1
nl='
'
root=${root%"$nl"x}
for manifest in STAGE.sha256 STAGE.links; do
  [ -f "$manifest" ] && [ ! -h "$manifest" ] || exit 1
  # Shell read can discard NUL, so validate raw bytes before parsing.
  iconv -f UTF-8 -t UTF-8 "$manifest" >/dev/null 2>&1 || exit 1
  [ "$(LC_ALL=C tr -cd '\000' < "$manifest" | wc -c)" -eq 0 ] || exit 1
done
tab=$(printf '\t')
# UTF-8 C1 controls are C2 80..9F; grep's C locale recognizes only ASCII controls.
c1_prefix=$(printf '\302')
c1_first=$(printf '\200')
c1_last=$(printf '\237')
has_control() {
  printf '%s' "$1" | LC_ALL=C grep -q -e '[[:cntrl:]]' -e "$c1_prefix[$c1_first-$c1_last]"
}
safe_path() {
  case $1 in
    ''|/*|*/|*//*|.|..|./*|../*|*/./*|*/../*|*/.|*/..|*\\*) return 1 ;;
  esac
  ! has_control "$1"
}
safe_target() {
  case $1 in ''|*\\*) return 1 ;; esac
  ! has_control "$1"
}
stays_inside() (
  path=$1
  target=$2
  case $target in /*) exit 1 ;; esac
  depth=0
  while [ "$path" != "${path#*/}" ]; do
    depth=$((depth + 1))
    path=${path#*/}
  done
  set -f
  IFS=/
  set -- $target
  for component do
    case $component in
      ..) [ "$depth" -gt 0 ] || exit 1; depth=$((depth - 1)) ;;
      ''|.) ;;
      *) depth=$((depth + 1)) ;;
    esac
  done
)
resolved_inside() (
  path=$1
  target=$2
  case $path in */*) candidate=${path%/*} ;; *) candidate=. ;; esac
  set -f
  IFS=/
  set -- $target
  for component do
    [ -n "$component" ] || continue
    candidate=$candidate/$component
    resolved=$(readlink -e "$candidate" && printf x) || exit 1
    case $resolved in "$root$nl"x|"$root"/*"$nl"x) ;; *) exit 1 ;; esac
  done
)
contains_line() {
  [ -n "$1" ] && printf '%s\n' "$1" | grep -Fxq -- "$2"
}
# Pass each directory as an argument so a newline in its name stays visible.
find . -type d -exec sh -c '
  control=$1
  newline=$2
  shift 2
  for dir do
    [ "$dir" = . ] && continue
    name=${dir##*/}
    printf "%s" "$name" | iconv -f UTF-8 -t UTF-8 >/dev/null 2>&1 || exit 1
    case $name in *\\*|*"$newline"*) exit 1 ;; esac
    if printf "%s" "$name" | LC_ALL=C grep -q -e "[[:cntrl:]]" -e "$control"; then exit 1; fi
  done
' sh "$c1_prefix[$c1_first-$c1_last]" "$nl" {} + || exit 1
files=
while :; do
  line=
  if IFS= read -r line; then :; elif [ -z "$line" ]; then break; else exit 1; fi
  case $line in *'  '*) ;; *) exit 1 ;; esac
  digest=${line%%'  '*}
  path=${line#"$digest  "}
  [ "${#digest}" -eq 64 ] || exit 1
  case $digest in *[!0-9a-f]*) exit 1 ;; esac
  safe_path "$path" || exit 1
  contains_line "$files" "$path" && exit 1
  files="${files}${files:+$nl}$path"
done < STAGE.sha256
links=
execs=
while :; do
  line=
  if IFS= read -r line; then :; elif [ -z "$line" ]; then break; else exit 1; fi
  case $line in
    "exec${tab}"*)
      path=${line#"exec${tab}"}
      case $path in *"$tab"*) exit 1 ;; esac
      safe_path "$path" || exit 1
      contains_line "$files" "$path" || exit 1
      contains_line "$execs" "$path" && exit 1
      execs="${execs}${execs:+$nl}$path"
      ;;
    "link${tab}"*)
      fields=${line#"link${tab}"}
      case $fields in *"$tab"*) ;; *) exit 1 ;; esac
      path=${fields%%"$tab"*}
      target=${fields#"$path$tab"}
      case $target in *"$tab"*) exit 1 ;; esac
      safe_path "$path" && safe_target "$target" && stays_inside "$path" "$target" || exit 1
      contains_line "$files" "$path" && exit 1
      contains_line "$links" "$path" && exit 1
      links="${links}${links:+$nl}$path"
      ;;
    *) exit 1 ;;
  esac
done < STAGE.links
expected=$({ printf '%s\n' "$files"; printf '%s\n' "$links"; } | sed '/^$/d' | LC_ALL=C sort)
actual=$(
  found=$(find . -path ./STAGE.sha256 -prune -o -path ./STAGE.links -prune -o ! -type d -print) || exit 1
  printf '%s\n' "$found" | sed 's|^\./||' | LC_ALL=C sort
)
[ "$expected" = "$actual" ]
printf '%s\n' "$files" | while IFS= read -r path; do
  [ -f "$path" ] && [ ! -h "$path" ] || exit 1
  # -perm -100 tests the owner's execute bit, not this process's access.
  owner_exec=$(find "./$path" -prune -perm -100 -print) || exit 1
  if contains_line "$execs" "$path"; then
    [ "$owner_exec" = "./$path" ] || exit 1
  else
    [ -z "$owner_exec" ] || exit 1
  fi
done
while IFS="$tab" read -r kind path target; do
  if [ "$kind" = link ]; then
    [ -h "$path" ] || exit 1
    actual=$(readlink "$path" && printf x) || exit 1
    [ "$actual" = "$target$nl"x ] || exit 1
    resolved_inside "$path" "$target" || exit 1
  fi
done < STAGE.links
sha256sum -c STAGE.sha256 >/dev/null
```
