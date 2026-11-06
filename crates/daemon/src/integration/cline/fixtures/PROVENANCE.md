# Fixture provenance

Hook payloads captured from Cline CLI 3.0.74 by the capture kit of
`pohunekd/plugin-cline` (commit 3fe4b40adf8cef5783707006b30b98cb0811acbb,
`compat/RELEASE` release=3.0.74). Each file is the `payload` object of one
captured hook call on stdin; the captured environment and ancestor lists are not
copied, because the scenarios provide their own.

| Fixture | Captured call |
|---|---|
| root_resume | hooks/assigned-first-turn/001-TaskResume |
| root_start | hooks/generated-id/001-TaskStart |
| root_resume_after_clear | hooks/assigned-after-clear/001-TaskResume |
| root_resume_after_fork | hooks/assigned-after-fork/001-TaskResume |
| root_complete | hooks/assigned-first-turn/002-TaskComplete |
| root_error | hooks/error-400/002-TaskError |
| root_shutdown | hooks/cancel-escape-subagent/006-SessionShutdown |
| subagent_resume | hooks/assigned-subagent/003-TaskResume |
| subagent_complete | hooks/assigned-subagent/005-TaskComplete |
| subagent_resume_escape | hooks/cancel-escape-subagent/003-TaskResume |
| subagent_shutdown | hooks/cancel-escape-subagent/004-SessionShutdown |
| tool_call | hooks/assigned-subagent/002-PreToolUse |
| hub_start | hooks/hub-attach/001-TaskStart |
| hub_resume | hooks/hub-prewarm/001-TaskResume |

Synthetic payloads (not captured; derived by changing one field of a captured
payload): `subagent_error` (`subagent_complete` with `hookName` `agent_error`),
`subagent_abort` (`subagent_shutdown` with `hookName` `agent_abort`),
`root_resume_no_session` (`root_resume` without `sessionContext`),
`root_resume_bad_session` (`root_resume` with a `rootSessionId` outside the id
character set), `malformed_empty` and `malformed_not_json`.
