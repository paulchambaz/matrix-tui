#!/usr/bin/env bash
set -eu

cd "$(dirname "$0")"
sock=mxdemo
cols=120
rows=30
theme=282828,ebdbb2,282828,cc241d,98971a,d79921,458588,b16286,689d6a,a89984,928374,fb4934,b8bb26,fabd2f,83a598,d3869b,8ec07c,ebdbb2
tm() { tmux -L "$sock" -f /dev/null "$@"; }
pause() { sleep "$1"; }
key() { tm send-keys -t demo "$@"; }
say() {
  local i
  for ((i = 0; i < ${#1}; i++)); do
    key -l "${1:$i:1}"
    sleep 0.07
  done
}

drive() {
  pause 3
  say "L"; pause 2.5
  say "J"; pause 2
  say "N"; pause 2.5

  say "f"; say "back"; pause 0.8
  key Enter; pause 2.5

  say "a"; pause 2.5
  say "x"; pause 1.5
  say "5"; pause 2

  say "i"
  say "Reviewed it, looks good to me. Deploying after lunch :rocket:"
  pause 0.7
  key Enter; pause 5
  key Escape; pause 0.5

  say "m"; pause 2
  say "m"; pause 0.5

  say ":status away"; pause 0.5
  key Enter; pause 2

  say ":dm ma"; pause 1
  key Enter; pause 2
  say "i"
  say "Sure, give me ten minutes."
  pause 0.5
  key Enter; pause 1.5
  key Escape; pause 0.5

  say ":server club"; pause 0.5
  key Enter; pause 2.5
  say "L"; pause 2
  say "J"; pause 2.5

  say "?"; pause 3
  key Escape; pause 0.5
  say ":q"
  key Enter; pause 1
}

tm kill-session -t demo 2>/dev/null || true
tm new-session -d -s demo -x "$cols" -y "$rows" target/release/examples/demo
tm set -g escape-time 0
tm set -g status off
(sleep 1; drive) &
asciinema rec --overwrite --window-size "${cols}x${rows}" -c "tmux -L $sock -f /dev/null attach -t demo" demo.cast
wait
agg --font-family "SauceCodePro Nerd Font Mono" --font-size 16 --theme $theme demo.cast demo.gif
