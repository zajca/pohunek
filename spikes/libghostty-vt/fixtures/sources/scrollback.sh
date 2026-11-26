#!/bin/sh
# record: --rows 5 --cols 20 --resize 5x10
# Fifteen lines on a five-row screen (three screen heights), then a resize.
stty -echo
i=1
while [ "$i" -le 15 ]; do
    printf 'line%02d\n' "$i"
    i=$((i + 1))
done
printf '\033]7771;resize-point\007'
read -r pause
printf 'done\n'
