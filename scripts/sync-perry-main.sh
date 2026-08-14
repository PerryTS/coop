#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
perry_source="${PERRY_REPO:-$repo_root/../perry/perry}"
worktree="$repo_root/.perry-main"

if [[ ! -d "$perry_source/.git" ]]; then
  echo "Set PERRY_REPO to a Perry git checkout (looked in $perry_source)" >&2
  exit 1
fi

git -C "$perry_source" fetch origin main
if [[ ! -e "$worktree" ]]; then
  git -C "$perry_source" worktree add --detach "$worktree" origin/main
elif [[ -n "$(git -C "$worktree" status --short)" ]]; then
  echo "Refusing to update dirty worktree: $worktree" >&2
  exit 1
else
  git -C "$worktree" checkout --detach origin/main
fi

perry_version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$worktree/Cargo.toml")"
perry_commit="$(git -C "$worktree" rev-parse HEAD)"
lock_file="$repo_root/perry-main.lock"
lock_temp="$lock_file.tmp.$$"
printf 'version = "%s"\ncommit = "%s"\n' "$perry_version" "$perry_commit" > "$lock_temp"
mv "$lock_temp" "$lock_file"

git -C "$worktree" show -s --format='Perry main: %H %cs %s'
