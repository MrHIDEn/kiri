#!/bin/bash
# WASI workloads through the real docker CLI. Kiri runs `wasi/wasm` images in
# the daemon, on wasmtime, and does not boot the VM. This script checks that
# path the way a person would: hello-world on stdout, args and env, a
# non-zero exit, a read-only bind, a detached container's ps/stop/rm, and
# `--rm` actually removing the container.
#
# Kiri has no image builder, so the image is not `docker build`ed. The
# modules live as `.wat` in tests/wasm/ and a `cargo test` (no VM, no daemon)
# packs them into a `docker load` archive. That needs a toolchain that can
# build this crate (Rust 1.97 or newer: wasmtime's minimum).
#
#   tests/wasm.sh
#   DOCKER_CONTEXT=kiri-dev tests/wasm.sh
#   DOCKER_HOST=unix:///tmp/kiri.sock tests/wasm.sh
#   DOCKER=docker.exe DOCKER_HOST=tcp://127.0.0.1:2375 tests/wasm.sh
#
# The daemon must already be running. A missing engine is an error, not a
# skip: this script is the wasm check, and it has nothing to say about any
# other engine. Exits non-zero if any check fails. Containers it creates are
# labelled kiri.wasm-test=<run> and removed at the end, even on ^C.

set -u

DOCKER=${DOCKER:-docker}
# Git Bash rewrites arguments that look like paths. The bind below is one.
case "${OSTYPE:-}" in msys*|cygwin*) export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' ;; esac
D=("$DOCKER")
if [ -n "${DOCKER_HOST:-}" ]; then D+=(-H "$DOCKER_HOST")
else D+=(--context "${DOCKER_CONTEXT:-kiri}"); fi
d() { "${D[@]}" "$@"; }

if [ -z "${DOCKER_HOST:-}" ]; then
  CTX=${DOCKER_CONTEXT:-kiri}
  SOCK=$(docker context inspect "$CTX" --format '{{.Endpoints.docker.Host}}' 2>/dev/null); SOCK=${SOCK#unix://}
  [ -S "$SOCK" ] || { echo "wasm: no engine socket for context $CTX ($SOCK)"; exit 1; }
fi

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
RUN=kiri-wasm-$$
LABEL=kiri.wasm-test=$RUN
IMAGE=kiri-wasm-test:latest
PASS=0 FAIL=0
FAILED=()

ok() { PASS=$((PASS + 1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL + 1)); FAILED+=("$1"); echo "  FAIL $1${2:+: $2}"; }

# Seconds, then a command. `d` is this script's docker, which timeout cannot
# exec (it is a function). GNU timeout where it exists, else perl's alarm.
lim() {
  local s=$1; shift
  case $1 in
    d) shift; set -- "${D[@]}" "$@" ;;
  esac
  if command -v timeout >/dev/null 2>&1; then timeout "$s" "$@"
  elif command -v perl >/dev/null 2>&1; then perl -e 'alarm shift; exec @ARGV or exit 127' "$s" "$@"
  else "$@"; fi
}

cleanup() {
  ids=$(d ps -aq --filter "label=$LABEL" 2>/dev/null || true)
  if [ -n "$ids" ]; then
    # shellcheck disable=SC2086
    d rm -f $ids >/dev/null 2>&1 || true
  fi
  if [ -n "${NOTE:-}" ]; then rm -rf "$NOTE"; fi
  if [ -n "${TAR:-}" ]; then rm -f "$TAR"; fi
}
trap cleanup EXIT

echo "== image from tests/wasm/*.wat"
TAR=$(mktemp)
export KIRI_WASM_IMAGE=$TAR
if ! (cd "$ROOT" && cargo test -p kiri --bin kiri daemon::wasm::tests::a_loadable_wasi_image_can_be_written -- --exact) >"${TAR}.log" 2>&1; then
  echo "wasm: cargo test did not pack the image"
  cat "${TAR}.log"
  exit 1
fi
rm -f "${TAR}.log"
if ! load=$(lim 60 d load -i "$TAR" 2>&1); then
  echo "wasm: docker load failed: $load"
  exit 1
fi
printf '%s\n' "$load" | grep -q "$IMAGE" || { echo "wasm: docker load did not name $IMAGE: $load"; exit 1; }
echo "  loaded $IMAGE"

run() { lim 30 d run --label "$LABEL" --platform wasi/wasm "$@"; }

echo "== hello, args, exit, read-only"
out=$(run --rm --name "$RUN-hello" "$IMAGE" 2>&1) && ec=0 || ec=$?
if [ "$ec" = 0 ] && printf '%s' "$out" | grep -q 'hello-wasi'; then ok "hello-world stdout"; else bad "hello-world stdout" "$out"; fi

out=$(run --rm --name "$RUN-args" --entrypoint /args.wasm -e WASM_MODE=wasi "$IMAGE" alpha beta 2>&1) && ec=0 || ec=$?
if [ "$ec" = 0 ] && printf '%s' "$out" | grep -q 'alpha' && printf '%s' "$out" | grep -q 'beta' && printf '%s' "$out" | grep -q 'WASM_MODE=wasi' && printf '%s' "$out" | grep -q 'WASM_IMAGE=1'; then
  ok "args and env reach the module"
else
  bad "args and env reach the module" "$out"
fi

set +e
out=$(run --rm --name "$RUN-exit" --entrypoint /exit.wasm "$IMAGE" 2>&1)
ec=$?
set -e
if [ "$ec" = 42 ]; then ok "non-zero exit code is the module's"; else bad "non-zero exit code is the module's" "exit $ec: $out"; fi
set +e

NOTE=$(mktemp -d)
printf 'wasm-note' >"$NOTE/note.txt"
out=$(run --rm --name "$RUN-read" --entrypoint /read.wasm -v "$NOTE:/data:ro" "$IMAGE" 2>&1) && ec=0 || ec=$?
if [ "$ec" = 0 ] && printf '%s' "$out" | grep -q 'wasm-note' && printf '%s' "$out" | grep -q 'readonly'; then
  ok "read-only volume can be read"
else
  bad "read-only volume can be read" "$out"
fi
if [ -e "$NOTE/nope.txt" ]; then bad "read-only volume was not written"; else ok "read-only volume was not written"; fi

echo "== detached ps, stop, rm, and --rm"
if cid=$(run -d --name "$RUN-sleep" --entrypoint /sleep.wasm "$IMAGE" 2>&1); then
  status=$(d ps --filter "name=^/${RUN}-sleep$" --format '{{.Status}}' 2>&1 || true)
  if printf '%s' "$status" | grep -q '^Up'; then ok "detached wasm container is listed Up"; else bad "detached wasm container is listed Up" "$status"; fi
  if lim 20 d stop "$RUN-sleep" >/dev/null 2>&1; then
    state=$(d inspect -f '{{.State.Status}} {{.State.ExitCode}}' "$RUN-sleep" 2>&1 || true)
    case "$state" in
      "exited 143"|"exited 137") ok "docker stop ends the module ($state)" ;;
      *) bad "docker stop ends the module" "$state" ;;
    esac
  else
    bad "docker stop ends the module" "stop timed out"
  fi
  if d rm "$RUN-sleep" >/dev/null 2>&1; then
    left=$(d ps -aq --filter "name=^/${RUN}-sleep$" 2>/dev/null || true)
    if [ -z "$left" ]; then ok "docker rm removes it"; else bad "docker rm removes it" "$left"; fi
  else
    bad "docker rm removes it"
  fi
else
  bad "detached wasm container starts" "$cid"
fi

out=$(run --rm --name "$RUN-gone" "$IMAGE" 2>&1) && ec=0 || ec=$?
left=$(d ps -aq --filter "name=^/${RUN}-gone$" 2>/dev/null || true)
if [ "$ec" = 0 ] && [ -z "$left" ]; then ok "--rm removes the container"; else bad "--rm removes the container" "exit $ec left=${left:-none} $out"; fi

echo
echo "wasm: $PASS passed, $FAIL failed"
if [ "$FAIL" != 0 ]; then
  printf '  %s\n' "${FAILED[@]}"
  exit 1
fi
