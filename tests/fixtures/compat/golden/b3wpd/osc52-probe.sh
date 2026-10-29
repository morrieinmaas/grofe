#!/bin/sh
# Asks for the clipboard with an OSC 52 query and shows the bytes the
# terminal sends back. usage: osc52-probe.sh <selection|-> <bel|st>
# ("-" is an empty selection)
stty raw -echo opost onlcr min 0 time 10
sel=$1
[ "$sel" = - ] && sel=
case "$2" in
st) end='\033\134' ;;
*) end='\007' ;;
esac
printf "\033]52;$sel;?$end"
dd bs=1 count=100 2>/dev/null | cat -v
echo
echo done
sleep 100000
