# ssh through foretype to the same server. Render with: vhs demo/foretype.tape
Output demo/foretype.mp4

Set Shell "zsh"
Set FontSize 22
Set Width 820
Set Height 180
Set Padding 26
Set Framerate 30
Set TypingSpeed 55ms
Set CursorBlink false
Set Theme "Catppuccin Mocha"

# Log in off camera with a neutral prompt.
Hide
Type "foretype shell" Enter
Sleep 1s
Type "ssh i" Enter
Sleep 5s
Type ` export PS1='$ '; clear; printf '\e[2mssh + foretype, same server\e[0m\n'` Enter
Sleep 2s
Show

Sleep 600ms
Type `echo "remote made local, no more typing lag"`
Sleep 1200ms
Enter
Sleep 2500ms
