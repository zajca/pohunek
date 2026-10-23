## Unreleased (2026-10-23, Hermes working screen, #1524)

The Hermes package (1.0.3) reads the staff prompt followed by the interrupt
placeholder (`⚕ ❯ msg=interrupt · /queue · /bg · /steer · Ctrl+C cancel`, the
line Hermes 0.20.0 paints into an empty composer while a turn runs) as
`working`, as the last non-empty line apart from the rule chrome. Before, only
the bare staff prompt matched, so a real working screen was not read as
`working` by the screen rule. The compact layout, which drops the arrow, is
read the same way.
