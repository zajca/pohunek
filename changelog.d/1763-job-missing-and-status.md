---
kind: fixed
---
A `refresh-path` rolling back over a daemon job that vanished now registers the job again from the previous definition instead of failing with `service_rollback_failed` and no way out but `uninstall`; `service_daemon_job_missing` names `pohunek service upgrade --from <prefix>/libexec/pohunek/<active version>`, which registers a missing job again and keeps `service.toml`. `pohunek service status` shows a pending upgrade as `pending upgrade <to> from <previous>` (`pending_transaction.previous_version` in `--json`), and the refusal of a resumed upgrade and of `refresh-path` over a pending transaction name the rollback to the previous version. The hints no longer refer to an issue.
