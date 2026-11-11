#!/usr/bin/env bash
# Builds the server's image and checks it as it will run: non-root, read-only, the repository on
# a volume, the tokens mounted read-only, a real clone and push, and a Trivy scan. Pushes nothing.
set -euo pipefail

image=${MNEME_IMAGE:-localhost/mneme:dev}
limits=(--cpus 2 --memory 4g)
work=$(mktemp -d)
volume=mneme-image-check-$$
container=
cleanup() {
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

token=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
hash=$(printf %s "$token" | sha256sum | cut -d' ' -f1)
printf '{ "check": "%s" }\n' "$hash" > "$work/tokens.json"
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

podman save -q -o "$work/image.tar" "$image"
trivy image --input "$work/image.tar" --scanners vuln --severity HIGH,CRITICAL --exit-code 1 \
  ${MISE_OFFLINE:+--skip-db-update --skip-java-db-update}
echo "image $image: $(podman image inspect "$image" --format '{{.Size}}') bytes"
