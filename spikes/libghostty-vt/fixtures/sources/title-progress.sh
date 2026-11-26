#!/bin/sh
# record: --rows 4 --cols 40
# OSC 0 and OSC 2 titles, and an OSC 9;4 progress report (state 1, 50 percent).
stty -echo
printf '\033]0;title one\007'
printf '\033]2;title two\033\\'
printf '\033]9;4;1;50\007'
printf 'visible\n'
