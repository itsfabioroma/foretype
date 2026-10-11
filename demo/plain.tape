# Typing a prompt into a coding agent (pi) on a server ~440 ms away, over plain ssh.
# Render with demo/render.sh, which sets the ssh target and agent command.
# Nothing is ever submitted to the agent: the prompt is typed, never sent.
Output demo/plain.mp4

Set Shell "zsh"
Set FontSize 18
Set Width 960
Set Height 400
Set Padding 18
Set Framerate 30
Set TypingSpeed 45ms
Set CursorBlink false
Set Theme "Catppuccin Mocha"

Hide
Type `clear; printf '\e[1;31m plain ssh\e[0;2m  server ~440 ms away, every key waits for the round trip\e[0m\n'; ssh -t ${=FORETYPE_DEMO_SSH} "$FORETYPE_DEMO_AGENT"` Enter
Sleep 14s
Type "A"
Sleep 2s
Show

Sleep 700ms
Type "dd rate limiting to the login endpoint and write tests for the lockout"
Sleep 3s
