---
type: Concept
id: concept/upstream-staging
title: Upstream staging
description: How cargo xtask compat stage-upstream installs the pinned upstream runtime releases, Pi, Codex and Claude Code from a committed npm lockfile and Hermes through its repository provisioner, the lock fields and manifests that pin every byte, why a provisioned stage never moves and is verified from a pristine copy before each use, the offline verify-stage check and its POSIX equivalent, and why a lock edit changes attestation digests.
source_kind: manual
intents: [debug, help, project]
---

# Upstream staging

The official runtime packages are tested against the real upstream runtime.
`cargo xtask compat stage-upstream` installs exactly the release a runtime's
`compat/<runtime>/compatibility-lock.json` pins, with network access, into a
directory that can later be verified and used offline (for example inside a
network-isolated archive smoke run). Implemented in
`crates/xtask/src/upstream_stage.rs`; it knows no runtime by name. The lock's
`upstream` object selects how a release is installed: an npm release (no
`upstream.kind`, an `upstream.npm` package) from a committed npm project, or a
provisioned runtime (`upstream.kind` `provisioner`) by a repository script. A
new `runtime-packages/<runtime>` is staged by adding its lock fields and its
npm project or provisioner.

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
of another `schema` or `upstream.kind`, or without `upstream.kind` and
`upstream.npm`, is refused as an unsupported shape.

A provisioned runtime's `upstream` object carries:

| Field | Meaning |
| --- | --- |
| `kind` | `provisioner`. npm fields (`npm`, `integrity`, `sha256`) beside it are refused. |
| `provisioner` | The install script, `scripts/<name>`: a regular, owner-executable file directly in the repository's `scripts/` directory, never a link. Any other path is refused before anything runs. |
| `release`, `binary`, `version_output` | As for npm. The script owns its other pins; `scripts/provision-hermes-compat` verifies the Hermes source archive and a pinned CPython build (`compat/hermes/README.md`). |

Lock bytes are hashed into every attestation (`upstream_lock_digest`), so any
edit of a lock file, including adding these fields, changes the digests of the
attestations made afterwards.

## Staging

`cargo xtask compat stage-upstream --runtime <rt> --out <dir>` writes
`<dir>/<rt>/`. A non-empty destination is refused.

### npm releases

For an npm lock the stage is an npm prefix: `lib/` is the npm project (`package.json`,
`package-lock.json`, `node_modules/`) and `bin/<binary>` is a relative link into
it. It runs `npm ci` with a cleared environment (only `PATH`, which must reach
`node`, plus private `HOME`, cache and empty user and global npmrc files, so no
token or registry setting of the host reaches npm), `--ignore-scripts` unless the
lock says otherwise, and the registry of the lockfile. Then it checks the
installed release and root integrity, the optional native digest, writes the
manifests, runs `<binary> --version` in a private `HOME` and requires the lock's
banner, and re-checks the manifests so a runtime that writes into its own
install is refused. The install runs in a scratch directory that is moved to
`<dir>/<rt>/` only when every check passed, so a failed run leaves nothing
behind. Node is not staged: the runner provides it.

### Provisioned runtimes

The `provisioner` kind stages a Python virtual environment: the script must
create it at `venv/` of the install root, and every entrypoint must run its
interpreter `venv/bin/python` or `venv/bin/python3`. uv names the interpreter
as its `sys.executable` reports it: `python` on Linux, `python3` on macOS. A
provisioned runtime that is not a Python
virtual environment needs its interpreter declared in the lock before this
kind can stage it.

For a `provisioner` lock the command runs the script with `<dir>/<rt>` as its
install root and builds the stage there, in place. The script gets a cleared
environment (only `PATH`, which must reach the script's own tools, plus a
private `HOME` and `TMPDIR`), a 30-minute limit, and must print the absolute
path of an entrypoint named `<binary>` inside the stage as its only output.
The command then links `bin/<binary>` to that entrypoint and copies the lock
and the script into `inputs/` (`inputs/compatibility-lock.json`,
`inputs/<script name>`). It requires the entrypoint to start its interpreter
directly: a first line exactly `#!<stage>/venv/bin/python` or
`#!<stage>/venv/bin/python3`, with the canonical stage path (`/private/tmp`, not
`/tmp`, on macOS) and no arguments, naming a regular owner-executable file.
Another interpreter, `venv/bin/python3.12` beside them included, is refused. Then it
writes the manifests, runs `<binary> --version` in a private `HOME`, requires
the lock's banner as the first output line (Hermes prints its install
directory and Python version after it), and re-checks the manifests. A failed
run removes the partial stage.

A provisioned stage is used only at the path it was staged at, never moved:
uv writes the virtual environment's interpreter into every console script as
an absolute shebang. A copy must be restored to the same absolute path (the CI
cache does that); `verify-stage` refuses a stage moved anywhere else. For the
same reason a stage path at which uv would write a `#!/bin/sh` exec trampoline
is refused before the script runs: `<dir>/<rt>/venv/bin/python3`, the
longest interpreter path the shebang check accepts, longer than 124 bytes, so
that the shebang line exceeds 127 bytes, or a path with whitespace. Pohunek's frozen launch rule refuses a trampoline root.

A provisioned stage is verified from a pristine copy before each use, not
after it. Its consumers run Python with `PYTHONDONTWRITEBYTECODE=1` (the
`--version` probe of the command, the provisioner's own probe, the isolated
environment of `cargo xtask hermes compatibility`, the daemon's capability
probe, the CLI's Hermes runner for the Hermes processes it starts), but not all
of them do, and the variable alone does not keep the tree byte-stable:

- A Hermes session that the daemon launches runs without the variable and
  compiles bytecode for everything it imports into the stage's standard
  library and site-packages.

- Python's isolated mode (`-I`) ignores the variable, and the `.pth` finder of
  an editable install is compiled at interpreter startup, before a script can
  set `sys.dont_write_bytecode`; only `-B` prevents it. The CLI's staged-plugin
  validator and installed-plugin probe run the stage's interpreter with `-I`
  and no `-B` and import `yaml` or `sqlite3` before setting the flag, so they
  write `__pycache__/*.pyc` into the stage.
- Hermes writes into its own editable checkout on launch:
  `source/.bytecode-fingerprint` on the first run, and the interactive banner's
  update check runs `git fetch` in `source/` (`FETCH_HEAD`, `shallow.lock`,
  pack files).

So `verify-stage` refuses a stage after the compatibility gate has run against
it. The CI runtime cache is a copy taken before any consumer runs, restored to
the staging path and verified before the gate; a second consumer of the same
stage needs a fresh restore of that copy, not the tree the first one used.

### Manifests

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
the content). For an npm stage it also requires `lib/package.json` and
`lib/package-lock.json` to equal the committed npm project and re-checks the
installed release, integrity and native digest against the lock. For a
provisioned stage it requires `inputs/` to equal the committed lock and
provisioner, `bin/<binary>` to resolve to an entrypoint of that name inside the
stage, and that entrypoint's first line to be exactly
`#!<stage>/venv/bin/python` or `#!<stage>/venv/bin/python3`: a `/bin/sh`
trampoline, another interpreter or a moved stage is refused even when its bytes
are recorded. The manifests guard against drift between staging and use; they are
not a signature, so the committed lock and npm project or provisioner remain
the root of trust.

Where no Rust tooling exists (the isolated namespace) run this POSIX `sh`
verifier with the stage directory as its argument. It needs `sha256sum`,
`find`, `sed`, `sort`, `grep`, `readlink` (with `-e`), `iconv`, `tr` and `wc` and fails on
every drift `verify-stage` refuses, except the checks against the committed
project, provisioner and lock and the shebang check of a provisioned
entrypoint. Keep the script in the repository or the runner, not in
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
  # Resolve the symlink itself to retain a final slash or dot after a file.
  resolved=$(readlink -e "$path" && printf x) || exit 1
  case $resolved in "$root$nl"x|"$root"/*"$nl"x) ;; *) exit 1 ;; esac
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
