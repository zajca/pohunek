#!/bin/sh
# record: --rows 4 --cols 20
# Combining marks: one mark, stacked marks, and a mark after a wide character.
# U+0301 is \314\201, U+0300 is \314\200, U+0302 is \314\202.
stty -echo
printf 'e\314\201x\n'
printf 'a\314\200\314\201\314\202x\n'
printf '中\314\201x\n'
