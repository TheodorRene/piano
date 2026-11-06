#!/bin/sh
# Runs piano off screen, for screenshots: in its own headless sway, with the sound muted
# (PIANO_MUTE: the engine renders at real-time pace into nothing, PipeWire is never opened)
# and scratch settings. Nothing appears on the user's screen and nothing plays.
#
#   tools/gui-headless.sh [-r WxH] [-t seconds] [-s shot-at-seconds] [-b from:to]
#                         [-p 'secs:key …'] [-e 'json'] [-o dir] [binary]
#
# -r the output size (the window tiles to fill it; default 1920x1170 like the user's panel);
# -s when to take the screenshot; -b takes a burst of screenshots every ~45 ms from `from` to
# `to` seconds (burst-NNN.png), for anything that changes too fast for one shot; -p presses
# keys at those times, by gpui's key name: '1:a' presses A, '1.5:^a' lets it go, 'space' is
# the pedal (PIANO_TEST_PRESS: the headless sway has no keyboard); -e adds settings fields.
# The binary defaults to ./target/release/piano, which needs the dev shell: run this under
# `nix develop -c`.
set -eu
res=1920x1170 secs=6 shot=4 extra= out= presses= burst=
while getopts r:t:s:b:e:p:o: opt; do
  case $opt in
    r) res=$OPTARG ;; t) secs=$OPTARG ;; s) shot=$OPTARG ;; b) burst=$OPTARG ;;
    e) extra=$OPTARG ;; p) presses=$OPTARG ;; o) out=$OPTARG ;;
    *) exit 2 ;;
  esac
done
shift $((OPTIND - 1))
bin=$(realpath "${1:-./target/release/piano}")
out=${out:-$(mktemp -d)}
mkdir -p "$out/state/piano"
printf '{ %s }\n' "$extra" > "$out/state/piano/settings.json"

# Its own config, so none of the user's (or /etc/sway's) exec lines run: that one runs
# `systemctl --user import-environment`, which would point the user's session at this
# display. It reports the display it made; sway sets WAYLAND_DISPLAY for its children.
rm -f "$out/display"
cat > "$out/sway.cfg" <<EOF
output HEADLESS-1 mode --custom ${res}@120Hz
default_border none
exec echo \$WAYLAND_DISPLAY > $out/display
EOF
env -u WAYLAND_DISPLAY -u SWAYSOCK -u I3SOCK -u DISPLAY \
  WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1 \
  sway -c "$out/sway.cfg" > "$out/sway.log" 2>&1 &
sway_pid=$!
trap 'kill $sway_pid 2>/dev/null || true' EXIT
for _ in $(seq 50); do [ -s "$out/display" ] && break; sleep 0.1; done
display=$(cat "$out/display")

if [ -n "$burst" ]; then
  from=${burst%:*} to=${burst#*:}
  (sleep "$from"; end=$(echo "$to - $from" | bc); i=0
   t0=$(date +%s.%N)
   while [ "$(echo "$(date +%s.%N) - $t0 < $end" | bc)" = 1 ]; do
     WAYLAND_DISPLAY=$display grim "$out/burst-$(printf %03d $i).png"; i=$((i + 1)); sleep 0.045
   done) &
else
  (sleep "$shot"; WAYLAND_DISPLAY=$display grim "$out/shot.png") &
fi
shot_pid=$!
WAYLAND_DISPLAY=$display PIANO_MUTE=1 PIANO_TEST_PRESS=$presses XDG_STATE_HOME=$out/state \
  timeout "$secs" "$bin" > "$out/app.log" 2>&1 || true
wait $shot_pid
grep -E "panic|rror" "$out/app.log" || true
if [ -n "$burst" ]; then echo "burst: $out/burst-*.png"; else echo "screenshot: $out/shot.png"; fi
