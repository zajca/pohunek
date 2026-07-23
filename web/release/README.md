# Pohunek web control center

This archive contains the complete web control center for Linux x86_64:

- `pohunek-web`, a standalone backend executable with Bun embedded;
- `frontend/`, the built browser application served by that executable;
- `install.sh`, which installs both under the current user's XDG data directory
  and writes a systemd user unit; and
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
