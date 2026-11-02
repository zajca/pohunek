---
kind: changed
---
`pohunek assistant` and `cargo xtask docs check` refuse an `assistant/system.md` whose safety or navigation line holds a carriage return that is not part of a CRLF line ending, and name the line, instead of parsing the text after it as part of the same rule.
