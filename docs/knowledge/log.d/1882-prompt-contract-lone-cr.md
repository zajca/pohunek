## Unreleased (2026-11-02, lone carriage return in the assistant prompt contract, #1882)

- The shared prompt-contract parser refuses a line of `assistant/system.md` that holds a carriage return which is not part of a CRLF line ending, because CommonMark ends a line there and the text would render as a second item while parsing as one. Both `cargo xtask docs check` and the launch of `pohunek assistant` name the offending line. See [system.md](assistant/system.md).
