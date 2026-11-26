#!/bin/sh
# record: --rows 4 --cols 10
# East Asian wide characters, including a wide character that does not fit in
# the last column.
stty -echo
printf 'ab中文cd\n'
printf 'abcdefghi中x\n'
