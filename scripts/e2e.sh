#!/usr/bin/env bash
# End-to-end proof that the loop closes: a Python program asks, a human answers
# through the CLI, and the program resumes with the answer. Exercises both the
# suspend/replay path and the live long-poll path, plus signal silence detection.
#
# Usage: scripts/e2e.sh            (from the repo root)
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
PORT="${PORT:-8799}"
WORK="$(mktemp -d)"
export INTERJECT_URL="http://127.0.0.1:${PORT}"
export INTERJECT_PROJECT="e2e"
export PYTHONPATH="$ROOT/python/src"

CARGO="${CARGO:-cargo}"
PY="${PY:-python3}"
DAEMON=""

cleanup() {
  if [ -n "$DAEMON" ]; then
    kill "$DAEMON" 2>/dev/null || true
    wait "$DAEMON" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
fail() { printf '\033[31mFAIL: %s\033[0m\n' "$1" >&2; exit 1; }

step "build"
(cd rust && $CARGO build --quiet)
BIN="$ROOT/rust/target/debug/interjectd"
[ -x "$BIN" ] || fail "interjectd was not built"

step "start interjectd"
"$BIN" serve --addr "127.0.0.1:${PORT}" --db "$WORK/e2e.sqlite3" >"$WORK/daemon.log" 2>&1 &
DAEMON=$!
for _ in $(seq 50); do
  if curl -fsS "$INTERJECT_URL/healthz" >/dev/null 2>&1; then break; fi
  sleep 0.1
done
curl -fsS "$INTERJECT_URL/healthz" || fail "daemon never became healthy"
echo

step "a program asks, and suspends because nobody has answered"
KEY=$($PY - <<'PY'
import interject
try:
    interject.ask(
        "Is this a car?",
        options=["car", "motorcycle", "boat"],
        id="vehicle_type",
        context={"title": "2018 Honda CBR", "price": 4200},
        wait=0,
    )
except interject.Suspended as suspended:
    print(suspended.key)
else:
    raise SystemExit("expected Suspended")
PY
)
echo "suspended on ${KEY:0:12}"

step "the question is waiting in the inbox"
"$BIN" inbox | tee "$WORK/inbox.txt"
grep -q "Is this a car?" "$WORK/inbox.txt" || fail "question missing from inbox"
grep -q "${KEY:0:12}" "$WORK/inbox.txt" || fail "key missing from inbox"

step "a human answers it"
"$BIN" answer "$KEY" motorcycle

step "the program runs again and resumes with the answer"
ANSWER=$($PY - <<'PY'
import interject
print(interject.ask(
    "Is this a car?",
    options=["car", "motorcycle", "boat"],
    id="vehicle_type",
    context={"title": "2018 Honda CBR", "price": 4200},
    wait=0,
))
PY
)
[ "$ANSWER" = "motorcycle" ] || fail "expected 'motorcycle', got '$ANSWER'"
echo "resumed with: $ANSWER"

step "the live path: a waiting program is woken by an answer"
$PY - >"$WORK/live.txt" 2>&1 <<'PY' &
import interject
print(interject.ask("Deploy to production?", kind="approve", id="deploy_gate",
                    context={"sha": "abc1234"}, wait=20))
PY
LIVE=$!
sleep 1.5
LIVE_KEY=$($PY -c "
import interject, os
print(interject.question_key(os.environ['INTERJECT_PROJECT'], 'deploy_gate', {'sha': 'abc1234'}))
")
"$BIN" answer "$LIVE_KEY" true
wait $LIVE || fail "the waiting program exited non-zero"
grep -q "True" "$WORK/live.txt" || fail "long-poll did not deliver the answer: $(cat "$WORK/live.txt")"
echo "woken by the answer: $(cat "$WORK/live.txt")"

step "the outbound half: silence is an event"
$PY -c "
import interject
interject.heartbeat('etl.nightly', expect_every=1)
"
"$BIN" signals | grep -q "live" || fail "signal not reported live"
sleep 2.2
"$BIN" signals | tee "$WORK/signals.txt"
grep -q "SILENT" "$WORK/signals.txt" || fail "signal did not go silent past its deadline"

step "TTL expiry applies the declared default silently"
DEFAULTED=$($PY - <<'PY'
import time, interject
kwargs = dict(id="ttl_demo", kind="text", context={"n": 1}, ttl=1, default="skipped")
try:
    interject.ask("Anyone there?", wait=0, **kwargs)
except interject.Suspended:
    pass
time.sleep(1.4)
print(interject.ask("Anyone there?", wait=0, **kwargs))
PY
)
[ "$DEFAULTED" = "skipped" ] || fail "expected 'skipped', got '$DEFAULTED'"
echo "expired to default: $DEFAULTED"

step "triage: a calibrated class stops asking"
curl -fsS -X PUT "$INTERJECT_URL/v0/policies" \
  -H "Content-Type: application/json" -H "X-Interject-Project: $INTERJECT_PROJECT" \
  -d '{"question_id":"triage_demo","threshold":0.9,"agreement_target":0.9,"shadow_rate":0,"min_samples":3,"enabled":true}' \
  >/dev/null
AUTO=$($PY - <<'PYCAL'
import interject

SUGGEST = {"value": "car", "confidence": 0.99}
client = interject.Client()

# Three human answers agreeing with the suggestion build the agreement history.
for n in range(3):
    try:
        interject.ask("Is this a car?", options=["car", "motorcycle"], id="triage_demo",
                      context={"n": n}, suggest=SUGGEST, wait=0)
    except interject.Suspended as suspended:
        client.answer(suspended.key, "car", "e2e")

# The fourth clears every condition, so it never reaches a human.
print(interject.ask("Is this a car?", options=["car", "motorcycle"], id="triage_demo",
                    context={"n": 99}, suggest=SUGGEST, wait=0))
PYCAL
)
[ "$AUTO" = "car" ] || fail "expected the fourth question to be auto-answered, got '$AUTO'"
echo "answered without a human: $AUTO"
"$BIN" calibration

printf '\n\033[32mALL GOOD — the loop closes.\033[0m\n'
