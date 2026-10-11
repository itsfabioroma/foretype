#!/bin/sh
# Records demo/plain-vs-foretype.gif: one terminal split in two, plain ssh on top and foretype
# below, with tmux sending every keystroke to both panes at the same instant.
#
# The tape runs `ssh devbox`. Point FORETYPE_DEMO_SSH_CONFIG at an ssh config that
# defines it, for example:
#
#   Include ~/.ssh/config
#   Host devbox
#       HostName your.server
#       RequestTTY force
#       RemoteCommand PS1='$ ' exec bash --norc --noprofile -i
#       ControlMaster auto
#       ControlPath /tmp/foretype-demo-%C
#       ControlPersist 180
#
# Needs vhs (brew install vhs), tmux, gifsicle and foretype on PATH.
set -eu
cd "$(dirname "$0")/.."
: "${FORETYPE_DEMO_SSH_CONFIG:?set the ssh config that defines devbox}"
tmux() { command tmux -L foretype-demo "$@"; }

# Connect once up front so the recorded `ssh devbox` does not include the handshake.
ssh -F "$FORETYPE_DEMO_SSH_CONFIG" -o RemoteCommand=none -o RequestTTY=no -fN devbox

tmux kill-server 2>/dev/null || true
tmux -f /dev/null new-session -d -s demo -x 86 -y 15 zsh
tmux split-window -v -t demo foretype shell
tmux set -g status off
tmux set -g pane-border-style fg=colour238
tmux set -g pane-active-border-style fg=colour238
sleep 2
label_0='plain ssh, server ~440 ms away'
label_1='ssh + foretype, same server'
for pane in 0 1; do
  eval "label=\$label_$pane"
  tmux send-keys -t "demo.$pane" \
    "alias ssh='ssh -F $FORETYPE_DEMO_SSH_CONFIG'; PROMPT='%% '; clear; printf '\\e[2m%s\\e[0m\\n' '$label'" Enter
done
sleep 1
tmux select-pane -t demo.1
tmux set -w -t demo synchronize-panes on

vhs demo/foretype.tape
tmux kill-server
ssh -F "$FORETYPE_DEMO_SSH_CONFIG" -O exit devbox 2>/dev/null || true
gifsicle -O3 --lossy=30 --colors 64 demo/recording.gif -o demo/plain-vs-foretype.gif
rm -f demo/recording.gif
ls -lh demo/plain-vs-foretype.gif
