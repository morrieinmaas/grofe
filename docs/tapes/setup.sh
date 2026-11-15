# Sourced by docs/tapes/grofe.tape inside vhs's shell: a private grofe
# server on $GROFE_SOCK with a fresh home, laid out for the screenshot.
export HOME="$GROFE_DEMO/home" XDG_CONFIG_HOME="$GROFE_DEMO/config" XDG_STATE_HOME="$GROFE_DEMO/state"
export PS1='$ '
unset TMUX TMUX_PANE GROFE GROFE_PANE
g() { grofe -S "$GROFE_SOCK" "$@"; }
run() { sleep 1; g send-keys -t grofe:code "$1" Enter; }
g new-session -d -s grofe -n code -c "$GROFE_REPO"
run 'git --no-pager log --oneline --no-decorate -n 40'
g split-window -h -t grofe:code -c "$GROFE_REPO/src"
run 'ls'
g split-window -v -t grofe:code -c "$GROFE_REPO"
run 'head -n 30 Cargo.toml'
g new-window -d -t grofe -n notes -c "$GROFE_REPO"
g select-pane -t grofe:code -L
clear
