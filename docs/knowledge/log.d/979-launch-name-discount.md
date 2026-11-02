## Unreleased (2026-11-02, launch-name echo discount, #979)

- The detector keeps the session name the runtime generation was launched with (`[naming]`) and withholds an OSC title that equals it, or equals it followed by `-`, from manifest evaluation. Screen regions are never filtered, and `session.detection` previews keep the raw title. See [Claude package](guides/claude-package.md#session-name).
