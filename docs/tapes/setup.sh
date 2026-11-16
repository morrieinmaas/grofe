# Sourced by docs/tapes/grofe.tape inside vhs's shell: a private grofe
# server on $GROFE_SOCK with a fresh home, a small demo project and three
# coding-agent panes.
#
# The agents are stand-ins, as in tests/agents.rs: a program run under the
# agent's name (so grofe recognises it, as it does the real one) that prints
# a short transcript, with its state set by `grofe agent report`, the call
# the agents' own hooks make.
export HOME="$GROFE_DEMO/home" XDG_CONFIG_HOME="$GROFE_DEMO/config" XDG_STATE_HOME="$GROFE_DEMO/state"
export PS1='$ '
unset TMUX TMUX_PANE GROFE GROFE_PANE
demo="$HOME/weather"
mkdir -p "$demo/src" "$HOME/bin" "$HOME/transcripts"
cat > "$demo/config.toml" <<'TOML'
# grofe config for the weather project
history-limit = 100000

[keys.prefix]
"|" = "split-window -h -c '#{pane_current_path}'"
"-" = "split-window -v -c '#{pane_current_path}'"
TOML
printf 'fn main() {\n    println!("sunny");\n}\n' > "$demo/src/main.rs"

printf '> make the forecast parser handle missing temperatures\n\n  Read src/main.rs\n  Read cities.csv\n  Edited src/main.rs (+14 -3)\n  Running cargo test ...\n' > "$HOME/transcripts/claude"
printf '> add a /health endpoint\n\n  Wrote src/health.rs\n  Wants to run: cargo test\n' > "$HOME/transcripts/codex"
printf '> update the README for the new endpoint\n\n  Edited README.md\n  Finished.\n' > "$HOME/transcripts/opencode"
for kind in claude codex opencode; do
    printf '#!/bin/bash\ncat "$HOME/transcripts/%s"\nexec -a %s sleep 86400\n' "$kind" "$kind" > "$HOME/bin/$kind"
    chmod +x "$HOME/bin/$kind"
done
export PATH="$HOME/bin:$PATH"

g() { grofe -S "$GROFE_SOCK" "$@"; }
run() { sleep 1; g send-keys -t "$1" "$2" Enter; }
report() { g agent report "$2" -t "$1" --kind "$3" --msg "$4"; }

# Window 1: Claude Code at work beside the config. Pane titles are set so
# nothing shows the machine's hostname.
api=$(g new-session -d -P -F '#{pane_id}' -x 140 -y 42 -s weather -n api -c "$demo")
g select-pane -t "$api" -T claude
run "$api" 'clear; claude'
cfg=$(g split-window -h -P -F '#{pane_id}' -l 40% -t "$api" -c "$demo")
g select-pane -t "$cfg" -T config.toml
run "$cfg" 'vim --clean config.toml'
# Window 2: Codex waiting for permission. Window 3: opencode finished.
web=$(g new-window -d -P -F '#{pane_id}' -t weather -n web -c "$demo")
g select-pane -t "$web" -T codex
run "$web" 'clear; codex'
docs=$(g new-window -d -P -F '#{pane_id}' -t weather -n docs -c "$demo")
g select-pane -t "$docs" -T opencode
run "$docs" 'clear; opencode'
sleep 2
report "$api" working claude 'Running cargo test'
report "$web" blocked codex 'Wants to run: cargo test'
report "$docs" done opencode 'Edited README.md'
g select-window -t weather:api
g select-pane -t "$api"
clear
