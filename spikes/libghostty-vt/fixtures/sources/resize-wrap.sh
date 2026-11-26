#!/bin/sh
# record: --rows 4 --cols 20 --resize 4x10 --resize 4x20
# A soft-wrapped line on the primary screen, shrunk and grown back.
stty -echo
printf 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123\n'
printf '\033]7771;resize-point\007'
read -r pause
printf 'after-shrink\n'
printf '\033]7771;resize-point\007'
read -r pause
printf 'after-grow\n'
