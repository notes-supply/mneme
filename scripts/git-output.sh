#!/usr/bin/env bash
# Writes what the push check reads from Git, as the host's Git and the image's Git print it, into
# tests/fixtures/git/<version>/. The unit tests in src/hook.rs read those files, so the push
# check's parsing is tested against both versions without needing two Gits.
set -euo pipefail

image=${1:-${MNEME_IMAGE:-localhost/mneme:dev}}
out=tests/fixtures/git
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
date='@1788220800 +0000'
git() {
  command env -i PATH="$PATH" HOME="$work" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
    GIT_AUTHOR_NAME=Writer GIT_AUTHOR_EMAIL=writer@example.invalid GIT_AUTHOR_DATE="$date" \
    GIT_COMMITTER_NAME=Writer GIT_COMMITTER_EMAIL=writer@example.invalid GIT_COMMITTER_DATE="$date" \
    git -C "$work/wiki" "$@"
}

mkdir -p "$work/wiki/notes"
git init -q -b main
printf '# A\n' > "$work/wiki/a.md"
printf '# B\n' > "$work/wiki/notes/b.md"
git add -A && git commit -q -m Start
git checkout -q -b side
printf '# Side\n' > "$work/wiki/side.md"
git add -A && git commit -q -m Side
git checkout -q main
printf '# C\n' > "$work/wiki/c.md"
git add -A && git commit -q -m C
git merge -q --no-edit side
ln -s a.md "$work/wiki/link.md"
printf '#!/bin/sh\n' > "$work/wiki/run.sh" && chmod +x "$work/wiki/run.sh"
printf 'space\n' > "$work/wiki/with space.md"
printf 'umlaut\n' > "$work/wiki/ö.md"
printf 'tab\n' > "$work/wiki/tab"$'\t'"name.md"
printf 'newline\n' > "$work/wiki/new"$'\n'"line.md"
git add -A
git update-index --add --cacheinfo "160000,$(git rev-parse HEAD),vendor/lib"
git commit -q -m 'Every kind of entry'

# The same arguments as src/hook.rs, on a repository whose commits are all new.
rev_list=(rev-list --parents main)
ls_tree=(ls-tree -r -t -z -l main)
save() {
  local version=$1
  shift
  mkdir -p "$out/$version"
  "$@" "${rev_list[@]}" > "$out/$version/rev-list"
  "$@" "${ls_tree[@]}" > "$out/$version/ls-tree"
  echo "$out/$version: $(wc -c < "$out/$version/rev-list") and $(wc -c < "$out/$version/ls-tree") bytes"
}
in_image() {
  podman run --rm --cpus 2 --memory 4g --read-only -v "$work/wiki:/wiki:ro" \
    --entrypoint git "$image" -c safe.directory='*' -C /wiki "$@"
}
host=$(command git --version | cut -d' ' -f3)
image_git=$(in_image --version | cut -d' ' -f3)
save "$host" git
save "$image_git" in_image
for file in rev-list ls-tree; do
  if cmp -s "$out/$host/$file" "$out/$image_git/$file"; then
    echo "$file: Git $host and Git $image_git print the same bytes"
  else
    echo "$file: Git $host and Git $image_git differ"
  fi
done
