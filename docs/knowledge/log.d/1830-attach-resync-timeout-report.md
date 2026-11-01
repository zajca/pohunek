## Unreleased (2026-11-01, attach resync timeout report, #1830)

- The attach scenario `keystrokes_reach_the_child_while_the_attach_stream_reads_no_output_and_resyncs_after` reports, when its final wait times out or the stream reports an error, close or exit frame, the frames read, the gaps and the repaint seen, whether the post-repaint keystroke was sent, the snapshot, output and live byte counts, and the last twelve frame kinds. Test code only; no production behavior changes.
