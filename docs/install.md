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
`packaging/install-daemon.sh` wrapper around `pohunek service install`.
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

Every release asset (Linux, macOS, and SDK archives and tarballs, and their
`.sha256` checksum files) has a GitHub build-provenance attestation. Verify a download with:

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
unpack, and put the binaries on your `PATH`.

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

- copies `pohunek`, `pohunekd`, and `pohunek-sessiond` into
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

# 3. Install agent hooks (native session-id capture + notifications)
pohunek integration install

# 4. Start an agent session and attach to it
pohunek session new --agent claude --name "fix-login-bug"
pohunek session list
pohunek attach <session-id>        # Ctrl-] detaches, the agent keeps running
```

For an isolated feature branch, let the daemon create a dedicated worktree:

```bash
pohunek session new --agent codex \
  --repo ~/Code/myapp --branch feat/retry-logic --base-branch main \
  --input "Add retry logic to the API client, then run the tests."
```

## Install the Pi runtime package

Pi (`pi`, the Pi coding agent) is an optional runtime delivered as a runtime
package, not compiled into the daemon. The package declares launch, native
resume and fork, input framing, and detection for Pi `1.0.x`; the supported range
and the verified release are in `compat/pi/compatibility-lock.json`. It needs a
Pi on the daemon's `PATH` (`npm install --global
@earendil-works/pi-coding-agent@1.0.2`, Node 22.19 or newer).

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
  `pi --session <id>` and `pohunek session fork` runs `pi --fork <id>`.
- Pi writes its session file only after the first model reply. A session that
  never got one has nothing to resume, and resume is refused with
  `agent_native_reference_missing` before anything launches. The same code
  answers a session file that was deleted.
- A forked session holds no reference of its own, so it is not resumable.
- Recovery looks for the session file below `PI_CODING_AGENT_DIR` (default
  `~/.pi/agent`) in `sessions/`. A Pi configured with `--session-dir`,
  `PI_CODING_AGENT_SESSION_DIR`, or the `sessionDir` setting stores elsewhere,
  and recovery then fails closed.
- A Pi outside the supported range refuses to launch with
  `agent_runtime_unsupported`; update the package (`pohunek plugin update`) when
  a new Pi range is verified.
