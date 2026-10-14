## Unreleased (2026-10-12, notifications watch filters, #858)

- `pohunek notifications watch` accepts `--session <id>`, `--kind <kind>`, and
  `--status <status>` (`--unread` is `--status unread`), with the values of the
  matching `notifications list` flags. The daemon subscription stays global;
  the filters run client-side on the record each `notification_created` or
  `notification_updated` event carries, on every host with `--all-hosts`, in
  human and JSON output. An update that marks an earlier `turn_completed`
  `acknowledged` therefore does not pass `--unread`. While any filter is set,
  `notification_deleted` events are dropped, because they carry only the
  notification id, and `--status deleted` is refused. Without filters every
  event is printed. Documented in the
  `debug-daemon` runbook, the `remote-hosts` guide, `docs/cli.md`, and
  `docs/public-api.md`.
