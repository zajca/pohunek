## Unreleased (2026-11-02, Pi package session name, #979)

- `[naming] launch_args` accepts a pair form (`["--name", "{session_name}"]`: the whole placeholder token directly after a flag token) besides the joined `--name={session_name}`. In the pair form core omits the group for a name that starts with `-` (WARN with the session id, no name). See [Runtime packages](guides/runtime-packages.md#session-name-arguments).
- Pi package 1.1.0 declares it; Pi reads the name only as the next element of `--name`. Checked from the Pi 1.0.0 and 1.4.0 sources without any model request. See [Pi package](guides/pi-package.md#session-name).
