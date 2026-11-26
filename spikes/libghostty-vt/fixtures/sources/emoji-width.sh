#!/bin/sh
# record: --rows 8 --cols 20
# Emoji ZWJ sequence, VS15/VS16 presentation selectors and regional-indicator
# flags, each printed with the grapheme-cluster mode 2027 off and on.
# U+1F468 ZWJ U+1F469 ZWJ U+1F467 is the family sequence, U+2764 U+FE0F the
# emoji-presentation heart, U+2764 U+FE0E the text-presentation heart, and
# U+1F1E8 U+1F1FF a flag.
stty -echo
family='\360\237\221\250\342\200\215\360\237\221\251\342\200\215\360\237\221\247'
heart16='\342\235\244\357\270\217'
heart15='\342\235\244\357\270\216'
flag='\360\237\207\250\360\237\207\277'
printf '\033[?2027l'
printf "${family}x\n"
printf '\033[?2027h'
printf "${family}x\n"
printf "${heart16}x\n"
printf '\033[?2027l'
printf "${heart16}x\n"
printf "${heart15}x\n"
printf "${flag}x\n"
printf '\033[?2027h'
printf "${flag}x\n"
