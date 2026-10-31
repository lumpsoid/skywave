#!/usr/bin/env bash
# Toggle the emulator's network the way a phone loses it.
#   net.sh airplane on|off    # airplane mode (all radios)
#   net.sh wifi on|off        # Wi-Fi only (mobile data takes over if on)
#   net.sh data on|off        # mobile data only
#   net.sh show               # active network
# Env: ANDROID_SERIAL (default emulator-5600).
set -euo pipefail
SERIAL="${ANDROID_SERIAL:-emulator-5600}"
a() { adb -s "$SERIAL" shell "$@"; }
state() { [[ $1 == on ]] && echo enable || echo disable; }
case "${1:-}" in
airplane) a cmd connectivity airplane-mode "$(state "$2")" ;;
wifi) a svc wifi "$(state "$2")" ;;
data) a svc data "$(state "$2")" ;;
show) a dumpsys connectivity | grep -m1 'Active default network' || true; a dumpsys connectivity | grep -o 'ni{[A-Z]*[^ ]* [A-Z]*' | sort -u || true ;;
*) sed -n '2,7p' "$0"; exit 2 ;;
esac
