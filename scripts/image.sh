#!/usr/bin/env bash
# Builds the server's image and checks it as it will run: non-root, read-only, in the memory its
# pod has, the repository on a volume, the tokens mounted read-only, a real clone and push, and a
# Trivy scan. It then runs the autosave service from the image against the server, so the
# service's Git options meet the image's Git, and sends a file of the largest size the rules let
# in through both. Pushes nothing.
set -euo pipefail

image=${MNEME_IMAGE:-localhost/mneme:dev}
# A log file of podman's own, since a journald log is not readable for every user.
limits=(--cpus 2 --memory 4g --log-driver k8s-file)
# The server gets what deploy/deployment.yaml gives its pod, and no swap beyond it.
pod_memory=$((256 << 20))
server_limits=(--cpus 2 --memory "$pod_memory" --memory-swap "$pod_memory" --log-driver k8s-file)
# The largest file the rules let in.
large=$((100 << 20))
# The most memory the server may hold that it cannot give back while such a file passes through.
# It measured 4 to 6 MiB. Without core.bigFileThreshold, Git alone holds 200 MiB.
most_held=$((64 << 20))
work=$(mktemp -d)
volume=mneme-image-check-$$
container=
services=()
cleanup() {
  for service in "${services[@]}"; do podman rm -f "$service" >/dev/null 2>&1 || true; done
  if [ -n "$container" ]; then podman rm -f "$container" >/dev/null 2>&1 || true; fi
  podman volume rm -f "$volume" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
fail() {
  echo "image check failed: $*" >&2
  if [ -n "$container" ]; then podman logs "$container" >&2 || true; fi
  exit 1
}

podman build --cpu-period=100000 --cpu-quota=200000 --memory 4g -t "$image" -f Containerfile .

binary=$(podman create "$image")
podman cp "$binary:/usr/local/bin/mneme" "$work/mneme"
podman rm "$binary" >/dev/null
if readelf -lW "$work/mneme" | grep -q 'program interpreter'; then
  fail "mneme names a program interpreter, so it is not fully static"
fi
echo "mneme is static: $(readelf -hW "$work/mneme" | grep 'Type:' | xargs)"

user=$(podman image inspect "$image" --format '{{.Config.User}}')
[ "$user" = "65532:65532" ] || fail "the image runs as $user, not 65532:65532"

new_token() { head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n'; }
hash() { printf %s "$1" | sha256sum | cut -d' ' -f1; }
token=$(new_token)
other_token=$(new_token)
printf '{ "check": "%s", "other": "%s" }\n' "$(hash "$token")" "$(hash "$other_token")" \
  > "$work/tokens.json"
chmod 644 "$work/tokens.json"
podman volume create "$volume" >/dev/null
container=$(podman run -d "${server_limits[@]}" --read-only \
  -v "$volume:/data" -v "$work/tokens.json:/etc/mneme/tokens.json:ro,Z" \
  -p 127.0.0.1::8080 "$image")
port=$(podman port "$container" 8080 | head -1 | sed 's/.*://')
url=http://127.0.0.1:$port
for _ in $(seq 100); do
  curl -fsS "$url/healthz" >/dev/null 2>&1 && break
  podman container exists "$container" && [ "$(podman inspect "$container" --format '{{.State.Running}}')" = true ] \
    || fail "the server stopped"
  sleep 0.2
done
curl -fsS "$url/healthz" >/dev/null || fail "the server did not answer"
echo "mneme serve started read-only, so the image's $(podman exec "$container" git --version) is new enough"

[ "$(podman exec "$container" sh -c 'id -u')" = 65532 ] || fail "the server does not run as 65532"
[ "$(podman inspect "$container" --format '{{.HostConfig.ReadonlyRootfs}}')" = true ] \
  || fail "the root filesystem is writable"
if podman exec "$container" sh -c 'touch /written' 2>/dev/null; then fail "the root filesystem took a write"; fi
if podman exec "$container" sh -c 'touch /etc/mneme/tokens.json' 2>/dev/null; then fail "the tokens file took a write"; fi
if podman exec "$container" sh -c 'command -v apk' >/dev/null; then fail "the image has a package manager"; fi
echo "runs as 65532 on a read-only root, the tokens file is read-only, and there is no package manager"

# What the kernel says of the server's memory, from inside its container.
memory() { podman exec "$container" cat "/sys/fs/cgroup/$1"; }
[ "$(memory memory.max)" = "$pod_memory" ] || fail "the server's memory limit is $(memory memory.max), not $pod_memory"
[ "$(memory memory.swap.max)" = 0 ] || fail "the server may swap $(memory memory.swap.max) bytes"
echo "the server has $((pod_memory >> 20)) MiB of memory and no swap, as its pod does"

# Git with no configuration of the machine's own, and the token sent only as a header. Each
# command is shown without it.
git() {
  echo "\$ git $*" >&2
  command env -i PATH="$PATH" HOME="$work" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
    GIT_AUTHOR_NAME=Check GIT_AUTHOR_EMAIL=check@example.invalid \
    GIT_COMMITTER_NAME=Check GIT_COMMITTER_EMAIL=check@example.invalid \
    git -c "http.extraHeader=Authorization: Bearer $token" "$@"
}
cd "$work"
git clone "$url/git/wiki.git" wiki
printf '# Image check\n\nPushed through the container.\n' > wiki/check.md
git -C wiki add check.md
git -C wiki commit -q -m 'Check the image'
git -C wiki push origin main
ln -s check.md wiki/link.md
git -C wiki add link.md
git -C wiki commit -q -m 'Push a symlink'
if git -C wiki push origin main 2>refused; then fail "the push check let a symlink in"; fi
cat refused >&2
grep -q 'mneme: commit .* has a symlink at link.md' refused || fail "the push check did not run"
git clone "$url/git/wiki.git" again
grep -q 'Pushed through the container' again/check.md || fail "a fresh clone lacks the page"
cd - >/dev/null

pages=$(curl -fsS "$url/v1/pages")
echo "GET /v1/pages: $pages"
printf %s "$pages" | grep -q '"path":"check.md","title":"Image check"' || fail "/v1/pages lacks the pushed page"

# The autosave service, run from the image as 65532 with its folder mapped to this user, on the
# host's network so that it reaches the server's published port.
printf %s "$token" > "$work/check.token"
printf %s "$other_token" > "$work/other.token"
mkdir "$work/folder-check" "$work/folder-other"
autosave() {
  local name=$1
  shift
  podman run "${limits[@]}" --read-only --network host --userns keep-id:uid=65532,gid=65532 \
    -v "$work/folder-$name:/wiki" -v "$work/$name.token:/token:ro" "$@" \
    --entrypoint mneme "$image" autosave "${autosave_command[@]}" \
    --folder /wiki --server "$url" --token-file /token --name "$name" "${autosave_timing[@]}"
}
# Runs the service on a folder until it reports `outcome`, then stops it with SIGTERM.
run_until() {
  local name=$1 outcome=$2 service
  autosave_command=(run)
  autosave_timing=(--tick-seconds 0.2 --quiet-seconds 1 --fetch-seconds 1)
  service=$(autosave "$name" -d)
  services+=("$service")
  for _ in $(seq 300); do
    podman logs "$service" 2>&1 | grep -q "\"outcome\":\"$outcome\"" && break
    [ "$(podman inspect "$service" --format '{{.State.Running}}')" = true ] \
      || { podman logs "$service" >&2; fail "the service on $name stopped"; }
    sleep 0.2
  done
  podman stop -t 30 "$service" >/dev/null
  podman logs "$service" > "$work/$name.log" 2>&1
  echo "autosave on $name: $(grep -o '"outcome":"[a-z]*"' "$work/$name.log" | cut -d'"' -f4 | xargs)"
  grep -q "\"outcome\":\"$outcome\"" "$work/$name.log" || { cat "$work/$name.log" >&2; fail "$name never reported $outcome"; }
  if grep -Eq '"outcome":"(blocked|stopped|offline)"' "$work/$name.log"; then
    cat "$work/$name.log" >&2; fail "the service on $name was stuck"
  fi
  [ "$(podman inspect "$service" --format '{{.State.ExitCode}}')" = 0 ] || fail "the service on $name did not stop cleanly"
  podman rm "$service" >/dev/null
}
# Dates a page a minute back, so that the service finds it quiet on its first tick.
write_page() {
  printf '%s' "$2" > "$1"
  touch -d "@$(($(date +%s) - 60))" "$1"
}
autosave_command=(init)
autosave_timing=()
for name in check other; do autosave "$name" --rm; done
[ -f "$work/folder-other/check.md" ] || fail "autosave init did not clone the wiki"
echo "mneme autosave init cloned the wiki into two folders with $(podman exec "$container" git --version)"

write_page "$work/folder-check/saved.md" $'# Saved\n\nThe first line.\n'
run_until check pushed
run_until other pulled
grep -q 'The first line' "$work/folder-other/saved.md" || fail "the other folder did not fast-forward"
echo "one save pushed from one folder and fast-forwarded into the other"

write_page "$work/folder-check/saved.md" $'# Saved\n\nThe line from check.\n'
write_page "$work/folder-other/saved.md" $'# Saved\n\nThe line from other.\n'
run_until check pushed
run_until other pushed
git -C "$work/again" pull -q --ff-only
grep -q 'The line from other' "$work/again/saved.md" || fail "the replayed side did not win the overlap"
git -C "$work/again" log -1 --format=%B | grep -q '^Overlap: [0-9a-f]\{40\}$' || fail "the replay has no Overlap trailer"
changes=$(curl -fsS "$url/v1/changes?limit=1")
echo "GET /v1/changes?limit=1: $changes"
printf %s "$changes" | grep -q '"overlap_author":"check"' || fail "the overlap does not name check's edit"
echo "an overlapping edit was replayed and pushed with an Overlap trailer naming check's edit"

# A file of the largest size the rules let in, of bytes that do not compress, in the pod's
# memory: pushed by hand with a stock Git, fetched by a second clone, read back whole and by
# range, then changed in one folder by the autosave service and followed in the other. A sampler
# in the container notes the most memory the server held that it could not give back. With no
# swap that is its anonymous memory (`anon`: the heap and stacks of mneme and of Git), its shared
# memory (`shmem`, which is in `file` but cannot be dropped), and memory the kernel holds for it
# (`kernel`: page tables, socket buffers and slab, part of which could be given back, so it counts
# high). All three come from one read of `memory.stat`, so they belong to one moment. The page
# cache is left out: it fills what is left of the limit and is dropped when something needs room.
podman exec "$container" sh -c '
  peak=0
  while [ ! -e /tmp/sampled ]; do
    stat=$(cat /sys/fs/cgroup/memory.stat)
    anon= shmem= kernel=
    while read -r key value; do
      case $key in anon) anon=$value ;; shmem) shmem=$value ;; kernel) kernel=$value ;; esac
    done <<STAT
$stat
STAT
    if [ -z "$anon" ] || [ -z "$shmem" ] || [ -z "$kernel" ]; then echo unreadable; exit; fi
    held=$((anon + shmem + kernel))
    if [ "$held" -gt "$peak" ]; then peak=$held; echo "$peak"; fi
    sleep 0.05
  done' > "$work/held" &
sampler=$!
sha() { sha256sum "$1" | cut -d' ' -f1; }
git -C "$work/again" pull -q --ff-only
mkdir "$work/again/brand"
head -c "$large" /dev/urandom > "$work/again/brand/kit.bin"
first=$(sha "$work/again/brand/kit.bin")
git -C "$work/again" add brand/kit.bin
git -C "$work/again" commit -q -m 'Add a brand kit'
git -C "$work/again" push origin main || fail "the server refused a file of $((large >> 20)) MiB"
git clone "$url/git/wiki.git" "$work/large"
[ "$(sha "$work/large/brand/kit.bin")" = "$first" ] || fail "a second clone got other bytes"
curl -fsS -D "$work/raw.headers" -o "$work/raw.whole" "$url/v1/raw/brand/kit.bin"
[ "$(sha "$work/raw.whole")" = "$first" ] || fail "/v1/raw sent other bytes"
tr -d '\r' < "$work/raw.headers" | grep -qix "content-length: $large" || fail "/v1/raw did not say how long the file is"
curl -fsS -r "$((large / 2))-$((large / 2 + (1 << 20) - 1))" -o "$work/raw.part" -w '%{http_code}\n' \
  "$url/v1/raw/brand/kit.bin" > "$work/raw.status"
[ "$(cat "$work/raw.status")" = 206 ] || fail "a range of /v1/raw was $(cat "$work/raw.status"), not 206"
dd if="$work/again/brand/kit.bin" bs=1M skip=$((large >> 21)) count=1 status=none | cmp -s - "$work/raw.part" \
  || fail "a range from the middle of /v1/raw has other bytes"
echo "a file of $((large >> 20)) MiB was pushed, fetched by a second clone, and read back whole and by range"

# One byte more is refused. The file is all zeros, so that it packs to almost nothing.
truncate -s $((large + 1)) "$work/again/brand/over.bin"
git -C "$work/again" add brand/over.bin
git -C "$work/again" commit -q -m 'Add a file over the limit'
if git -C "$work/again" push origin main 2>"$work/refused"; then fail "the push check let in a file over $((large >> 20)) MiB"; fi
cat "$work/refused" >&2
grep -q "mneme: commit .* has a file over $((large >> 20)) MiB at brand/over.bin" "$work/refused" \
  || fail "a file over the limit was not refused for its size"

run_until check pulled
[ "$(sha "$work/folder-check/brand/kit.bin")" = "$first" ] || fail "the service pulled other bytes"
printf 'changed by the service' | dd of="$work/folder-check/brand/kit.bin" bs=1 seek=$((large / 2)) conv=notrunc status=none
touch -d "@$(($(date +%s) - 60))" "$work/folder-check/brand/kit.bin"
second=$(sha "$work/folder-check/brand/kit.bin")
run_until check pushed
run_until other pulled
[ "$(sha "$work/folder-other/brand/kit.bin")" = "$second" ] || fail "the other folder did not get the changed file"
[ "$(curl -fsS "$url/v1/raw/brand/kit.bin" | sha256sum | cut -d' ' -f1)" = "$second" ] || fail "/v1/raw did not send the changed file"
kit=$(curl -fsS "$url/v1/files/brand/kit.bin?limit=1")
echo "GET /v1/files/brand/kit.bin?limit=1: $kit"
printf %s "$kit" | grep -q "\"size\":$large,.*\"versions\":2,\"author\":\"check\"" || fail "/v1/files does not list the changed file"
echo "the autosave service changed the file in one folder, and the other folder followed"

podman exec "$container" touch /tmp/sampled
wait "$sampler"
held=$(tail -1 "$work/held")
events=$(memory memory.events | xargs)
[ "$(podman inspect "$container" --format '{{.State.Running}} {{.State.OOMKilled}}')" = "true false" ] \
  || fail "the server was killed for memory"
for event in oom oom_kill; do
  case " $events " in
    *" $event 0 "*) ;;
    *) fail "the server ran out of memory: $events" ;;
  esac
done
case $held in
  '' | *[!0-9]*) fail "the sampler could not read anon, shmem and kernel from memory.stat: ${held:-nothing}" ;;
esac
[ "$held" -lt "$most_held" ] \
  || fail "the server held $held bytes of anonymous, shared and kernel memory, and may hold $most_held"
echo "the server was never out of memory: it held at most $((held >> 20)) MiB of anonymous, shared and kernel memory, and $(($(memory memory.peak) >> 20)) MiB with the page cache, of $((pod_memory >> 20)) MiB"

podman save -q -o "$work/image.tar" "$image"
trivy image --input "$work/image.tar" --scanners vuln --severity HIGH,CRITICAL --exit-code 1 \
  ${MISE_OFFLINE:+--skip-db-update --skip-java-db-update}
echo "image $image: $(podman image inspect "$image" --format '{{.Size}}') bytes"
