# Spike report: screen models over PTY traces

Models: vt100, vt100-scrollback, libghostty

## Assertion outcomes

| model | group | passed | total |
|---|---|---|---|
| libghostty | pohunek-contract | 1 | 1 |
| libghostty | spec | 51 | 51 |
| libghostty | width-method | 7 | 7 |
| vt100 | pohunek-contract | 1 | 1 |
| vt100 | spec | 44 | 51 |
| vt100 | width-method | 4 | 7 |
| vt100-scrollback | pohunek-contract | 1 | 1 |
| vt100-scrollback | spec | 45 | 51 |
| vt100-scrollback | width-method | 4 | 7 |

All groups together (includes width-method and assertions a model cannot observe):

| model | passed | total |
|---|---|---|
| libghostty | 59 | 59 |
| vt100 | 49 | 59 |
| vt100-scrollback | 50 | 59 |

### Basis `spec`

| trace | assertion | vt100 | vt100-scrollback | libghostty |
|---|---|---|---|---|
| claude | enters-alternate-screen | pass | pass | pass |
| claude | leaves-alternate-screen | pass | pass | pass |
| claude | size-stays | pass | pass | pass |
| claude | reply-marker | pass | pass | pass |
| claude | reply-first-word | pass | pass | pass |
| claude | reply-third-word | pass | pass | pass |
| claude | reply-second-paragraph | pass | pass | pass |
| combining | one-mark-joins-base | pass | pass | pass |
| combining | one-mark-next-cell | pass | pass | pass |
| combining | stacked-marks-join-base | pass | pass | pass |
| combining | stacked-marks-next-cell | pass | pass | pass |
| combining | mark-after-wide-joins-base | pass | pass | pass |
| combining | mark-after-wide-next-cell | pass | pass | pass |
| hyperlink-wrap | link-first-cell | FAIL: the model cannot observe OSC 8 hyperlinks | FAIL: the model cannot observe OSC 8 hyperlinks | pass |
| hyperlink-wrap | link-after-wrap | FAIL: the model cannot observe OSC 8 hyperlinks | FAIL: the model cannot observe OSC 8 hyperlinks | pass |
| hyperlink-wrap | link-ends | FAIL: the model cannot observe OSC 8 hyperlinks | FAIL: the model cannot observe OSC 8 hyperlinks | pass |
| hyperlink-wrap | wrap-flag | pass | pass | pass |
| hyperlink-wrap | text-after-link | pass | pass | pass |
| kitty-keyboard | push-sets-flags | FAIL: the model cannot observe the Kitty keyboard flags | FAIL: the model cannot observe the Kitty keyboard flags | pass |
| kitty-keyboard | pop-restores-flags | FAIL: the model cannot observe the Kitty keyboard flags | FAIL: the model cannot observe the Kitty keyboard flags | pass |
| kitty-keyboard | negotiation-prints-nothing | pass | pass | pass |
| osc52 | write-observed | FAIL: the model cannot observe OSC 52 clipboard writes | FAIL: the model cannot observe OSC 52 clipboard writes | pass |
| osc52 | write-prints-nothing | pass | pass | pass |
| osc52 | read-prints-nothing | pass | pass | pass |
| resize-alt-screen | alt-screen-active-at-resize | pass | pass | pass |
| resize-alt-screen | size-after-resize | pass | pass | pass |
| resize-alt-screen | alt-screen-left | pass | pass | pass |
| resize-alt-screen | primary-content-restored | pass | pass | pass |
| resize-alt-screen | cursor-restored-after-1049 | pass | pass | pass |
| resize-wrap | autowrap-sets-wrapped | pass | pass | pass |
| resize-wrap | autowrap-continuation | pass | pass | pass |
| resize-wrap | size-after-shrink | pass | pass | pass |
| resize-wrap | size-after-grow | pass | pass | pass |
| scroll-region | above-region-untouched | pass | pass | pass |
| scroll-region | region-scrolled-up | pass | pass | pass |
| scroll-region | region-second-scrolled-up | pass | pass | pass |
| scroll-region | region-bottom-new-line | pass | pass | pass |
| scroll-region | below-region-untouched | pass | pass | pass |
| scroll-region | last-row-untouched | pass | pass | pass |
| scrollback | scrolled-rows-retained | FAIL: scrollback rows: expected "12", found "0" | pass | pass |
| scrollback | visible-bottom | pass | pass | pass |
| scrollback | size-after-shrink | pass | pass | pass |
| title-progress | latest-title-wins | pass | pass | pass |
| title-progress | progress-payload | pass | pass | pass |
| title-progress | osc-prints-nothing | pass | pass | pass |
| wide-chars | wide-first-cell | pass | pass | pass |
| wide-chars | wide-spacer-tail | pass | pass | pass |
| wide-chars | second-wide-cell | pass | pass | pass |
| wide-chars | narrow-after-wide | pass | pass | pass |
| wide-chars | wide-at-last-column-moved | pass | pass | pass |
| wide-chars | final-cursor | pass | pass | pass |

### Basis `pohunek-contract`

| trace | assertion | vt100 | vt100-scrollback | libghostty |
|---|---|---|---|---|
| program-status | probe-and-record-print-nothing | pass | pass | pass |

### Width-method assertions (not a plain correctness count)

| trace | assertion | vt100 | vt100-scrollback | libghostty |
|---|---|---|---|---|
| emoji-width | family-per-codepoint | pass | pass | pass |
| emoji-width | family-cluster | FAIL: cell text: expected "x", found "👩\u{200d}" | FAIL: cell text: expected "x", found "👩\u{200d}" | pass |
| emoji-width | vs16-heart-cluster | FAIL: cell text: expected "x", found "" | FAIL: cell text: expected "x", found "" | pass |
| emoji-width | vs16-heart-per-codepoint | pass | pass | pass |
| emoji-width | vs15-heart | pass | pass | pass |
| emoji-width | flag-per-codepoint | FAIL: cell text: expected "x", found "" | FAIL: cell text: expected "x", found "" | pass |
| emoji-width | flag-cluster | pass | pass | pass |

## Differences between model pairs

### claude : vt100 vs vt100-scrollback (0 differences)


### claude : vt100 vs libghostty (1 differences)

- record 21 title: Some("") vs None

### claude : vt100-scrollback vs libghostty (1 differences)

- record 21 title: Some("") vs None

### combining : vt100 vs vt100-scrollback (0 differences)


### combining : vt100 vs libghostty (0 differences)


### combining : vt100-scrollback vs libghostty (0 differences)


### emoji-width : vt100 vs vt100-scrollback (0 differences)


### emoji-width : vt100 vs libghostty (23 differences)

- record 1 cell text at row 1 col 0: "👨\u{200d}" vs "👨\u{200d}👩\u{200d}👧"
- record 1 cell text at row 1 col 2: "👩\u{200d}" vs "x"
- record 1 cell width at row 1 col 2: 2 vs 1
- record 1 spacer tail at row 1 col 3: true vs false
- record 1 cell text at row 1 col 4: "👧" vs ""
- record 1 cell width at row 1 col 4: 2 vs 1
- record 1 spacer tail at row 1 col 5: true vs false
- record 1 cell text at row 1 col 6: "x" vs ""
- record 1 cell width at row 2 col 0: 1 vs 2
- record 1 cell text at row 2 col 1: "x" vs ""
- record 1 spacer tail at row 2 col 1: false vs true
- record 1 cell text at row 2 col 2: "" vs "x"
- record 1 cell width at row 5 col 0: 1 vs 2
- record 1 cell text at row 5 col 1: "🇿" vs ""
- record 1 spacer tail at row 5 col 1: false vs true
- record 1 cell text at row 5 col 2: "x" vs "🇿"
- record 1 cell width at row 5 col 2: 1 vs 2
- record 1 spacer tail at row 5 col 3: false vs true
- record 1 cell text at row 5 col 4: "" vs "x"
- record 1 cell text at row 6 col 0: "🇨" vs "🇨🇿"
- record 1 cell width at row 6 col 0: 1 vs 2
- record 1 cell text at row 6 col 1: "🇿" vs ""
- record 1 spacer tail at row 6 col 1: false vs true

### emoji-width : vt100-scrollback vs libghostty (23 differences)

- record 1 cell text at row 1 col 0: "👨\u{200d}" vs "👨\u{200d}👩\u{200d}👧"
- record 1 cell text at row 1 col 2: "👩\u{200d}" vs "x"
- record 1 cell width at row 1 col 2: 2 vs 1
- record 1 spacer tail at row 1 col 3: true vs false
- record 1 cell text at row 1 col 4: "👧" vs ""
- record 1 cell width at row 1 col 4: 2 vs 1
- record 1 spacer tail at row 1 col 5: true vs false
- record 1 cell text at row 1 col 6: "x" vs ""
- record 1 cell width at row 2 col 0: 1 vs 2
- record 1 cell text at row 2 col 1: "x" vs ""
- record 1 spacer tail at row 2 col 1: false vs true
- record 1 cell text at row 2 col 2: "" vs "x"
- record 1 cell width at row 5 col 0: 1 vs 2
- record 1 cell text at row 5 col 1: "🇿" vs ""
- record 1 spacer tail at row 5 col 1: false vs true
- record 1 cell text at row 5 col 2: "x" vs "🇿"
- record 1 cell width at row 5 col 2: 1 vs 2
- record 1 spacer tail at row 5 col 3: false vs true
- record 1 cell text at row 5 col 4: "" vs "x"
- record 1 cell text at row 6 col 0: "🇨" vs "🇨🇿"
- record 1 cell width at row 6 col 0: 1 vs 2
- record 1 cell text at row 6 col 1: "🇿" vs ""
- record 1 spacer tail at row 6 col 1: false vs true

### hyperlink-wrap : vt100 vs vt100-scrollback (0 differences)


### hyperlink-wrap : vt100 vs libghostty (0 differences)


### hyperlink-wrap : vt100-scrollback vs libghostty (0 differences)


### kitty-keyboard : vt100 vs vt100-scrollback (0 differences)


### kitty-keyboard : vt100 vs libghostty (0 differences)


### kitty-keyboard : vt100-scrollback vs libghostty (0 differences)


### osc52 : vt100 vs vt100-scrollback (0 differences)


### osc52 : vt100 vs libghostty (0 differences)


### osc52 : vt100-scrollback vs libghostty (0 differences)


### program-status : vt100 vs vt100-scrollback (0 differences)


### program-status : vt100 vs libghostty (0 differences)


### program-status : vt100-scrollback vs libghostty (0 differences)


### resize-alt-screen : vt100 vs vt100-scrollback (0 differences)


### resize-alt-screen : vt100 vs libghostty (19 differences)

- record 3 cursor: 1,0 vs 2,0
- record 3 cell text at row 0 col 0: "a" vs ""
- record 3 cell text at row 0 col 1: "l" vs ""
- record 3 cell text at row 0 col 2: "t" vs ""
- record 3 cell text at row 0 col 3: "e" vs ""
- record 3 cell text at row 0 col 4: "r" vs ""
- record 3 cell text at row 0 col 5: "n" vs ""
- record 3 cell text at row 0 col 6: "a" vs ""
- record 3 cell text at row 0 col 7: "t" vs ""
- record 3 cell text at row 0 col 8: "e" vs ""
- record 3 cell text at row 1 col 0: "" vs "a"
- record 3 cell text at row 1 col 1: "" vs "l"
- record 3 cell text at row 1 col 2: "" vs "t"
- record 3 cell text at row 1 col 3: "" vs "e"
- record 3 cell text at row 1 col 4: "" vs "r"
- record 3 cell text at row 1 col 5: "" vs "n"
- record 3 cell text at row 1 col 6: "" vs "a"
- record 3 cell text at row 1 col 7: "" vs "t"
- record 3 cell text at row 1 col 8: "" vs "e"

### resize-alt-screen : vt100-scrollback vs libghostty (19 differences)

- record 3 cursor: 1,0 vs 2,0
- record 3 cell text at row 0 col 0: "a" vs ""
- record 3 cell text at row 0 col 1: "l" vs ""
- record 3 cell text at row 0 col 2: "t" vs ""
- record 3 cell text at row 0 col 3: "e" vs ""
- record 3 cell text at row 0 col 4: "r" vs ""
- record 3 cell text at row 0 col 5: "n" vs ""
- record 3 cell text at row 0 col 6: "a" vs ""
- record 3 cell text at row 0 col 7: "t" vs ""
- record 3 cell text at row 0 col 8: "e" vs ""
- record 3 cell text at row 1 col 0: "" vs "a"
- record 3 cell text at row 1 col 1: "" vs "l"
- record 3 cell text at row 1 col 2: "" vs "t"
- record 3 cell text at row 1 col 3: "" vs "e"
- record 3 cell text at row 1 col 4: "" vs "r"
- record 3 cell text at row 1 col 5: "" vs "n"
- record 3 cell text at row 1 col 6: "" vs "a"
- record 3 cell text at row 1 col 7: "" vs "t"
- record 3 cell text at row 1 col 8: "" vs "e"

### resize-wrap : vt100 vs vt100-scrollback (2 differences)

- record 6 scrollback rows: 0 vs 1
- record 7 scrollback rows: 0 vs 2

### resize-wrap : vt100 vs libghostty (89 differences)

- record 3 cursor: 2,0 vs 3,0
- record 3 row wrapped at row 0: false vs true
- record 3 row wrapped at row 1: false vs true
- record 3 cell text at row 1 col 0: "U" vs "K"
- record 3 cell text at row 1 col 1: "V" vs "L"
- record 3 cell text at row 1 col 2: "W" vs "M"
- record 3 cell text at row 1 col 3: "X" vs "N"
- record 3 cell text at row 1 col 4: "Y" vs "O"
- record 3 cell text at row 1 col 5: "Z" vs "P"
- record 3 cell text at row 1 col 6: "0" vs "Q"
- record 3 cell text at row 1 col 7: "1" vs "R"
- record 3 cell text at row 1 col 8: "2" vs "S"
- record 3 cell text at row 1 col 9: "3" vs "T"
- record 3 cell text at row 2 col 0: "" vs "U"
- record 3 cell text at row 2 col 1: "" vs "V"
- record 3 cell text at row 2 col 2: "" vs "W"
- record 3 cell text at row 2 col 3: "" vs "X"
- record 3 cell text at row 2 col 4: "" vs "Y"
- record 3 cell text at row 2 col 5: "" vs "Z"
- record 3 cell text at row 2 col 6: "" vs "0"
- record 3 cell text at row 2 col 7: "" vs "1"
- record 3 cell text at row 2 col 8: "" vs "2"
- record 3 cell text at row 2 col 9: "" vs "3"
- record 6 row wrapped at row 0: false vs true
- record 6 cell text at row 0 col 0: "U" vs "A"
- record 6 cell text at row 0 col 1: "V" vs "B"
- record 6 cell text at row 0 col 2: "W" vs "C"
- record 6 cell text at row 0 col 3: "X" vs "D"
- record 6 cell text at row 0 col 4: "Y" vs "E"
- record 6 cell text at row 0 col 5: "Z" vs "F"
- record 6 cell text at row 0 col 6: "0" vs "G"
- record 6 cell text at row 0 col 7: "1" vs "H"
- record 6 cell text at row 0 col 8: "2" vs "I"
- record 6 cell text at row 0 col 9: "3" vs "J"
- record 6 cell text at row 0 col 10: "" vs "K"
- record 6 cell text at row 0 col 11: "" vs "L"
- record 6 cell text at row 0 col 12: "" vs "M"
- record 6 cell text at row 0 col 13: "" vs "N"
- record 6 cell text at row 0 col 14: "" vs "O"
- record 6 cell text at row 0 col 15: "" vs "P"
- record 6 cell text at row 0 col 16: "" vs "Q"
- record 6 cell text at row 0 col 17: "" vs "R"
- record 6 cell text at row 0 col 18: "" vs "S"
- record 6 cell text at row 0 col 19: "" vs "T"
- record 6 cell text at row 1 col 0: "a" vs "U"
- record 6 cell text at row 1 col 1: "f" vs "V"
- record 6 cell text at row 1 col 2: "t" vs "W"
- record 6 cell text at row 1 col 3: "e" vs "X"
- record 6 cell text at row 1 col 4: "r" vs "Y"
- record 6 cell text at row 1 col 5: "-" vs "Z"
- record 6 cell text at row 1 col 6: "s" vs "0"
- record 6 cell text at row 1 col 7: "h" vs "1"
- record 6 cell text at row 1 col 8: "r" vs "2"
- record 6 cell text at row 1 col 9: "i" vs "3"
- record 6 cell text at row 2 col 0: "n" vs "a"
- record 6 cell text at row 2 col 1: "k" vs "f"
- record 6 cell text at row 2 col 2: "" vs "t"
- record 6 cell text at row 2 col 3: "" vs "e"
- record 6 cell text at row 2 col 4: "" vs "r"
- record 6 cell text at row 2 col 5: "" vs "-"
- record 6 cell text at row 2 col 6: "" vs "s"
- record 6 cell text at row 2 col 7: "" vs "h"
- record 6 cell text at row 2 col 8: "" vs "r"
- record 6 cell text at row 2 col 9: "" vs "i"
- record 6 cell text at row 2 col 10: "" vs "n"
- record 6 cell text at row 2 col 11: "" vs "k"
- record 7 scrollback rows: 0 vs 1
- record 7 cell text at row 0 col 0: "a" vs "U"
- record 7 cell text at row 0 col 1: "f" vs "V"
- record 7 cell text at row 0 col 2: "t" vs "W"
- record 7 cell text at row 0 col 3: "e" vs "X"
- record 7 cell text at row 0 col 4: "r" vs "Y"
- record 7 cell text at row 0 col 5: "-" vs "Z"
- record 7 cell text at row 0 col 6: "s" vs "0"
- record 7 cell text at row 0 col 7: "h" vs "1"
- record 7 cell text at row 0 col 8: "r" vs "2"
- record 7 cell text at row 0 col 9: "i" vs "3"
- record 7 cell text at row 1 col 0: "n" vs "a"
- record 7 cell text at row 1 col 1: "k" vs "f"
- record 7 cell text at row 1 col 2: "" vs "t"
- record 7 cell text at row 1 col 3: "" vs "e"
- record 7 cell text at row 1 col 4: "" vs "r"
- record 7 cell text at row 1 col 5: "" vs "-"
- record 7 cell text at row 1 col 6: "" vs "s"
- record 7 cell text at row 1 col 7: "" vs "h"
- record 7 cell text at row 1 col 8: "" vs "r"
- record 7 cell text at row 1 col 9: "" vs "i"
- record 7 cell text at row 1 col 10: "" vs "n"
- record 7 cell text at row 1 col 11: "" vs "k"

### resize-wrap : vt100-scrollback vs libghostty (90 differences)

- record 3 cursor: 2,0 vs 3,0
- record 3 row wrapped at row 0: false vs true
- record 3 row wrapped at row 1: false vs true
- record 3 cell text at row 1 col 0: "U" vs "K"
- record 3 cell text at row 1 col 1: "V" vs "L"
- record 3 cell text at row 1 col 2: "W" vs "M"
- record 3 cell text at row 1 col 3: "X" vs "N"
- record 3 cell text at row 1 col 4: "Y" vs "O"
- record 3 cell text at row 1 col 5: "Z" vs "P"
- record 3 cell text at row 1 col 6: "0" vs "Q"
- record 3 cell text at row 1 col 7: "1" vs "R"
- record 3 cell text at row 1 col 8: "2" vs "S"
- record 3 cell text at row 1 col 9: "3" vs "T"
- record 3 cell text at row 2 col 0: "" vs "U"
- record 3 cell text at row 2 col 1: "" vs "V"
- record 3 cell text at row 2 col 2: "" vs "W"
- record 3 cell text at row 2 col 3: "" vs "X"
- record 3 cell text at row 2 col 4: "" vs "Y"
- record 3 cell text at row 2 col 5: "" vs "Z"
- record 3 cell text at row 2 col 6: "" vs "0"
- record 3 cell text at row 2 col 7: "" vs "1"
- record 3 cell text at row 2 col 8: "" vs "2"
- record 3 cell text at row 2 col 9: "" vs "3"
- record 6 scrollback rows: 1 vs 0
- record 6 row wrapped at row 0: false vs true
- record 6 cell text at row 0 col 0: "U" vs "A"
- record 6 cell text at row 0 col 1: "V" vs "B"
- record 6 cell text at row 0 col 2: "W" vs "C"
- record 6 cell text at row 0 col 3: "X" vs "D"
- record 6 cell text at row 0 col 4: "Y" vs "E"
- record 6 cell text at row 0 col 5: "Z" vs "F"
- record 6 cell text at row 0 col 6: "0" vs "G"
- record 6 cell text at row 0 col 7: "1" vs "H"
- record 6 cell text at row 0 col 8: "2" vs "I"
- record 6 cell text at row 0 col 9: "3" vs "J"
- record 6 cell text at row 0 col 10: "" vs "K"
- record 6 cell text at row 0 col 11: "" vs "L"
- record 6 cell text at row 0 col 12: "" vs "M"
- record 6 cell text at row 0 col 13: "" vs "N"
- record 6 cell text at row 0 col 14: "" vs "O"
- record 6 cell text at row 0 col 15: "" vs "P"
- record 6 cell text at row 0 col 16: "" vs "Q"
- record 6 cell text at row 0 col 17: "" vs "R"
- record 6 cell text at row 0 col 18: "" vs "S"
- record 6 cell text at row 0 col 19: "" vs "T"
- record 6 cell text at row 1 col 0: "a" vs "U"
- record 6 cell text at row 1 col 1: "f" vs "V"
- record 6 cell text at row 1 col 2: "t" vs "W"
- record 6 cell text at row 1 col 3: "e" vs "X"
- record 6 cell text at row 1 col 4: "r" vs "Y"
- record 6 cell text at row 1 col 5: "-" vs "Z"
- record 6 cell text at row 1 col 6: "s" vs "0"
- record 6 cell text at row 1 col 7: "h" vs "1"
- record 6 cell text at row 1 col 8: "r" vs "2"
- record 6 cell text at row 1 col 9: "i" vs "3"
- record 6 cell text at row 2 col 0: "n" vs "a"
- record 6 cell text at row 2 col 1: "k" vs "f"
- record 6 cell text at row 2 col 2: "" vs "t"
- record 6 cell text at row 2 col 3: "" vs "e"
- record 6 cell text at row 2 col 4: "" vs "r"
- record 6 cell text at row 2 col 5: "" vs "-"
- record 6 cell text at row 2 col 6: "" vs "s"
- record 6 cell text at row 2 col 7: "" vs "h"
- record 6 cell text at row 2 col 8: "" vs "r"
- record 6 cell text at row 2 col 9: "" vs "i"
- record 6 cell text at row 2 col 10: "" vs "n"
- record 6 cell text at row 2 col 11: "" vs "k"
- record 7 scrollback rows: 2 vs 1
- record 7 cell text at row 0 col 0: "a" vs "U"
- record 7 cell text at row 0 col 1: "f" vs "V"
- record 7 cell text at row 0 col 2: "t" vs "W"
- record 7 cell text at row 0 col 3: "e" vs "X"
- record 7 cell text at row 0 col 4: "r" vs "Y"
- record 7 cell text at row 0 col 5: "-" vs "Z"
- record 7 cell text at row 0 col 6: "s" vs "0"
- record 7 cell text at row 0 col 7: "h" vs "1"
- record 7 cell text at row 0 col 8: "r" vs "2"
- record 7 cell text at row 0 col 9: "i" vs "3"
- record 7 cell text at row 1 col 0: "n" vs "a"
- record 7 cell text at row 1 col 1: "k" vs "f"
- record 7 cell text at row 1 col 2: "" vs "t"
- record 7 cell text at row 1 col 3: "" vs "e"
- record 7 cell text at row 1 col 4: "" vs "r"
- record 7 cell text at row 1 col 5: "" vs "-"
- record 7 cell text at row 1 col 6: "" vs "s"
- record 7 cell text at row 1 col 7: "" vs "h"
- record 7 cell text at row 1 col 8: "" vs "r"
- record 7 cell text at row 1 col 9: "" vs "i"
- record 7 cell text at row 1 col 10: "" vs "n"
- record 7 cell text at row 1 col 11: "" vs "k"

### scroll-region : vt100 vs vt100-scrollback (0 differences)


### scroll-region : vt100 vs libghostty (0 differences)


### scroll-region : vt100-scrollback vs libghostty (0 differences)


### scrollback : vt100 vs vt100-scrollback (2 differences)

- record 3 scrollback rows: 0 vs 11
- record 4 scrollback rows: 0 vs 12

### scrollback : vt100 vs libghostty (2 differences)

- record 3 scrollback rows: 0 vs 11
- record 4 scrollback rows: 0 vs 12

### scrollback : vt100-scrollback vs libghostty (0 differences)


### title-progress : vt100 vs vt100-scrollback (0 differences)


### title-progress : vt100 vs libghostty (0 differences)


### title-progress : vt100-scrollback vs libghostty (0 differences)


### wide-chars : vt100 vs vt100-scrollback (0 differences)


### wide-chars : vt100 vs libghostty (1 differences)

- record 1 row wrapped at row 1: false vs true

### wide-chars : vt100-scrollback vs libghostty (1 differences)

- record 1 row wrapped at row 1: false vs true

## Repaint round trips

| trace | model | repaint bytes | differences |
|---|---|---|---|
| claude | vt100 | 139 | 1 |
| claude | vt100-scrollback | 139 | 1 |
| claude | libghostty | 5784 | 0 |
| combining | vt100 | 73 | 0 |
| combining | vt100-scrollback | 73 | 0 |
| combining | libghostty | 5576 | 0 |
| emoji-width | vt100 | 140 | 0 |
| emoji-width | vt100-scrollback | 140 | 0 |
| emoji-width | libghostty | 5651 | 17 |
| hyperlink-wrap | vt100 | 68 | 0 |
| hyperlink-wrap | vt100-scrollback | 68 | 0 |
| hyperlink-wrap | libghostty | 5605 | 0 |
| kitty-keyboard | vt100 | 65 | 0 |
| kitty-keyboard | vt100-scrollback | 65 | 0 |
| kitty-keyboard | libghostty | 5584 | 0 |
| osc52 | vt100 | 74 | 0 |
| osc52 | vt100-scrollback | 74 | 0 |
| osc52 | libghostty | 5593 | 0 |
| program-status | vt100 | 58 | 0 |
| program-status | vt100-scrollback | 58 | 0 |
| program-status | libghostty | 5579 | 0 |
| resize-alt-screen | vt100 | 64 | 0 |
| resize-alt-screen | vt100-scrollback | 64 | 0 |
| resize-alt-screen | libghostty | 5584 | 0 |
| resize-wrap | vt100 | 77 | 0 |
| resize-wrap | vt100-scrollback | 77 | 1 |
| resize-wrap | libghostty | 5610 | 0 |
| scroll-region | vt100 | 70 | 0 |
| scroll-region | vt100-scrollback | 70 | 0 |
| scroll-region | libghostty | 5573 | 0 |
| scrollback | vt100 | 79 | 0 |
| scrollback | vt100-scrollback | 79 | 1 |
| scrollback | libghostty | 5671 | 0 |
| title-progress | vt100 | 58 | 1 |
| title-progress | vt100-scrollback | 58 | 1 |
| title-progress | libghostty | 5579 | 1 |
| wide-chars | vt100 | 78 | 0 |
| wide-chars | vt100-scrollback | 78 | 0 |
| wide-chars | libghostty | 5572 | 0 |
