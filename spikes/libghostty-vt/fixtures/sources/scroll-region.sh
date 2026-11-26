#!/bin/sh
# record: --rows 6 --cols 20
# DECSTBM scroll region: a line feed at the bottom margin scrolls only the
# region.
stty -echo
printf 'r1\nr2\nr3\nr4\nr5\nr6'
printf '\033[2;4r'
printf '\033[4;1H'
printf '\nX'
printf '\033[r'
