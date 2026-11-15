# Sourced by docs/tapes/grofe.tape inside vhs's shell: a private grofe
# server on $GROFE_SOCK with a fresh home and a small demo project.
export HOME="$GROFE_DEMO/home" XDG_CONFIG_HOME="$GROFE_DEMO/config" XDG_STATE_HOME="$GROFE_DEMO/state"
export PS1='$ '
unset TMUX TMUX_PANE GROFE GROFE_PANE
demo="$HOME/weather"
mkdir -p "$demo/src"
cat > "$demo/config.toml" <<'TOML'
# grofe config for the weather project
history-limit = 100000
mouse = true

[status]
right = "#{session_name} %H:%M"
interval = 5

[keys.prefix]
"|" = "split-window -h -c '#{pane_current_path}'"
"-" = "split-window -v -c '#{pane_current_path}'"
"r" = { cmd = "reload", desc = "reload the config" }
TOML
printf '# weather\n\nA small forecast service.\n' > "$demo/README.md"
printf 'fn main() {\n    println!("sunny");\n}\n' > "$demo/src/main.rs"
printf 'city,temp\nLisbon,24\nOslo,9\nTokyo,18\n' > "$demo/cities.csv"

g() { grofe -S "$GROFE_SOCK" "$@"; }
run() { sleep 1; g send-keys -t "$1" "$2" Enter; }
g new-session -d -s weather -n code -c "$demo"
run weather:code 'vim --clean config.toml'
g split-window -h -l 46% -t weather:code -c "$demo"
run weather:code 'ls -R'
g split-window -v -t weather:code -c "$demo"
run weather:code 'column -s, -t cities.csv'
run weather:code 'echo "build ok"'
g new-window -d -t weather -n logs -c "$demo"
g select-pane -t weather:code.1
clear
