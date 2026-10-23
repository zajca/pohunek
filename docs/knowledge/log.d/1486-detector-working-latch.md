## Unreleased (2026-10-23, detector working latch, #1486)

A turn whose working title or working frame is replaced by an idle one inside a
single output chunk now publishes `working` before the idle state, so
`session input --until working` and `session new --input-ack-timeout-ms` see a
fast turn. The detector evaluates an agent manifest at each OSC title or
progress event and at each ordered screen observation point of the chunk; the
generic shell manifest is unchanged. Under an agent manifest the terminal
observer now stays armed after startup (`ScreenTracker::resume_observation`).
