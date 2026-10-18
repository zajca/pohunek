## Unreleased (2026-10-18, wrapper version-probe guidance, #1196)

Launch refusals and doctor now explain that a wrapper program must answer its
runtime package's version-probe arguments without the profile's fixed arguments.
Unparseable output, failed probes and releases outside the supported range keep
their distinct diagnostics; raw probe output is never included. Probe execution,
isolation, limits and executable resolution are unchanged. The
[profile guide](concepts/agent-profiles.md#wrapper-programs-and-version-probes)
shows conforming direnv-style and Python bridge wrappers for new launches and
resumes.
