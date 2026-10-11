#!/bin/sh
# Records plain ssh and ssh through foretype to the same server, then stacks them into
# demo/foretype.gif. Needs vhs (brew install vhs), ffmpeg and gifsicle, an ssh host
# named in the tapes, and foretype on PATH.
set -eu
cd "$(dirname "$0")/.."
vhs demo/plain.tape
vhs demo/foretype.tape
# Both recordings start at the same keystroke, so frames line up.
ffmpeg -v error -y -i demo/plain.mp4 -i demo/foretype.mp4 -filter_complex "
  [0:v]pad=iw:ih+4:0:0:color=0x45475a[top];
  [top][1:v]vstack=inputs=2,fps=25,split[a][b];
  [a]palettegen=max_colors=64:stats_mode=diff[p];
  [b][p]paletteuse=dither=none:diff_mode=rectangle" demo/stacked.gif
gifsicle -O3 --lossy=30 demo/stacked.gif -o demo/foretype.gif
rm -f demo/stacked.gif
ls -lh demo/foretype.gif
