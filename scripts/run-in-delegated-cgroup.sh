#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: COOP_DELEGATED_CGROUP_ROOT=/sys/fs/cgroup/... $0 COMMAND [ARG ...]" >&2
  exit 64
fi

root="${COOP_DELEGATED_CGROUP_ROOT:-}"
if [[ -z "$root" || "$root" != /* ]]; then
  echo "COOP_DELEGATED_CGROUP_ROOT must be an absolute path" >&2
  exit 64
fi

root="$(readlink -f -- "$root")"
case "$root" in
  /sys/fs/cgroup/*) ;;
  *)
    echo "refusing non-cgroup delegated root: $root" >&2
    exit 64
    ;;
esac

if [[ ! -f "$root/cgroup.controllers" ]]; then
  echo "delegated cgroup root does not exist: $root" >&2
  exit 66
fi

runner="$(sudo mktemp -d "$root/runner.XXXXXXXX")"
runner_user="$(id -un)"
runner_path="$PATH"

cleanup() {
  if ! sudo rmdir -- "$runner"; then
    echo "could not remove delegated runner cgroup: $runner" >&2
  fi
}
trap cleanup EXIT

# Moving an unprivileged process from its session cgroup into a sibling cgroup
# requires write access at their common ancestor. Start a root-owned shell,
# move that exact shell first, then drop back to the invoking user. Every child
# of COMMAND consequently begins inside the delegated runner subtree.
sudo -E bash -c '
  set -euo pipefail
  runner=$1
  runner_user=$2
  runner_path=$3
  shift 3
  printf "%s\n" "$$" > "$runner/cgroup.procs"
  exec sudo -E -H -u "$runner_user" -- env "PATH=$runner_path" "$@"
' coop-cgroup-runner "$runner" "$runner_user" "$runner_path" "$@"
