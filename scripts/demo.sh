#!/usr/bin/env bash
# A five-minute tour of interject, told through the failure that motivated it.
#
#   scripts/demo.sh            # runs everything, cleans up after itself
#   PORT=8899 scripts/demo.sh  # if 8791 is busy
#
# Nothing here is staged: it starts a real daemon, runs real clients against it,
# and prints what actually comes back.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
PORT="${PORT:-8791}"
WORK="$(mktemp -d)"
export INTERJECT_URL="http://127.0.0.1:${PORT}"
export INTERJECT_PROJECT="autosniper"
export PYTHONPATH="$ROOT/python/src"
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

bold()  { printf '\033[1m%s\033[0m\n' "$1"; }
dim()   { printf '\033[2m%s\033[0m\n' "$1"; }
green() { printf '\033[32m%s\033[0m\n' "$1"; }
act()   { printf '\n\033[1;4m%s\033[0m\n\n' "$1"; }
pause() { sleep "${DEMO_PAUSE:-0.4}"; }

(cd rust && cargo build --quiet)
BIN="$ROOT/rust/target/debug/interjectd"
"$BIN" serve --addr "127.0.0.1:${PORT}" --db "$WORK/demo.sqlite3" --sweep-secs 1 \
  >"$WORK/daemon.log" 2>&1 &
DAEMON=$!
for _ in $(seq 60); do
  curl -fsS "$INTERJECT_URL/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done

cat <<'INTRO'

  interject — a durable ask() primitive

  A used-car pricing pipeline runs every five minutes on a laptop. Most of what
  it does needs no human. A handful of listings a day are genuinely ambiguous,
  and getting those wrong is how it once priced a motorcycle like an Accord and
  woke someone up about a 53% "deal".

INTRO
pause

# ---------------------------------------------------------------------------
act "1. The pipeline hits something it cannot decide, and does not block"

# Seed one previously-answered listing, so the contrast is visible: the system
# only stops on what it genuinely cannot call.
$PY - >/dev/null <<'SEEDEOF'
import interject
listing = {"item_id": "1939127040180729", "title": "2014 Ford Focus SEL", "price": 5900}
try:
    interject.ask("What kind of vehicle is this?",
                  options=["car", "truck", "van", "suv", "motorcycle", "boat", "trailer"],
                  id="vehicle_type", context=listing,
                  batch_key="autosniper.vehicle_type", wait=0)
except interject.Suspended as suspended:
    interject.Client().answer(suspended.key, "car", "john")
SEEDEOF

dim "$ $PY pricing_pass.py"
$PY - <<'PYEOF'
import interject

LISTINGS = [
    {"item_id": "4464948307163199", "title": "2018 Honda CBR 500R", "price": 4200},
    {"item_id": "1939127040180729", "title": "2014 Ford Focus SEL",  "price": 5900},
    {"item_id": "2204522610293472", "title": "Shoreline 14ft w/ 9.9", "price": 2400},
]

priced, waiting = 0, 0
for listing in LISTINGS:
    try:
        kind = interject.ask(
            "What kind of vehicle is this?",
            options=["car", "truck", "van", "suv", "motorcycle", "boat", "trailer"],
            id="vehicle_type",
            context=listing,
            batch_key="autosniper.vehicle_type",
            wait=0,
        )
        priced += 1
        print(f"  priced   {listing['title']:<24} as {kind}")
    except interject.Suspended:
        waiting += 1
        print(f"  waiting  {listing['title']:<24} needs a human")

print(f"\n  {priced} priced, {waiting} parked. The pass finished; nothing is blocked.")
PYEOF
pause

# ---------------------------------------------------------------------------
act "2. It reaches you as one batch, not three notifications"

dim "$ interjectd inbox"
"$BIN" inbox
pause

# ---------------------------------------------------------------------------
act "3. You answer. The next run resumes exactly where it stopped."

KEYS=$($PY - <<'PYEOF'
import interject
for listing in [
    {"item_id": "4464948307163199", "title": "2018 Honda CBR 500R", "price": 4200},
    {"item_id": "2204522610293472", "title": "Shoreline 14ft w/ 9.9", "price": 2400},
]:
    print(interject.key_for("vehicle_type", listing))
PYEOF
)
set -- $KEYS
dim "$ interjectd answer ${1:0:12} motorcycle"
"$BIN" answer "$1" motorcycle
dim "$ interjectd answer ${2:0:12} boat"
"$BIN" answer "$2" boat
echo
dim "$ $PY pricing_pass.py     # the same script, run again"
$PY - <<'PYEOF'
import interject

LISTINGS = [
    {"item_id": "4464948307163199", "title": "2018 Honda CBR 500R", "price": 4200},
    {"item_id": "1939127040180729", "title": "2014 Ford Focus SEL",  "price": 5900},
    {"item_id": "2204522610293472", "title": "Shoreline 14ft w/ 9.9", "price": 2400},
]
for listing in LISTINGS:
    try:
        kind = interject.ask(
            "What kind of vehicle is this?",
            options=["car", "truck", "van", "suv", "motorcycle", "boat", "trailer"],
            id="vehicle_type", context=listing,
            batch_key="autosniper.vehicle_type", wait=0,
        )
        verdict = "priced" if kind in {"car", "truck", "van", "suv"} else "dropped"
        print(f"  {verdict:<8} {listing['title']:<24} ({kind})")
    except interject.Suspended:
        print(f"  waiting  {listing['title']}")
PYEOF
dim "  No re-asking: the answers are keyed by content, so a rerun replays them."
pause

# ---------------------------------------------------------------------------
act "4. The other half: a job that goes quiet when it owed you a signal"

dim "  The real incident: a scraper hung at 20:50 holding a lock. Every run for"
dim "  the next three days saw the lock, exited, and told nobody. 2,765 no-ops."
echo
dim "$ # the scan declares what it owes you, once"
$PY -c "
import interject
interject.heartbeat('autosniper.scan', expect_every=2)
print('  autosniper.scan — expects to report every 2s')
"
dim "$ # ...and then it hangs. We simply stop reporting."
sleep 3.2
dim "$ interjectd signals"
"$BIN" signals
pause

# ---------------------------------------------------------------------------
act "5. It learns to stop asking you"

dim "  The pipeline already has a keyword rule that proposes an answer. Attach it"
dim "  as a suggestion, and let interject measure whether it deserves trust."
echo
curl -fsS -X PUT "$INTERJECT_URL/v0/policies" \
  -H "Content-Type: application/json" -H "X-Interject-Project: $INTERJECT_PROJECT" \
  -d '{"question_id":"title_language","threshold":0.9,"agreement_target":0.9,"shadow_rate":0,"min_samples":3,"enabled":true}' >/dev/null
dim "$ interjectd policy set title_language --enable --min-samples 3"
echo
$PY - <<'PYEOF'
import interject

client = interject.Client()
SUGGEST = {"value": "english", "confidence": 0.97}

for n, title in enumerate(["2011 Ford Focus", "2013 Civic LX", "2009 Corolla"]):
    try:
        interject.ask("What language is this listing in?", options=["english", "french"],
                      id="title_language", context={"title": title},
                      suggest=SUGGEST, wait=0)
    except interject.Suspended as suspended:
        client.answer(suspended.key, "english", "john")
        print(f"  asked, and you agreed:  {title}")

answer = interject.ask("What language is this listing in?", options=["english", "french"],
                       id="title_language", context={"title": "2016 Mazda 3 GS"},
                       suggest=SUGGEST, wait=0)
print(f"\n  not asked at all:       2016 Mazda 3 GS -> {answer}")
PYEOF
echo
dim "$ interjectd calibration"
"$BIN" calibration
echo
dim "  Agreement is measured, not asserted: every question that carried a"
dim "  suggestion and got a human answer is a free comparison. Once a class stops"
dim "  being shown to you, a sample is still asked anyway to keep it honest."
pause

# ---------------------------------------------------------------------------
act "And from a phone"

"$BIN" pair --project "$INTERJECT_PROJECT" --db "$WORK/demo.sqlite3" --label phone
echo
dim "  Open $INTERJECT_URL, type the code, and the questions above are answerable"
dim "  with one tap. The daemon serves that page itself — no hosting, no build."
echo
green "  That is the whole idea: it weaves in only where judgment is required,"
green "  and it works to need you less."
echo
