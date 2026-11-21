#!/bin/sh
# Builds dist/homestat: a local Home Assistant add-on that compiles homestat
# from source on the HA host (no registry needed). Copy that folder to
# /addons/homestat on the Home Assistant machine.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/dist/homestat"
rm -rf "$out"
mkdir -p "$out"
cp "$root"/addon/config.yaml "$root"/addon/DOCS.md "$root"/addon/CHANGELOG.md "$out"/
# No image: → the Supervisor builds the Dockerfile next to config.yaml.
sed -i.bak '/^image:/d' "$out/config.yaml" && rm "$out/config.yaml.bak"
cp -R "$root"/Cargo.toml "$root"/Cargo.lock "$root"/src "$root"/assets "$root"/Dockerfile "$out"/
echo "wrote $out"
