#!/usr/bin/env bash
# Publishes deploy/ for a cluster's Flux: pins the image by digest, pushes deploy/ as one OCI
# artifact tagged with the commit, signs the image and the artifact, then moves the artifact's
# main tag, which Flux follows. CI runs it once the gate passed on the commit; the kind
# rehearsal runs it against a local registry.
#
#   scripts/release.sh IMAGE@sha256:DIGEST oci://REGISTRY/notes-supply/manifests/mneme
#
# It signs keyless with the CI workflow's identity, which is what the cluster verifies. With
# COSIGN_KEY naming a key file it signs with that key instead, and records nothing in the
# public transparency log.
set -euo pipefail

image=$1
artifact=$2
fail() {
  echo "release: $*" >&2
  exit 1
}
[[ $image =~ ^([^@]+)@(sha256:[0-9a-f]{64})$ ]] || fail "$image is not pinned by digest"
repository=${BASH_REMATCH[1]}
digest=${BASH_REMATCH[2]}
git diff --quiet HEAD -- deploy || fail "deploy/ differs from the commit"
commit=$(git rev-parse HEAD)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -R deploy "$work/manifests"
cat >>"$work/manifests/kustomization.yaml" <<EOF
images:
  - name: ${repository##*/}
    newName: $repository
    digest: $digest
EOF
kubectl kustomize "$work/manifests" >"$work/rendered.yaml"
if grep -E '^\s*(- )?image:' "$work/rendered.yaml" | grep -v '@sha256:'; then
  fail "the manifests name an image that is not pinned by digest"
fi
grep -qF "image: $image" "$work/rendered.yaml" || fail "the manifests do not run $image"

# Flux 2.8.2 finds no signature in cosign 3's default bundle format, which GHCR stores under a
# referrers tag; it verifies the classic .sig signature, so sign in that format.
signing=(--yes --new-bundle-format=false --use-signing-config=false)
if [ -n "${COSIGN_KEY:-}" ]; then
  signing+=(--key "$COSIGN_KEY" --tlog-upload=false)
fi
cosign sign "${signing[@]}" "$image"
pushed=$(flux push artifact "$artifact:$commit" --path "$work/manifests" --reproducible \
  --source "$(git config --get remote.origin.url)" --revision "main@sha1:$commit" --output json)
[[ $pushed =~ \"digest\":\ *\"(sha256:[0-9a-f]{64})\" ]] || fail "flux printed no digest: $pushed"
signed="${artifact#oci://}@${BASH_REMATCH[1]}"
cosign sign "${signing[@]}" "$signed"
flux tag artifact "$artifact:$commit" --tag main
echo "released $commit as $signed"
