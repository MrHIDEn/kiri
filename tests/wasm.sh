#!/bin/bash
# WASI workloads through the real docker CLI. Kiri runs `wasi/wasm` images in
# the daemon, on wasmtime, and does not boot the VM. This script checks that
# path the way a person would: hello-world on stdout, args and env, a
# non-zero exit, a read-only bind, a detached container's ps/stop/rm,
# `--rm` actually removing the container, a wasi:http component, and a
# component that calls out with wasi:http/outgoing-handler.
#
# The http section needs curl. It always checks the published port. It also
# requests `http://<name>.kiri.local/users/1` when this engine says it is
# serving container domains (`GET /kiri/domains`, the same call tests/e2e.sh
# uses) and the process on that port lists this container. The request uses
# curl --resolve, so it does not need DNS. Another Kiri may own port 80
# while this one has domains off; that is a skip, not a failure.
#
# The outgoing section needs perl, which answers one small HTTP server the
# modules call on this machine's loopback.
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
PASS=0 FAIL=0 SKIP=0
FAILED=()

ok() { PASS=$((PASS + 1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL + 1)); FAILED+=("$1"); echo "  FAIL $1${2:+: $2}"; }
skip() { SKIP=$((SKIP + 1)); echo "  skip $1 ($2)"; }

# GET on this engine's API. tests/e2e.sh reads the domain routers the same way.
engine_get() {
  local path=$1 host
  if [ -n "${DOCKER_HOST:-}" ]; then
    case "$DOCKER_HOST" in
      unix://*) curl -fsS --max-time 2 --unix-socket "${DOCKER_HOST#unix://}" "http://localhost${path}" ;;
      tcp://*)
        host=${DOCKER_HOST#tcp://}
        host=${host%/}
        curl -fsS --max-time 2 "http://${host}${path}" ;;
      *) return 1 ;;
    esac
  elif [ -n "${SOCK:-}" ]; then
    curl -fsS --max-time 2 --unix-socket "$SOCK" "http://localhost${path}"
  else
    return 1
  fi
}

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
  if [ -n "${FETCH_PID:-}" ]; then kill "$FETCH_PID" >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

echo "== image from tests/wasm"
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

echo "== wasi:http"
name=$RUN-http
if ! command -v curl >/dev/null 2>&1; then
  bad "wasi:http checks need curl"
elif cid=$(run -d --name "$name" --entrypoint /http.wasm -p 127.0.0.1::80 "$IMAGE" 2>&1); then
  status=$(d ps --filter "name=^/${name}$" --format '{{.Status}}' 2>&1 || true)
  if printf '%s' "$status" | grep -q '^Up'; then ok "detached http container is listed Up"; else bad "detached http container is listed Up" "$status"; fi
  hp=$(d port "$name" 80/tcp 2>/dev/null | head -n 1 || true)
  hp=${hp##*:}
  body=
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    body=$(curl -fsS --max-time 5 -H 'X-Echo: hi' "http://127.0.0.1:${hp}/users/1" 2>&1) && break
    sleep 0.2
  done
  if printf '%s' "$body" | grep -q 'GET /users/1' && printf '%s' "$body" | grep -q 'hi'; then
    ok "published port serves /users/1"
  else
    bad "published port serves /users/1" "$body"
  fi
  logs=$(d logs "$name" 2>&1 || true)
  if printf '%s' "$logs" | grep -q 'GET /users/1'; then ok "the request is in the container log"; else bad "the request is in the container log" "$logs"; fi
  # The port in GET /kiri/domains is this engine's router, and only while
  # `serving` is true. Port 80 can belong to the installed Kiri when this
  # one (Kiri Dev) has domains off.
  check="http://${name}.kiri.local/users/1 reaches the module"
  if ! command -v python3 >/dev/null 2>&1; then
    skip "$check" "python3 is needed to read /kiri/domains"
  elif ! json=$(engine_get /kiri/domains 2>/dev/null); then
    skip "$check" "this engine did not answer /kiri/domains"
  else
    parsed=$(printf '%s' "$json" | python3 -c 'import json,sys; d=json.load(sys.stdin)["http"]; print(d["port"], "yes" if d.get("serving") else "no")' 2>/dev/null || true)
    dport=${parsed%% *}
    serving=${parsed##* }
    if [ -z "$parsed" ] || [ "$dport" = "$serving" ]; then
      skip "$check" "could not read this engine's domain router"
    elif [ "$serving" != yes ]; then
      skip "$check" "this engine's container domains are off"
    else
      page=$(curl -sS --max-time 2 --resolve "kiri-wasm-probe.kiri.local:${dport}:127.0.0.1" "http://kiri-wasm-probe.kiri.local:${dport}/" 2>&1) && pec=0 || pec=$?
      if [ "$pec" != 0 ]; then
        bad "$check" "this engine's router on port ${dport} did not answer: $page"
      elif ! printf '%s' "$page" | grep -q "$name"; then
        if printf '%s' "$page" | grep -q 'No running container'; then
          skip "$check" "port ${dport} belongs to another Kiri"
        else
          bad "$check" "port ${dport} is not this engine's router: $page"
        fi
      else
        body=$(curl -fsS --max-time 5 --resolve "${name}.kiri.local:${dport}:127.0.0.1" -H 'X-Echo: name' "http://${name}.kiri.local:${dport}/users/1" 2>&1) && ec=0 || ec=$?
        if [ "$ec" = 0 ] && printf '%s' "$body" | grep -q 'GET /users/1' && printf '%s' "$body" | grep -q 'name'; then
          ok "$check"
        else
          bad "$check" "exit $ec: $body"
        fi
      fi
    fi
  fi
  if lim 20 d stop "$name" >/dev/null 2>&1; then
    state=$(d inspect -f '{{.State.Status}} {{.State.ExitCode}}' "$name" 2>&1 || true)
    case "$state" in
      "exited 143"|"exited 137") ok "docker stop ends the http server ($state)" ;;
      *) bad "docker stop ends the http server" "$state" ;;
    esac
  else
    bad "docker stop ends the http server" "stop timed out"
  fi
  if d rm "$name" >/dev/null 2>&1; then
    left=$(d ps -aq --filter "name=^/${name}$" 2>/dev/null || true)
    if [ -z "$left" ]; then ok "docker rm removes the http container"; else bad "docker rm removes the http container" "$left"; fi
  else
    bad "docker rm removes the http container"
  fi
else
  bad "http container starts" "$cid"
fi

echo "== wasi:http outgoing"
if ! command -v perl >/dev/null 2>&1; then
  bad "outgoing checks need perl"
else
  FETCH_PORT_FILE=$(mktemp)
  perl -e '
    use IO::Socket::INET;
    my $srv = IO::Socket::INET->new(LocalAddr => "127.0.0.1", LocalPort => 0, Listen => 5, Reuse => 1) or die $!;
    print $srv->sockport, "\n";
    STDOUT->flush;
    while (my $c = $srv->accept) {
      local $/ = "\r\n\r\n";
      my $req = <$c>;
      my $body = "kiri-fetched-ok\n";
      print $c "HTTP/1.1 200 OK\r\nContent-Length: ".length($body)."\r\nConnection: close\r\n\r\n$body";
      close $c;
    }
  ' >"$FETCH_PORT_FILE" &
  FETCH_PID=$!
  fport=
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    fport=$(head -n 1 "$FETCH_PORT_FILE" 2>/dev/null || true)
    [ -n "$fport" ] && break
    sleep 0.1
  done
  if [ -z "$fport" ]; then
    bad "the test HTTP server starts"
  else
    url="http://127.0.0.1:${fport}/hello"
    out=$(run --rm --name "$RUN-fetch" --entrypoint /fetch.wasm "$IMAGE" "$url" 2>&1) && ec=0 || ec=$?
    if [ "$ec" = 0 ] && printf '%s' "$out" | grep -q 'kiri-fetched-ok'; then
      ok "a command module GETs the test server"
    else
      bad "a command module GETs the test server" "exit $ec: $out"
    fi
    fname=$RUN-out
    if cid=$(run -d --name "$fname" --entrypoint /http.wasm -p 127.0.0.1::80 "$IMAGE" 2>&1); then
      hp=$(d port "$fname" 80/tcp 2>/dev/null | head -n 1 || true)
      hp=${hp##*:}
      body=
      for _ in 1 2 3 4 5 6 7 8 9 10; do
        body=$(curl -fsS --max-time 5 -H "X-Fetch: $url" "http://127.0.0.1:${hp}/" 2>&1) && break
        sleep 0.2
      done
      if printf '%s' "$body" | grep -q 'kiri-fetched-ok'; then
        ok "an incoming handler returns the fetched response"
      else
        bad "an incoming handler returns the fetched response" "$body"
      fi
      lim 20 d rm -f "$fname" >/dev/null 2>&1 || true
    else
      bad "an incoming handler returns the fetched response" "$cid"
    fi
  fi
  kill "$FETCH_PID" >/dev/null 2>&1 || true
  wait "$FETCH_PID" 2>/dev/null || true
  FETCH_PID=
  rm -f "$FETCH_PORT_FILE"
fi

echo
if [ "$SKIP" = 0 ]; then
  echo "wasm: $PASS passed, $FAIL failed"
else
  echo "wasm: $PASS passed, $FAIL failed, $SKIP skipped"
fi
if [ "$FAIL" != 0 ]; then
  printf '  %s\n' "${FAILED[@]}"
  exit 1
fi
