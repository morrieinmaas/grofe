#!/bin/sh
# What a pane prints for the G3 sixel scenarios (golden/g3/). Cells are
# tmux's default 16x32 pixels, since the scenarios' client reports none.
# usage: g3-sixel.sh <case>

# A sixel image $1 columns wide (in cells) and $2 rows tall: 16 pixels per
# column, and enough six-pixel bands to make the rows (32 pixels each).
img() {
	printf '\033Pq#0;2;100;0;0'
	bands=$(( $2 * 32 / 6 ))
	i=0
	while [ "$i" -lt "$bands" ]; do
		printf '!%d~' $(( $1 * 16 ))
		i=$((i + 1))
		[ "$i" -lt "$bands" ] && printf -- '-'
	done
	printf '\033\\'
}

case $1 in
fit)
	printf '\033[3;5H'
	img 2 2
	printf 'after'
	;;
clear)
	printf '\033[3;5H'
	img 2 2
	printf 'drawn'
	sleep 1
	printf '\033[2J\033[Hcleared'
	;;
overwrite)
	printf '\033[3;5H'
	img 3 2
	printf '\033[3;6HX'
	printf '\033[10;1Hdone'
	;;
scroll)
	printf '\033[7;5H'
	img 2 2
	printf 'bottom'
	sleep 1
	printf '\n\n\n\n\n\n\n\n1\n2\nend'
	;;
bottom)
	printf '\033[10;1H'
	img 2 2
	printf 'last'
	;;
wide)
	printf '\033[2;30H'
	img 4 2
	printf 'right'
	;;
tall)
	printf '\033[1;1H'
	img 2 20
	printf 'tall'
	;;
esac
exec cat
