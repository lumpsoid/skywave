#!/usr/bin/env bash
# Render skywave.svg into every platform's launcher icon set.
# Needs rsvg-convert (librsvg) and magick (ImageMagick). Android 8+ uses the
# vector adaptive icon in res/drawable; the PNGs here are for older launchers.
set -euo pipefail
cd "$(dirname "$0")"
gui=..
src=skywave.svg

# Square art with rounded corners, for platforms that don't mask the icon.
rounded=$(mktemp --suffix=.svg)
trap 'rm -f "$rounded"' EXIT
sed -e 's|<svg |<svg clip-path="url(#r)" |' \
    -e 's|<rect x="-25"|<clipPath id="r"><rect width="100" height="100" rx="22"/></clipPath>&|' \
    "$src" >"$rounded"

png() { rsvg-convert -w "$2" -h "$2" "$1" -o "$3"; }

for d in mdpi:48 hdpi:72 xhdpi:96 xxhdpi:144 xxxhdpi:192; do
  png "$rounded" "${d#*:}" "$gui/android/app/src/main/res/mipmap-${d%%:*}/ic_launcher.png"
done

# iOS masks the icon itself and rejects transparency.
ios=$gui/ios/Runner/Assets.xcassets/AppIcon.appiconset
for f in "$ios"/Icon-App-*.png; do
  name=${f##*/Icon-App-}           # 83.5x83.5@2x.png
  pt=${name%%x*}
  scale=${name##*@}; scale=${scale%x.png}
  png "$src" "$(awk "BEGIN{print int($pt*$scale+0.5)}")" "$f"
done

# macOS: the rounded square on a transparent canvas with the standard margin.
mac=$gui/macos/Runner/Assets.xcassets/AppIcon.appiconset
for s in 16 32 64 128 256 512 1024; do
  inner=$(( s * 824 / 1024 ))
  rsvg-convert -w "$inner" -h "$inner" "$rounded" |
    magick - -background none -gravity center -extent "${s}x${s}" "$mac/app_icon_$s.png"
done

png "$rounded" 256 "$gui/linux/runner/resources/app_icon.png"

ico=$(mktemp -d)
for s in 16 24 32 48 64 256; do png "$rounded" "$s" "$ico/$s.png"; done
magick "$ico"/{16,24,32,48,64,256}.png "$gui/windows/runner/resources/app_icon.ico"
rm -r "$ico"
