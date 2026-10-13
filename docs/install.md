# Install pohunek manually

The [README](../README.md#install-with-an-agent) has a prompt that lets a
coding agent run these steps for you. This page is the manual path: release
archives, the login service, building from source, and a first session. The
[setup guide](knowledge/guides/setup.md) covers configuration and `pohunek
doctor` checks in depth; the [macOS runbook](knowledge/runbooks/install-on-macos.md)
covers Homebrew, Gatekeeper, upgrade, and removal on a Mac.

## Release archives

Each release publishes `pohunek-cli-*` and `pohunek-daemon-*` archives for
x86_64 Linux with both glibc and MUSL. Every
archive contains its license, the README, the reference pages under `docs/`
(features, install, CLI, SDK, development), and the offline knowledge
documentation under `docs/offline/`.
Daemon archives contain `pohunekd`, `pohunek-sessiond`, `pohunek`, and the
`packaging/install-daemon.sh` wrapper around `pohunek service install`, plus
`runtime-catalog-anchor.json`, the runtime-catalog trust anchor that the daemon
reads from the directory of its executable. CLI and relay archives do not
contain it. A release daemon archive also carries a `runtime/` directory with
the signed `runtime-catalog.json` and, for the targets with attested official
runtime packages, `runtime/packages/<runtime>.tar.zst` and
`runtime/attestations/<runtime>-<target>.json` (the Apple Silicon archive carries the
catalog and no package). The `MANIFEST` covers these files.
Every CLI, daemon, and relay archive is packed deterministically
(members sorted, root-owned, stamped with the tagged commit time) and carries a
`MANIFEST` with the SHA-256 of every member; the daemon installer verifies
it, the host OS and architecture, and member permissions before they run or
change anything.

macOS on Apple Silicon (macOS 14 or newer) is not yet a published platform:
public macOS support is declared only when the final native acceptance gate
(#105) passes. Each release already publishes
`pohunek-cli-<v>-aarch64-apple-darwin.tar.gz` and
`pohunek-daemon-<v>-aarch64-apple-darwin.tar.gz`, each with a `.sha256`. They are
ad-hoc signed (the `MANIFEST` says `signing adhoc`), not notarized, and carry no
Developer ID. A build made with `packaging/macos/package --development` is
unsigned, named `...-unsigned-development`, and never released. The install,
upgrade, rollback, uninstall, log, logout/reboot, and Gatekeeper procedures are
in the [macOS install runbook](knowledge/runbooks/install-on-macos.md).

Every release asset (Linux, macOS, and SDK archives and tarballs, the runtime
package archives, `runtime-catalog.json`, the attestation documents,
`release-inventory.sha256`, and the `.sha256` checksum files) has a GitHub
build-provenance attestation. `release-inventory.sha256` lists every other asset
of the release in `sha256sum -c` format. A release becomes visible only after all
of them were uploaded and verified. Verify a download with:

```bash
gh attestation verify <file> --repo zajca/pohunek
```

Install on macOS with Homebrew:

```bash
brew install zajca/pohunek/pohunek    # tap zajca/homebrew-pohunek, formula pohunek
pohunek service install
```

Run `pohunek service upgrade` after every `brew upgrade pohunek`, and
`pohunek service uninstall` before `brew uninstall pohunek`. The formula does not
touch launchd itself.

Or install from the archive: download the daemon archive with `curl -fLO` (curl
sets no quarantine attribute), check it with `shasum -a 256 -c` and
`gh attestation verify`, extract it, and run `./packaging/install-daemon.sh`.
A file downloaded through a browser gets `com.apple.quarantine`, and Gatekeeper
blocks it because it is not notarized; see the runbook before removing the
attribute. After an upgrade macOS may ask again whether `pohunek` may access its
`pohunek-relay` Keychain items, because an ad-hoc signature has no stable
designated requirement.

On Linux, download from [Releases](https://github.com/zajca/pohunek/releases),
unpack, and install the daemon archive with `packaging/install-daemon.sh` (see
below). If you copy the binaries onto your `PATH` by hand instead, copy
`runtime-catalog-anchor.json` next to `pohunekd` as well: the daemon reads the
anchor only from the directory of its own executable, and without it official
runtime packages are refused with `official_trust_unavailable`.

Protocol v2 was a one-time coordinated pre-1.0 boundary. Before that M1
transition, every CLI, SDK, custom client, and local or remote
daemon had to cross together. The legacy integer-v1 envelope and fixed
`codex`/`claude` notification-policy fields have no compatibility shim. Once a
fleet is on v2, peers negotiate their highest overlap: M2 and this M3 plugin do
not raise the public protocol version or require a second coordinated boundary.
Do not binary-downgrade a host after it has persisted Hermes enum values or the
provider-keyed notification policy; recover by upgrading forward instead.

For the daemon component, run the included `packaging/install-daemon.sh`. It
calls `pohunek service install` (or `pohunek service upgrade` when a service is
already installed and no interrupted install is pending), which:

- copies `pohunek`, `pohunekd`, `pohunek-sessiond`, and the archive's
  `runtime-catalog-anchor.json` (mode `0644`; a build without one installs
  without it, and an anchor that is not valid is refused with
  `service_staged_anchor_invalid`) into
  `<prefix>/libexec/pohunek/<version>/` and installs `<prefix>/bin/pohunek`; a
  fresh install uses `POHUNEK_INSTALL_PREFIX` (default `~/.local`), an upgrade
  keeps the prefix recorded in `service.toml` and refuses a different
  `POHUNEK_INSTALL_PREFIX`;
- writes `~/.config/pohunek/service.toml` (mode `0600`) with every deadline,
  the agent environment allowlist, and the installation namespace;
- registers the daemon as the only login service: the systemd user unit
  `pohunek-<ns>-daemon.service` and slice `pohunek-<ns>-sessions.slice` on Linux
  (systemd 255 or newer), or the launchd agent
  `~/Library/LaunchAgents/io.github.zajca.pohunek.<ns>.daemon.plist` on macOS;
- waits until the daemon job's own process answers on the socket, and journals
  every step in `~/.local/state/pohunek/service-install.json` so an interrupted
  run resumes or rolls back (an install that fails from the registration step on
  is kept pending for a rerun or `pohunek service uninstall` instead).

The daemon starts each session worker as its own native job per worker
generation (a systemd transient unit, or a launchd job whose definition stays in
`~/.local/state/pohunek/launchd/`), so an upgrade restarts only the daemon while
live workers keep running from their versioned directory. Agents receive only
an allowlisted base environment (`[environment] allowlist` in `service.toml`)
rather than the daemon's whole environment. `pohunek service uninstall` refuses
while sessions are live unless `--stop-sessions` is given, and keeps durable
metadata unless `--purge` is given.

`pohunek service upgrade` first runs a read-only preflight with the new
binaries and refuses while a live session would lose its recovery or would not
be adopted by the new daemon (`service_upgrade_sessions_at_risk`, listing each
session with a reason code). Passing `--accept-runtime-loss` proceeds after you
have reviewed that list; a store the new daemon would refuse to start with is
never overridable. See the
[update runbook](knowledge/runbooks/update-after-release.md).

The first upgrade from a legacy daemon-owned PTY release refuses live sessions
by default because those open PTYs cannot be transferred. Let them finish; use
`--accept-runtime-loss` only after reviewing the affected ids and knowingly
accepting the destructive boundary. An install that still runs the older
`pohunek-session@` template workers is retired only after the wrapper has
closed new connections to the legacy daemon, run the migration preflight, and
re-checked every not-inactive worker state after the daemon stopped; any
detected worker aborts the installer without removing legacy files. The whole
run holds the service transaction lock (`pohunek service lock`), and
`pohunek service check` first confirms that the final install or upgrade would
accept `HOME`, the XDG roots, the prefix, and every directory it writes. See the
[migration guide](migrations/durable-session-workers.md) and
[operations runbook](runbooks/durable-session-workers.md).

Or build from source (Rust 1.96+):

```bash
git clone https://github.com/zajca/pohunek.git
cd pohunek
cargo build --release --locked \
  --bin pohunek --bin pohunekd --bin pohunek-sessiond
```

## Quick start

```bash
# 1. Check the environment (binaries, socket paths, writable state dirs)
pohunek doctor

# 2. Install the host daemon as a login service. Run the pohunek that sits next
#    to pohunekd and pohunek-sessiond (unpacked daemon archive or target/release);
#    --from defaults to its directory.
./target/release/pohunek service install
pohunek service status
pohunek health
pohunek host governance inspect local --json

# 3. Install the official Claude Code package from the extracted daemon
#    archive, then its hooks (native session-id capture + notifications); see
#    "Install official runtime packages from the release archive" below.
pohunek plugin install runtime/packages/claude.tar.zst --catalog runtime/runtime-catalog.json --yes
pohunek integration install --agent claude

# 4. Start an agent session and attach to it
pohunek session new --agent claude --name "fix-login-bug"
pohunek session list
pohunek attach <session-id>        # Ctrl-] detaches, the agent keeps running
```

Claude Code, Codex and Hermes are served only by their official packages, so
step 3 installs the Claude Code package before its hooks: a bare `integration install`
covers only the runtimes the daemon serves. The packages capture the native
session id through those hooks, so without them a session cannot be resumed.
`--agent claude` needs Claude Code's config home (`CLAUDE_CONFIG_DIR`, else
`~/.claude`) to exist, which Claude Code creates on its first run. The Apple
Silicon archive carries no runtime package yet (#929), so on macOS steps 3 and 4
need a package runtime you install yourself.

For an isolated feature branch, let the daemon create a dedicated worktree. To
run Codex there, install its package and then its hooks the same way;
`--agent codex` needs Codex's config home (`CODEX_HOME`, else `~/.codex`) to
exist, which Codex creates on its first run:

```bash
pohunek plugin install runtime/packages/codex.tar.zst --catalog runtime/runtime-catalog.json --yes
pohunek integration install --agent codex
pohunek session new --agent codex \
  --repo ~/Code/myapp --branch feat/retry-logic --base-branch main \
  --input "Add retry logic to the API client, then run the tests."
```

## Install official runtime packages from the release archive

Each Linux daemon archive carries the official runtime packages of its release
and the signed catalog that authorizes them:

```text
pohunek-daemon-<version>-<target>/
  runtime-catalog-anchor.json        trust anchor, installed beside pohunekd
  runtime/runtime-catalog.json       signed catalog of this release
  runtime/packages/pi.tar.zst        official Pi package
  runtime/packages/codex.tar.zst     official Codex package
  runtime/packages/claude.tar.zst    official Claude Code package
  runtime/packages/hermes.tar.zst    official Hermes Agent package
  runtime/attestations/<runtime>-<target>.json
```

The same catalog and package archives are also published as separate release
assets (`runtime-catalog.json`, `pohunek-runtime-<runtime>-<version>.tar.zst`).
Every catalog entry pins its package to exactly the core version of its release
(`core = "=<version>"`). Current releases list only the Linux x86_64 glibc and musl
targets; the Apple Silicon archive carries the catalog and no package.

1. Install or upgrade the daemon from this same archive with
   `./packaging/install-daemon.sh`. A catalog from a release other than the
   running daemon's is refused with `package_incompatible`, or with
   `package_untrusted` when it is older than a catalog this host has already
   accepted.
2. Run `pohunek doctor` and check that `catalog_trust_anchor` is `ok`. `warn`
   means the daemon has no anchor (`official_trust_unavailable`), `fail` means
   the anchor cannot be trusted (`official_trust_anchor_invalid`); see the
   [runtime package recovery runbook](knowledge/runbooks/runtime-package-recovery.md).
3. From the extracted archive directory, review what a package declares. The
   command prints the package id, version, digest, trust origin and the program
   and arguments the daemon will launch as you, then fails with
   `consent_required`; nothing is installed:

   ```bash
   cd pohunek-daemon-<version>-<target>
   pohunek plugin install runtime/packages/pi.tar.zst --catalog runtime/runtime-catalog.json
   ```

4. Install it by repeating the command with `--yes`, and check the result:

   ```bash
   pohunek plugin install runtime/packages/pi.tar.zst --catalog runtime/runtime-catalog.json --yes
   pohunek plugin list
   pohunek host inspect local --json
   ```

The CLI resolves both paths and the daemon on this machine reads them; the
command is local only and refuses a remote `--host`. The daemon verifies the
catalog signature against its anchor, refuses a catalog older than the newest
one it has accepted, and requires an entry that binds this package id, runtime
id, version and digest for this core version and platform. The package is
extracted into the owner-private package store and launches never read the
archive again; keep the archive and catalog (or the release assets): restoring a damaged
installation needs the same files, and an official archive can be restored only
while the daemon still runs the release whose catalog authorized it. The upstream agent itself is not part of the package: it
must be on the daemon's `PATH` in the range the package declares (see the
[Pi](knowledge/guides/pi-package.md), [Codex](knowledge/guides/codex-package.md),
[Claude Code](knowledge/guides/claude-package.md) and
[Hermes](knowledge/guides/hermes-package.md) package guides), or the
runtime reports itself unsupported.

`codex`, `claude` and `hermes` are reserved runtime ids that only a
catalog-authorized package may serve. The daemon compiles none of Codex, Claude
Code and Hermes in, so each is available only once its package is installed:

```bash
pohunek plugin install runtime/packages/claude.tar.zst --catalog runtime/runtime-catalog.json --yes
pohunek integration install --agent claude
pohunek plugin install runtime/packages/codex.tar.zst --catalog runtime/runtime-catalog.json --yes
pohunek integration install --agent codex
pohunek plugin install runtime/packages/hermes.tar.zst --catalog runtime/runtime-catalog.json --yes
```

Each `integration install` installs that runtime's hooks into its config home
(`CLAUDE_CONFIG_DIR`, else `~/.claude`; `CODEX_HOME`, else `~/.codex`; the
directory must exist); the package takes the native session id from them, so a
session without them cannot be resumed. A bare `pohunek integration install`
run before a package was installed did not cover its runtime. Until the package
is installed `pohunek session new --agent claude` (or `codex`) fails with
`agent_profile_not_found`, a host profile with `base = "claude"` (or `codex`)
and no package pin fails with `runtime_not_installed`, and `pohunek integration
install --agent claude` (or `codex`) fails with `runtime_not_installed`; each
recovery hint names the matching install command. Hooks pohunek installed
earlier stay visible and removable without the package (`pohunek integration
status|uninstall --agent claude`, or `--agent codex`). Without its package
`pohunek session new --agent hermes` fails with `agent_profile_not_found` and a
host profile with `base = "hermes"` and no package pin with
`runtime_not_installed`, with the same install hints. The Hermes operator plugin
lifecycle (`pohunek integration ... --agent hermes`) runs in the CLI against a
named Hermes profile; see the
[Hermes operator](knowledge/guides/hermes-operator.md) guide. The catalog lists
the three packages for the Linux x86_64 targets only and the Apple Silicon
archive carries no package, so a macOS daemon has no source of Codex, Claude
Code or Hermes until darwin packages are attested (#929). A `claude`, `codex` or
`hermes` started in a shell session or outside pohunek is not recognized as that
agent without its package.

A session launched from the built-in Claude Code, Codex or Hermes of an earlier
release is not served by this daemon. Without the package every interaction with
it (input, attach, resize, rename, metadata) is refused too
(`runtime_builtin_removed`; only stop and remove work), so its work cannot be
finished until the package is installed. With the official package installed,
the package adopts the session, live or stopped, whatever its version, and
serves its input again. Resume and fork then run through the package, and the
relaunched run is pinned to the package digest. A session whose recovery the
package cannot honor (its native reference kind, a hook-reported reference, or
fork support for a fork), and a session record of v0.33.0 that names no package,
answer `runtime_served_by_package`, and its work continues in a new session. A
fork of a Hermes session answers `agent_fork_unsupported`, because Hermes has no
native fork. Install the Claude Code and
Codex packages on the earlier release before upgrading so that live sessions
stay usable. No earlier release archive carries the Hermes package, so finish or
stop built-in Hermes sessions before upgrading; the upgrade preflight reports a
live one at risk, and after an upgrade with `--accept-runtime-loss` installing
the Hermes package from this release's archive lets you finish its work. On
the earlier release the first installed version of a package is enabled,
selected and takes the id over from that release's built-in runtime at once,
and a host agent profile on that base must be pinned to the package with
`pohunek plugin profile migrate <name> --yes` (without `--yes` it only previews
the change); until then its launches fail with `invalid_profile`. The same
migration pins a profile over `claude`, `codex` or `hermes` on this release once
its package is installed.

The Hermes operator plugin comes only from the Hermes package as well: the CLI
carries no copy of its payload, so install the package before the plugin:

```bash
pohunek plugin install runtime/packages/hermes.tar.zst --catalog runtime/runtime-catalog.json --yes
pohunek integration install --agent hermes --hermes-profile default \
  --access-mode manage --allow-host local
```

Without the package, `pohunek integration install|update --agent hermes` fails
with `hermes_package_not_installed` and changes nothing; its recovery hint names
the package install. A plugin an earlier CLI installed from its compiled copy
(`provenance` `source=embedded`) is not migrated automatically: status and
doctor report the missing package, `integration uninstall` still removes it,
and `integration update` moves it to the package once the package is
installed. The Apple Silicon archive carries no package, so a macOS host cannot
install the plugin until darwin packages are attested (#929).

To prepare first, install with `--no-enable` and run
`pohunek plugin enable <package-id>` when you are ready to switch. `pohunek
plugin disable` or `pohunek plugin uninstall` leaves `claude`, `codex` or
`hermes` unserved. A daemon upgrade does not change installed packages; they keep
serving until you run `pohunek plugin update <package> runtime/packages/<runtime>.tar.zst
--catalog runtime/runtime-catalog.json` from the new release's archive. A plain
`install` of another version leaves it installed but unselected.
The trust model and the package classes are described
in [the architecture](architecture.md#runtime-packages-and-trust) and the
[runtime packages guide](knowledge/guides/runtime-packages.md).

## Install the Pi runtime package from a checkout

Pi (`pi`, the Pi coding agent) is an optional runtime delivered as a runtime
package, not compiled into the daemon. The package declares launch, native
resume and fork, input framing, and detection for Pi `1.0.x`; the supported range
and the verified release are in `compat/pi/compatibility-lock.json`. It needs a
Pi on the daemon's `PATH` (`npm install --global
@earendil-works/pi-coding-agent@1.0.2`, Node 22.19 or newer). A release install
takes the official package from the daemon archive as above; a checkout builds
the archive and trusts it by its digest instead.

```bash
# From a checkout: build the archive and note the digest it prints.
cargo xtask package build runtime-packages/pi --output pi.tar.zst

# Review what it declares, then install it with that exact digest (local trust,
# never official). Repeat with --yes to install.
pohunek plugin install ./pi.tar.zst --sha256 sha256:<digest-printed-above>
pohunek plugin install ./pi.tar.zst --sha256 sha256:<digest-printed-above> --yes

pohunek host inspect local --json     # the pi runtime reports its version and `supported`
pohunek session new --agent pi --name "refactor-parser" --input "Refactor the parser."
```

- Pi chooses its own conversation file from the session id the daemon passes
  at launch (`pi --session-id <id>`), so `pohunek session resume` runs
  `pi --session <id>` and `pohunek session fork` runs
  `pi --fork <id> --session-id <fork id>` under a second id the daemon
  generates for the fork.
- Pi writes its session file only after the first model reply. A session that
  never got one has nothing to resume, and resume is refused with
  `agent_native_reference_missing` before anything launches. The same code
  answers a session file that was deleted.
- A forked session holds its own reference from the start: Pi writes the
  fork's session file `<time>_<fork id>.jsonl` at once, before any model
  reply, so `pohunek session resume` reopens the fork with
  `pi --session <fork id>`. Forks get their own reference from Pi package
  1.0.1 on; a session launched from package 1.0.0 keeps forks without one.
- Recovery looks for the session file below `PI_CODING_AGENT_DIR` (default
  `~/.pi/agent`) in `sessions/`, or directly in `PI_CODING_AGENT_SESSION_DIR`
  when the profile `[env]` (or the daemon's environment) sets it to an absolute
  path. From Pi package 1.0.2 on, a launch whose Pi would keep its sessions
  elsewhere (`--session-dir` in the profile `args`, or a `sessionDir` setting
  while that variable is unset) is refused with `agent_native_store_relocated`;
  set `PI_CODING_AGENT_SESSION_DIR` to that directory instead.
- A Pi extension dialog, the project trust prompt and the login steps read as
  `blocked`; the model, session and other selectors are never read as `idle`.
- A Pi outside the supported range refuses to launch with
  `agent_runtime_unsupported`; update the package (`pohunek plugin update`) when
  a new Pi range is verified.
