#!/bin/sh
# record: --rows 4 --cols 40 --resize 4x40
# Kitty keyboard protocol negotiation: push flags 1, query, pause (the
# recorder's same-size resize marks the moment), pop.
stty -echo
printf '\033[>1u'
printf '\033[?u'
printf 'pushed\n'
printf '\033]7771;resize-point\007'
read -r pause
printf '\033[<u'
printf 'popped\n'
