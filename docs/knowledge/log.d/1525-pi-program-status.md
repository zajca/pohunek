## Unreleased (2026-10-23, Pi 1.4.0 and program status, #1525)

The Pi package (1.0.4) is verified on Pi 1.4.0 and opts in to OSC 7501 program
status: the session worker answers Pi's probe, and Pi's records decide the
activity while a visible dialog still reads `blocked`. Pi 1.0.x keeps working
from the screen alone; its captures stay as the previous release in
`compat/pi/screens/n-1/`.

A session no longer keeps an `osc_progress` activity after the agent that
reported it leaves or is replaced: the activity is cleared (`state_source`
`process`) and the new manifest's reading replaces it (#1550).
