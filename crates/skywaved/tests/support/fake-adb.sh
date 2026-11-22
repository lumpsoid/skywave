#!/bin/sh
# A fake adb for the screen runner tests (tests/screen.rs). It keeps its state
# beside itself: every call is appended to calls.log, the screen is screen.png,
# and flag files change what it does: fail-capture, fail-input, hold,
# no-devices, and next.png, which becomes the screen after an input.
set -eu
dir=$(dirname "$0")
echo "$*" >> "$dir/calls.log"

case "$*" in
    "devices -l")
        echo "List of devices attached"
        if [ ! -e "$dir/no-devices" ]; then
            echo "emulator-5554          device product:sdk model:Pixel_7 device:emu64a transport_id:1"
        fi
        echo ""
        ;;
    "-s "*" exec-out screencap -p")
        if [ -e "$dir/fail-capture" ]; then
            echo "error: device offline" >&2
            exit 1
        fi
        cat "$dir/screen.png"
        ;;
    "-s "*" shell input "*)
        while [ -e "$dir/hold" ]; do
            sleep 0.05
        done
        if [ -e "$dir/fail-input" ]; then
            echo "error: device offline" >&2
            exit 1
        fi
        if [ -e "$dir/next.png" ]; then
            mv "$dir/next.png" "$dir/screen.png"
        fi
        ;;
    "-s "*" install -r "*)
        echo "Success"
        ;;
    "-s "*" shell am "*|"-s "*" shell monkey "*)
        ;;
    *)
        echo "fake-adb: unexpected $*" >&2
        exit 1
        ;;
esac
