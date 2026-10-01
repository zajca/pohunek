# Pohunek web control center

This archive contains the complete web control center for Linux x86_64 or
macOS on Apple Silicon (the archive name says which):

- `pohunek-web`, a standalone backend executable with Bun embedded;
- `frontend/`, the built browser application served by that executable;
- `install.sh`, which installs both under the current user's XDG data directory
  and writes a systemd user unit (Linux) or registers a launchd login agent
  (macOS); and
- `backend.env.example`, the required deployment configuration.

It must run on the same host as a compatible `pohunekd` instance. The backend
uses that daemon's local Unix socket for discovery and only accepts a NetBird
address as its public bind address.

## Install

Unpack the archive and run:

```sh
./install.sh
```

The installer never overwrites an existing backend configuration. Edit the
created `$XDG_CONFIG_HOME/pohunek/backend.env` (or
`~/.config/pohunek/backend.env` when `XDG_CONFIG_HOME` is unset) and set:

```ini
POHUNEK_BACKEND_BIND_HOST=<this host's NetBird address>
POHUNEK_BACKEND_PORT=<chosen TCP port>
```

Then enable and start the user service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now pohunek-backend.service
```

The browser connects to `http://<NetBird address>:<chosen TCP port>/`. Keep the
environment file owner-readable only because it can contain deployment-specific
paths.

To update, unpack a newer archive and run `./install.sh` again. It atomically
replaces the executable and static assets, removes stale assets from the
previous build, updates the user-unit file, and preserves `backend.env`. Reload
and restart the running service afterward:

```sh
systemctl --user daemon-reload
systemctl --user restart pohunek-backend.service
```

## macOS

The `aarch64-apple-darwin` archive needs macOS 14 or newer and an installed
daemon (`packaging/install-daemon.sh` from the daemon archive): the backend's
launchd label carries the daemon installation's namespace, and installing,
updating, or removing the backend never touches the daemon or any session.

```sh
./install.sh
```

creates `~/.config/pohunek/backend.env` (mode `0600`). Set
`POHUNEK_BACKEND_BIND_HOST` and `POHUNEK_BACKEND_PORT`, then run `./install.sh`
again: launchd has no environment file, so the installer copies the supported
`POHUNEK_BACKEND_*` settings from `backend.env` into the agent
`~/Library/LaunchAgents/io.github.zajca.pohunek.<namespace>.backend.plist` and
registers it. Re-run it after every edit of `backend.env`. A setting the
backend does not support, or one the installer manages (`POHUNEK_BACKEND_STATIC_DIR`,
`POHUNEK_BACKEND_LOG_DIR`), is refused. The agent logs to the owner-private
rotating `~/.local/state/pohunek/web-logs/pohunek-backend.jsonl`; a failure
before the configuration loads goes to `launchd.stderr` in the same directory.

Remove the agent and the installed files with `./install.sh --uninstall`; it
keeps `backend.env` and the logs. If Gatekeeper refuses the downloaded
executable, verify it with `codesign --verify --strict pohunek-web`; do not
disable Gatekeeper.
