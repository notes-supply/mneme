#!/usr/bin/env bash
# Builds the server's image and checks it as it will run: non-root, read-only, the repository on
# a volume, the tokens mounted read-only, a real clone and push, and a Trivy scan. It then runs the
# autosave service from the image against the server, so the service's Git options meet the
# image's Git. Pushes nothing.
set -euo pipefail

image=${MNEME_IMAGE:-localhost/mneme:dev}
# A log file of podman's own, since a journald log is not readable for every user.
limits=(--cpus 2 --memory 4g --log-driver k8s-file)
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
container=$(podman run -d "${limits[@]}" --read-only \
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

podman save -q -o "$work/image.tar" "$image"
trivy image --input "$work/image.tar" --scanners vuln --severity HIGH,CRITICAL --exit-code 1 \
  ${MISE_OFFLINE:+--skip-db-update --skip-java-db-update}
echo "image $image: $(podman image inspect "$image" --format '{{.Size}}') bytes"
