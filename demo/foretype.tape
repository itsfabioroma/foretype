# One terminal, two panes, the same keystrokes sent to both at the same instant:
# plain ssh on top, foretype below. Render with demo/render.sh, which sets up the panes.
Output demo/recording.gif

Set Shell "zsh"
Set FontSize 20
Set Width 900
Set Height 470
Set Padding 14
Set Framerate 30
Set TypingSpeed 55ms
Set CursorBlink false
Set Theme "Catppuccin Mocha"

Hide
Type "tmux -L foretype-demo attach -t demo" Enter
Sleep 1500ms
Show

Sleep 600ms
Type "ssh devbox"
Sleep 300ms
Enter
Sleep 2s
Type `echo "remote made local, no more typing lag"`
Sleep 1500ms
Enter
Sleep 2500ms
