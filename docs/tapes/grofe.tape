# The README and site screenshot. scripts/screenshots.sh renders it once per
# theme, filling in @THEME@, @FONT@, @GIF@ and @PNG@; run `just screenshots`.
Output "@GIF@"

Set Shell "bash"
Set FontFamily "@FONT@"
Set FontSize 16
Set Width 1280
Set Height 720
Set Padding 16
Set Theme "@THEME@"
Set TypingSpeed 0

Hide
Type "source docs/tapes/setup.sh" Enter
Sleep 3s
Type "grofe -S $GROFE_SOCK attach -t grofe" Enter
Sleep 4s
Show
Sleep 1s
Screenshot "@PNG@"
