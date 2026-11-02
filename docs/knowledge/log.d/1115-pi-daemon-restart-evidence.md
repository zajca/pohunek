## Unreleased (2026-11-02, Pi session across a daemon restart, #1115)

- The real-Pi lane gains `a_real_pi_session_resumes_after_a_daemon_restart`: with the locked Pi 1.4.0 a launched session survives a daemon restart (the new daemon adopts the live worker and the earlier turn is still on screen) and, after a stop and a second restart, resumes from Pi's session file under the same reference. Only the first turn reaches a model, and that model is the loopback stub; adoption and resume make no request. See [pi-package.md](guides/pi-package.md).
