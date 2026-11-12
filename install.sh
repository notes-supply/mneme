#!/bin/sh
# Installs Mneme and Atrium from their latest releases on GitHub, for Linux on x86_64 or
# aarch64, into ~/.local/bin:
#
#   curl -fsSL https://raw.githubusercontent.com/notes-supply/mneme/main/install.sh | sh
#
# Name the apps to install only those: `sh -s -- mneme`. MNEME_INSTALL_DIR names another
# folder. It never uses sudo.
#
# Each binary is checked against its release's SHA256SUMS and, when cosign is on PATH, against
# its keyless signature by that release's workflow, before it is moved into place. When atrium
# goes into ~/.local/bin and ~/.config/systemd/user/atrium.service is not there yet, it also
# installs Atrium's user unit, without enabling it.
#
# MNEME_RELEASES_URL replaces https://github.com/notes-supply, so that a test can serve a
# release of its own. The identity cosign checks is always GitHub's.
#
# Everything is in functions, and the last line runs them, so a download cut short runs nothing.
set -eu

releases=${MNEME_RELEASES_URL:-https://github.com/notes-supply}
issuer=https://token.actions.githubusercontent.com
docs=https://github.com/notes-supply/mneme/blob/main/docs
script=https://raw.githubusercontent.com/notes-supply/mneme/main/install.sh

say() { printf '%s\n' "$*"; }
fail() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

fetch() {
  case $releases in
    https://*) protocols='=https' ;;
    *) protocols='=http,https' ;;
  esac
  curl -fsSL --retry 3 --proto "$protocols" --proto-redir "$protocols" "$@"
}

# Sets arch to the name the release binaries use for this machine.
detect() {
  system=$(uname -s)
  [ "$system" = Linux ] \
    || fail "Mneme and Atrium have release binaries for Linux only, and this is $system"
  machine=$(uname -m)
  case $machine in
    x86_64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) fail "Mneme and Atrium have release binaries for x86_64 and aarch64 only, and this machine is $machine" ;;
  esac
  for tool in curl sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool is needed, and it is not on PATH"
  done
}

# Downloads the latest release of $1 into $work/$1 and checks it, or fails.
download() {
  app=$1
  latest=$(fetch -I -o /dev/null -w '%{url_effective}' "$releases/$app/releases/latest") \
    || fail "could not find the latest release of $app"
  tag=${latest##*/}
  case $tag in
    *[!A-Za-z0-9._-]*) fail "the latest release of $app has an unexpected tag: $tag" ;;
    v[0-9]*) ;;
    *) fail "the latest release of $app is at $latest, which names no version" ;;
  esac
  file=$app-$arch-linux
  mkdir "$work/$app"
  say "Downloading $app $tag for $arch"
  for name in "$file" "$file.sigstore.json" SHA256SUMS; do
    fetch -o "$work/$app/$name" "$releases/$app/releases/download/$tag/$name" \
      || fail "could not download $name of $app $tag"
  done

  expected=$(awk -v file="$file" '$2 == file || $2 == "*" file { print $1 }' "$work/$app/SHA256SUMS")
  [ -n "$expected" ] || fail "the SHA256SUMS of $app $tag does not list $file"
  actual=$(sha256sum "$work/$app/$file" | cut -d' ' -f1)
  [ "$actual" = "$expected" ] \
    || fail "$file of $app $tag does not match its SHA256SUMS, so nothing was installed"

  if [ "$cosign" = yes ]; then
    identity=https://github.com/notes-supply/$app/.github/workflows/binaries.yml@refs/tags/$tag
    if ! cosign verify-blob "$work/$app/$file" --bundle "$work/$app/$file.sigstore.json" \
      --certificate-oidc-issuer "$issuer" --certificate-identity "$identity" \
      >"$work/$app/cosign.log" 2>&1; then
      cat "$work/$app/cosign.log" >&2
      fail "$file of $app $tag is not signed by $identity, so nothing was installed"
    fi
  fi

  if [ "$app" = atrium ] && [ "$systemd" = yes ] && [ ! -e "$units/atrium.service" ]; then
    fetch -o "$work/atrium/atrium.service" "$releases/atrium/raw/$tag/contrib/atrium.service" \
      || fail "could not download Atrium's unit at $tag"
  fi
  say "$tag" >"$work/$app/tag"
}

# Moves the checked binary of $1 into $dir: a copy in the same folder, then a rename, so it is
# replaced in one step and a running one goes on with the file it has open.
place() {
  staged=$dir/.$1.new.$$
  cp "$work/$1/$1-$arch-linux" "$staged" \
    || fail "could not write to $dir. Set MNEME_INSTALL_DIR to a folder you can write"
  chmod 755 "$staged"
  mv -f "$staged" "$dir/$1"
  staged=
}

# Installs Atrium's user unit, unless one is there already, and loads it when the user manager
# answers, which it may not over SSH or on a headless machine. Sets unit to yes when there is a
# unit, and reload to yes when it is not loaded yet.
install_unit() {
  unit=yes
  if [ -e "$units/atrium.service" ]; then
    say "$units/atrium.service is already there, and was left as it is."
    return 0
  fi
  if [ ! -f "$work/atrium/atrium.service" ]; then
    unit=no
    return 0
  fi
  mkdir -p "$units"
  cp "$work/atrium/atrium.service" "$units/.atrium.service.new.$$"
  mv "$units/.atrium.service.new.$$" "$units/atrium.service"
  if systemctl --user show-environment >/dev/null 2>&1 && systemctl --user daemon-reload; then
    say "Installed $units/atrium.service, not enabled."
  else
    reload=yes
    say "Installed $units/atrium.service, not enabled. systemd's user manager did not answer, so run systemctl --user daemon-reload before enabling it."
  fi
}

# Says when git is missing or older than 2.45, which the autosave service needs.
check_git() {
  if ! command -v git >/dev/null 2>&1; then
    say "Mneme needs Git 2.45 or newer, and git is not on PATH."
    return 0
  fi
  version=$(git --version | awk '{ print $3 }')
  major=${version%%.*}
  minor=${version#*.}
  minor=${minor%%.*}
  case $major:$minor in
    *[!0-9:]* | :* | *:)
      say "Mneme needs Git 2.45 or newer, and git says it is $version."
      ;;
    *)
      if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 45 ]; }; then
        say "Mneme needs Git 2.45 or newer, and this is Git $version."
      fi
      ;;
  esac
}

has() {
  case " $apps " in
    *" $1 "*) return 0 ;;
    *) return 1 ;;
  esac
}

main() {
  if [ $# -eq 0 ]; then set -- mneme atrium; fi
  apps=
  for app in "$@"; do
    case $app in
      mneme | atrium) ;;
      *) fail "there is no app called $app. Name mneme, atrium or both, or none for both" ;;
    esac
    has "$app" || apps="$apps $app"
  done
  default_dir=${HOME:?HOME is not set}/.local/bin
  dir=${MNEME_INSTALL_DIR:-$default_dir}
  # Atrium's unit runs %h/.local/bin/atrium and %h/.local/bin/mneme, so it goes in only beside
  # binaries in that folder, on a machine with systemd.
  case ${XDG_CONFIG_HOME:-} in
    /*) units=$XDG_CONFIG_HOME/systemd/user ;;
    *) units=$HOME/.config/systemd/user ;;
  esac
  if [ "$dir" = "$default_dir" ] && command -v systemctl >/dev/null 2>&1; then
    systemd=yes
  else
    systemd=no
  fi
  detect
  if command -v cosign >/dev/null 2>&1; then cosign=yes; else cosign=no; fi

  work=$(mktemp -d)
  staged=
  trap 'rm -rf "$work"; if [ -n "$staged" ]; then rm -f "$staged"; fi' EXIT
  trap 'exit 1' HUP INT TERM

  # Every app is downloaded and checked before any is installed.
  for app in $apps; do download "$app"; done
  mkdir -p "$dir" || fail "could not create $dir. Set MNEME_INSTALL_DIR to a folder you can write"
  installed=
  replaced=no
  for app in $apps; do
    if [ -e "$dir/$app" ]; then replaced=yes; fi
    place "$app"
    installed="$installed, $app $(cat "$work/$app/tag")"
  done
  say "Installed ${installed#, } in $dir."
  if [ "$cosign" = yes ]; then
    say "Each binary matched its release's SHA256SUMS and its signature."
  else
    say "Each binary matched its release's SHA256SUMS. cosign is not on PATH, so only the checksums were checked; to check the signatures too, see $docs/install.md#verify-a-download"
  fi
  if [ "$replaced" = yes ]; then
    say "A service that was running the old version goes on with it until it restarts: systemctl --user try-restart mneme-autosave atrium"
  fi
  unit=no
  reload=no
  if has atrium && [ "$systemd" = yes ]; then install_unit; fi
  case ":$PATH:" in
    *":$dir:"*) ;;
    *) say "$dir is not on PATH. Add it, for example in ~/.profile: export PATH=\"$dir:\$PATH\"" ;;
  esac
  if has mneme; then check_git; fi

  say ""
  say "Next:"
  if has mneme; then
    say "  mneme init --remote https://github.com/YOU/wiki.git --token-file ~/.config/mneme/token"
    say "    syncs ~/wiki with a repository; $docs/configuration.md#remotes has the other remotes"
  fi
  if has atrium; then
    if [ ! -e "$dir/mneme" ]; then
      if [ "$dir" = "$default_dir" ]; then
        say "  curl -fsSL $script | sh -s -- mneme"
      else
        say "  curl -fsSL $script | MNEME_INSTALL_DIR=$dir sh -s -- mneme"
      fi
      say "    installs Mneme first, which Atrium runs to read the wiki"
    else
      if [ "$unit" = yes ]; then
        if [ "$reload" = yes ]; then say "  systemctl --user daemon-reload"; fi
        say "  systemctl --user enable --now atrium"
      else
        say "  atrium --wiki-folder ~/wiki"
      fi
      say "    serves the wiki at http://localhost:8080"
    fi
  fi
}

main "$@"
