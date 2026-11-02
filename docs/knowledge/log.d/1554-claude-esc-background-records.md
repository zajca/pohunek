## Unreleased (2026-11-02, Claude program-status scenarios, #1554)

- Real-Claude scenarios on 2.1.309 record the Esc interrupt (a root `idle` record, with `Interrupted` once the reply began) and the `id=` records of a background agent (a child `working` with a `title`, then a child `clear`; the root sequence is unchanged). See [Claude package](guides/claude-package.md#program-status-osc-7501).
- The Claude package's `live_status_spinner` rule also reads a status line whose timer the terminal width cut (`✻ Billowing… (1…`) as working. A parenthesis that opens with a non-digit still does not count; the limit is stated in the guide.
