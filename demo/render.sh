#!/bin/sh
# Records the same prompt typed into a coding agent on a far server, over plain ssh and
# through foretype, then stacks both into demo/foretype.gif.
#
#   FORETYPE_DEMO_SSH='-J jump1,jump2 myserver' \
#   FORETYPE_DEMO_AGENT='cd /tmp/demo && exec pi -ne -ns -np -nc --no-session --offline' \
#   demo/render.sh
#
# Needs vhs (brew install vhs), ffmpeg, gifsicle and foretype on PATH. The labels in the
# tapes state the round trip; measure yours with scripts/latency.py and edit them.
set -eu
cd "$(dirname "$0")/.."
: "${FORETYPE_DEMO_SSH:?set the ssh destination (and any -J jump hosts)}"
: "${FORETYPE_DEMO_AGENT:?set the remote command that starts the agent}"
export FORETYPE_DEMO_SSH FORETYPE_DEMO_AGENT
vhs demo/plain.tape
vhs demo/foretype.tape
# Keep each panel's label line and the agent's input box; drop its startup help.
# Coordinates match the tapes' size settings (960x400, font 18, padding 18).
panel='split[top][box];[top]crop=iw:95:0:0[t];[box]crop=iw:124:0:238[b];[t][b]vstack'
ffmpeg -v error -y -i demo/plain.mp4 -i demo/foretype.mp4 -filter_complex "
  [0:v]${panel},pad=iw:ih+4:0:0:color=0x45475a[slow];
  [1:v]${panel}[fast];
  [slow][fast]vstack,fps=25,split[a][b];
  [a]palettegen=max_colors=64:stats_mode=diff[p];
  [b][p]paletteuse=dither=none:diff_mode=rectangle" demo/stacked.gif
gifsicle -O3 --lossy=30 demo/stacked.gif -o demo/foretype.gif
rm -f demo/stacked.gif
ls -lh demo/foretype.gif
