#!/usr/bin/env bash
# Regenerate the README screenshots from synthetic data. Local use only: CI
# never runs this, and the images are not compared pixel by pixel.
#
# It runs crates/slashit-acceptance/tests/readme_screenshots.rs on a private
# display through run-desktop-acceptance.sh, against the application built by
# scripts/build-acceptance-app.sh (run that first). The test writes to
# target/readme-screenshots/. Only after it passes are the images copied into
# docs/assets/screenshots/, so a failed run changes nothing that is tracked.
#
#   scripts/capture-readme-screenshots.sh           capture and install
#   scripts/capture-readme-screenshots.sh --no-install   capture only
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

install=1
case ${1:-} in
  "") ;;
  --no-install) install=0 ;;
  -h | --help)
    sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *)
    echo "capture-readme-screenshots: unknown argument: $1" >&2
    exit 1
    ;;
esac

out=target/readme-screenshots
images=(pr-comment-review.png dashboard-kanban.png)
rm -rf "$out"

scripts/run-desktop-acceptance.sh --readme-screenshots

for image in "${images[@]}"; do
  [[ -s $out/$image ]] || {
    echo "capture-readme-screenshots: the run produced no $out/$image" >&2
    exit 1
  }
done

if ((install)); then
  for image in "${images[@]}"; do
    cp "$out/$image" "docs/assets/screenshots/$image"
  done
  echo "installed: ${images[*]} -> docs/assets/screenshots/"
else
  echo "captured to $out/ (not installed)"
fi
