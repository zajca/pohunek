#!/bin/sh
# record: --rows 4 --cols 40
# OSC 7501 program status: the feature probe and a state record print nothing.
stty -echo
printf '\033]7501;?\033\\'
printf '\033]7501;state=working:app=spike\033\\'
printf 'visible\n'
