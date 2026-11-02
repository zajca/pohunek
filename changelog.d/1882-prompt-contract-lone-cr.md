---
kind: changed
---
`pohunek assistant` and `cargo xtask docs check` refuse an `assistant/system.md` that holds a carriage return which is not part of a CRLF line ending, on any line, and name the line, instead of parsing the text after it as part of the same rule.
