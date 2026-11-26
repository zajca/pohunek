#!/bin/sh
# record: --rows 4 --cols 10
# An OSC 8 hyperlink that spans a soft wrap, followed by unlinked text.
stty -echo
printf '\033]8;;https://example.invalid/a\033\\'
printf '0123456789ABCDEF'
printf '\033]8;;\033\\'
printf 'z\n'
