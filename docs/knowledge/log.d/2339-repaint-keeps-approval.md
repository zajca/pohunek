## Unreleased (2026-11-14, a repaint keeps a record-backed approval open, #2339)

- After a record-backed `blocked` opened an approval, a `Working/Process` edge
  (output no detection rule names) no longer resolves it. Such output appears
  only once the record is gone (a root `clear`, a lag resync, a reconfigure),
  because a held record suppresses the process edge, and on a runtime that
  reports its own status it is a repaint, the same rule `session.input`
  already applies to process-sourced working. The approval stays unread
  until a working edge from the program's status channel or a screen rule,
  which resolves it also straight after the repaint's own `Working`. Before,
  the outcome after a root `clear` depended on how the terminal chunked the
  redraw: a single-chunk redraw left the approval unread, a split one
  acknowledged it. See the architecture section on OSC 7501 program status.
