#!/bin/sh
# record: --rows 6 --cols 20 --resize 8x30
# A resize while the alternate screen is active, then a return to the primary
# screen.
stty -echo
printf 'primary\n'
printf '\033[?1049h'
printf 'alternate\n'
printf '\033]7771;resize-point\007'
read -r pause
printf 'still-alternate\n'
printf '\033[?1049l'
printf 'back\n'
