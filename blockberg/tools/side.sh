#!/usr/bin/env bash
# tools/side.sh GEOM out.png [scale]  -> mockup crop above render crop
d=${BLOCKBERG_COMPARE_DIR:-/tmp/blockberg-compare}; cd "$(dirname "$0")"
convert reference/mockup.png -crop "$1" +repage $d/_a.png
convert $d/shot.png -crop "$1" +repage $d/_b.png
convert $d/_a.png $d/_b.png -append -scale "${3:-100}%" "$2"
