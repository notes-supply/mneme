#!/usr/bin/env bash
# Makes the screenshots in docs/images/{terminal,changes}-{light,dark}.png from a real
# run with invented content. Nothing in them is typed in by hand.
#
# Two machines, laptop and desktop, are containers of the server's image with those host names
# and a home folder each, and a third runs the server they sync with, all on a podman network of
# their own. In the machines, systemctl is a stand-in that runs the unit's ExecStart when
# mneme init enables it, so nothing reaches this computer's own user manager. The terminal image
# is what a terminal showed for the commands in it, with a note of how long the script waited
# between them. Git's pager there is cat, as GNU less with Git's default LESS=FRX behaves for
# output that fits the screen; busybox's less would fill the screen. The browser image is
# Atrium v0.1.0, downloaded into a temporary folder, reading the laptop's folder through
# mneme serve --folder.
#
#   MNEME_PLAYWRIGHT=../atrium mise run screenshots
#
# It needs podman, curl, node, oxipng from mise, and a folder whose node_modules has
# Playwright, named by MNEME_PLAYWRIGHT, such as a checkout of Atrium after npm ci.
# MNEME_CHROMIUM names a Chromium to run instead of Playwright's own, such as the system's. It
# takes a few minutes, since the services keep their default timings: 15 seconds of quiet
# before a save and a fetch every 30.
set -euo pipefail

image=${MNEME_IMAGE:-localhost/mneme:dev}
playwright=${MNEME_PLAYWRIGHT:?name a folder whose node_modules has playwright, such as a checkout of Atrium}
atrium_release=https://github.com/notes-supply/atrium/releases/download/v0.1.0
out=docs/images
work=$(mktemp -d)
name=mneme-shots-$$
started=()
atrium=
cleanup() {
  if [ -n "$atrium" ]; then kill "$atrium" 2>/dev/null || true; fi
  for container in "${started[@]}"; do podman rm -f -t 0 "$container" >/dev/null 2>&1 || true; done
  podman network rm -f "$name" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
fail() {
  echo "screenshots: $*" >&2
  exit 1
}
# Waits up to two minutes for a command to succeed.
wait_for() {
  local what=$1
  shift
  for _ in $(seq 240); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  fail "waited two minutes for $what"
}

podman build -q -t "$image" -f Containerfile . >/dev/null
binary=$(podman create "$image")
podman cp "$binary:/usr/local/bin/mneme" "$work/mneme"
podman rm "$binary" >/dev/null

# Atrium's release binary, checked against its release's checksums.
mkdir "$work/atrium"
file=atrium-$(uname -m)-linux
curl -fsSL --proto '=https' -o "$work/atrium/$file" "$atrium_release/$file" \
  -o "$work/atrium/SHA256SUMS" "$atrium_release/SHA256SUMS"
(cd "$work/atrium" && grep " $file\$" SHA256SUMS | sha256sum -c --quiet -) \
  || fail "$file does not match its SHA256SUMS"
chmod 755 "$work/atrium/$file"

cat >"$work/systemctl" <<'EOF'
#!/bin/sh
# A stand-in for systemctl --user: enable --now runs the unit's ExecStart, as systemd would.
case "$*" in
  "--user enable --now mneme-autosave.service")
    command=$(sed -n 's/^ExecStart=//p' "$HOME/.config/systemd/user/mneme-autosave.service")
    setsid sh -c "exec $command" </dev/null >>/tmp/mneme-autosave.log 2>&1 &
    ;;
esac
EOF
chmod 755 "$work/systemctl"

new_token() { head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n'; }
hash() { printf %s "$1" | sha256sum | cut -d' ' -f1; }
laptop_token=$(new_token)
desktop_token=$(new_token)
printf '{ "laptop": "%s", "desktop": "%s" }\n' "$(hash "$laptop_token")" "$(hash "$desktop_token")" \
  >"$work/tokens.json"
chmod 644 "$work/tokens.json"

podman network create "$name" >/dev/null
mkdir "$work/server"
started+=("$name-server")
podman run -d --name "$name-server" --network "$name" --network-alias mneme \
  --userns keep-id:uid=65532,gid=65532 -p 127.0.0.1::8080 \
  -v "$work/server:/data" -v "$work/tokens.json:/etc/mneme/tokens.json:ro" "$image" >/dev/null
server=http://127.0.0.1:$(podman port "$name-server" 8080 | head -1 | sed 's/.*://')
wait_for "the server" curl -fsS "$server/healthz"

machine() {
  mkdir -p "$work/$1/.config/mneme"
  started+=("$name-$1")
  podman run -d --name "$name-$1" --hostname "$1" --network "$name" \
    --userns keep-id:uid=65532,gid=65532 -e HOME="/home/$1" -w "/home/$1" \
    -v "$work/$1:/home/$1" -v "$work/systemctl:/usr/local/bin/systemctl:ro" \
    --entrypoint sleep "$image" 86400 >/dev/null
}
# Runs a command on a machine, out of the picture.
on() {
  local machine=$1
  shift
  podman exec "$name-$machine" sh -c "$*"
}
head_of() { on "$1" 'git -C ~/wiki rev-parse HEAD'; }
server_head() { curl -fsS "$server/v1/status" | sed -n 's/.*"head":"\([0-9a-f]*\)".*/\1/p'; }
# Whether a machine's folder is at the server's head, and its commit holds `text` in `page`.
synced() {
  [ "$(head_of "$1")" = "$(server_head)" ] && on "$1" "git -C ~/wiki grep -qF '$3' HEAD -- '$2'"
}

# The session the terminal image shows: one step per folder, numbered.
session=$work/session
mkdir "$session"
step=0
shown() {
  local machine=$1 command=$2 dir
  step=$((step + 1))
  dir=$session/$(printf %02d "$step")
  mkdir "$dir"
  printf %s "$machine" >"$dir/machine"
  printf %s "$command" >"$dir/command"
  podman exec -t -e GIT_PAGER=cat "$name-$machine" sh -c "$command" </dev/null >"$dir/output"
}
note() {
  step=$((step + 1))
  mkdir "$session/$(printf %02d "$step")"
  printf %s "$1" >"$session/$(printf %02d "$step")/note"
}

machine desktop
machine laptop
printf %s "$desktop_token" >"$work/desktop/.config/mneme/token"
printf %s "$laptop_token" >"$work/laptop/.config/mneme/token"
init='mneme init --server http://mneme:8080 --token-file ~/.config/mneme/token'

# The desktop has kept the wiki for a while: its notes become the wiki on its first save.
mkdir -p "$work/desktop/wiki/recipes" "$work/desktop/wiki/trips"
cat >"$work/desktop/wiki/index.md" <<'EOF'
# Home

Notes for the house, the garden and the trips we plan.

- [Garden](garden.md)
- [Reading](reading.md)
- [Sourdough](recipes/sourdough.md)
- [Trips](trips/)
EOF
cat >"$work/desktop/wiki/garden.md" <<'EOF'
# Garden

- Plant the garlic before the first frost
- Mulch the strawberries
EOF
cat >"$work/desktop/wiki/reading.md" <<'EOF'
# Reading

- The Overstory, Richard Powers
- A Pattern Language, Christopher Alexander
EOF
cat >"$work/desktop/wiki/recipes/sourdough.md" <<'EOF'
# Sourdough

500 g flour, 350 g water, 100 g starter, 10 g salt.

Fold every half hour for two hours, then shape and leave in the fridge overnight.
EOF
cat >"$work/desktop/wiki/trips/hardangervidda.md" <<'EOF'
# Hardangervidda in August

Five days, hut to hut, from Finse to Haukeliseter.
EOF
on desktop "$init" >/dev/null 2>&1
wait_for "the desktop's first save" synced desktop garden.md 'Mulch the strawberries'

shown laptop "$init"
shown laptop 'ls ~/wiki'
shown desktop "echo '- Order seed potatoes' >> ~/wiki/garden.md"
waited=$SECONDS
wait_for "the desktop's edit on the laptop" synced laptop garden.md 'Order seed potatoes'
note "$((SECONDS - waited)) seconds later"
shown laptop "git -C ~/wiki log --format='%h %ar  %s'"
shown laptop 'cat ~/wiki/garden.md'

# More of a day's work, for the browser image.
cat >"$work/laptop/wiki/trips/lofoten.md" <<'EOF'
# Lofoten in March

Ferry from Bodø to Moskenes, then four nights in Reine.

- Wool, crampons, a headlamp
- Book the rorbu before January
EOF
wait_for "the laptop's new page on the desktop" synced desktop trips/lofoten.md 'Book the rorbu'
sed -i 's/100 g starter/120 g starter/' "$work/desktop/wiki/recipes/sourdough.md"
echo '- Braiding Sweetgrass, Robin Wall Kimmerer' >>"$work/laptop/wiki/reading.md"
both() { synced laptop recipes/sourdough.md '120 g' && synced desktop reading.md Braiding; }
wait_for "both edits on both machines" both

# The desktop goes offline and both change the same line. When it is back, its replay keeps its
# own line and marks the commit with an Overlap trailer.
podman network disconnect "$name" "$name-desktop"
sed -i 's/- Order seed potatoes/- Order seed potatoes, Amandine and Charlotte/' "$work/desktop/wiki/garden.md"
saved_offline() { [ "$(on desktop 'git -C ~/wiki rev-list --count origin/main..main')" -gt 0 ]; }
wait_for "the desktop's offline save" saved_offline
sed -i 's/- Order seed potatoes/- Order seed potatoes from the market/' "$work/laptop/wiki/garden.md"
wait_for "the laptop's edit on the server" synced laptop garden.md 'from the market'
podman network connect --alias desktop "$name" "$name-desktop"
overlapped() { curl -fsS "$server/v1/changes?limit=5" | grep -q '"overlap_author":"laptop"'; }
wait_for "the overlap on the server" overlapped
wait_for "the overlap on the laptop" synced laptop garden.md 'Amandine and Charlotte'

port=$(node -e "const s = require('net').createServer().listen(0, '127.0.0.1', () => { console.log(s.address().port); s.close(); })")
TZ=UTC "$work/atrium/$file" --wiki-folder "$work/laptop/wiki" --mneme-bin "$work/mneme" \
  --addr "127.0.0.1:$port" --record-name laptop 2>"$work/atrium.log" &
atrium=$!
wait_for "Atrium" curl -fsS "http://localhost:$port/wiki/changes"

mkdir -p "$out"
MNEME_PLAYWRIGHT=$playwright node scripts/screenshots.mjs "$session" "http://localhost:$port" "$out"
oxipng -o max --strip safe -q "$out"/*.png
ls -l "$out"
