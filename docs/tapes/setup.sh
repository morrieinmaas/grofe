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
# Pane titles are set so nothing shows the machine's hostname.
g new-session -d -s weather -n code -c "$demo"
g select-pane -t weather:code -T config.toml
run weather:code 'vim --clean config.toml'
g split-window -h -l 46% -t weather:code -c "$demo"
g select-pane -t weather:code -T shell
run weather:code 'ls -R'
g split-window -v -t weather:code -c "$demo"
g select-pane -t weather:code -T data
run weather:code 'column -s, -t cities.csv'
g new-window -d -t weather -n logs -c "$demo"
g new-scratchpad -t weather:code -c "$demo"
g select-pane -t weather:code -T scratchpad
run weather:code 'wc -l cities.csv src/main.rs'
# A message pill once the tape's client has attached.
(sleep 5; g display-message -d 0 'prefix S hides the scratchpad') >/dev/null 2>&1 &
clear
