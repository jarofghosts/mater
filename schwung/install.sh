#!/usr/bin/env bash
#
# Copy a built module onto an attached Move. Development loop only — a release goes through
# schwung-manager at http://move.local:7700, which is the supported install path.
#
#   schwung/install.sh                 # to move.local
#   schwung/install.sh 192.168.1.40    # to a specific host
set -euo pipefail

MODULE_ID=mater
HOST=${1:-move.local}
USER=${SCHWUNG_USER:-ableton}
DEST="/data/UserData/schwung/modules/sound_generators/$MODULE_ID"

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
STAGE="$ROOT/dist/$MODULE_ID"

[ -d "$STAGE" ] || { echo "error: no $STAGE — run schwung/build.sh first" >&2; exit 1; }

echo "==> $USER@$HOST:$DEST"
ssh "$USER@$HOST" "mkdir -p '$DEST'"
scp "$STAGE/dsp.so" "$STAGE/module.json" "$USER@$HOST:$DEST/"

cat <<'EOF'

Installed. The chain host holds the old dsp.so open until the slot is reloaded, so:

  - swap the slot to another module and back, or
  - ssh ableton@move.local "systemctl --user restart schwung" (whichever unit is in use)

Then, to watch it come up:

  ssh ableton@move.local "touch /data/UserData/schwung/debug_log_on"
  ssh ableton@move.local "tail -f /data/UserData/schwung/debug.log" | grep mater
EOF
