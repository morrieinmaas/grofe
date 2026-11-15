# The README and site screenshot. scripts/screenshots.sh renders it once per
# theme, filling in @THEME@, @FONT@, @GIF@ and @PNG@; run `just screenshots`.
Output "@GIF@"

Set Shell "bash"
Set FontFamily "@FONT@"
Set FontSize 16
Set Width 1440
Set Height 810
Set Padding 16
Set Theme "@THEME@"
Set TypingSpeed 0

Hide
Type "source docs/tapes/setup.sh" Enter
Sleep 3s
Type "grofe -S $GROFE_SOCK attach -t weather" Enter
Sleep 6s
Show
Sleep 1s
Screenshot "@PNG@"
