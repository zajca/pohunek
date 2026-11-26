#!/bin/sh
# record: --rows 4 --cols 40
# OSC 52 clipboard write ("hello", base64 aGVsbG8=) and an OSC 52 read request.
stty -echo
printf '\033]52;c;aGVsbG8=\033\\'
printf 'after-write\n'
printf '\033]52;c;?\033\\'
printf 'after-read\n'
