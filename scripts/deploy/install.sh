#!/bin/sh
# Runs on the device, from deploy.sh: installs the skywaved binary and unit it
# uploaded, then starts it or picks up the new binary.
#   usage: sh -s -- now|after-work < install.sh   (over ssh)
#          sh install.sh now|after-work            (deploy.sh --local)
set -eu

case "${1:-}" in
  now | after-work)
    pickup=$1
    ;;
  *)
    echo "install.sh: expected 'now' or 'after-work'" >&2
    exit 2
    ;;
esac

bin="$HOME/.local/bin"
units="$HOME/.config/systemd/user"

# Checked before anything is replaced: a wrong-arch upload fails here.
chmod 755 "$bin/.skywaved.new"
"$bin/.skywaved.new" --version
mv -f "$bin/.skywaved.new" "$bin/skywaved"

unit_changed=0
if cmp -s "$units/.skywaved.service.new" "$units/skywaved.service"; then
  rm -f "$units/.skywaved.service.new"
else
  mv -f "$units/.skywaved.service.new" "$units/skywaved.service"
  systemctl --user daemon-reload
  unit_changed=1
fi

if ! systemctl --user is-enabled -q skywaved; then
  systemctl --user enable skywaved
fi

if ! systemctl --user is-active -q skywaved; then
  systemctl --user start skywaved
elif [ "$pickup" = now ] || [ "$unit_changed" -eq 1 ]; then
  # `skywaved restart` re-execs the path that serve started from, and a changed
  # unit may have changed that path (the old unit ran ~/.cargo/bin), so restart.
  systemctl --user restart skywaved
else
  "$bin/skywaved" restart --after-work
fi

systemctl --user --no-pager status skywaved || true
echo "check: skywaved status"
