#!/usr/bin/env bash
# EvolveRoute M1 latency baseline.
# Measures added latency of the gateway vs direct mock-upstream access
# on loopback. Requires the built binary (cargo build).
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=./target/debug/evolveroute
N=${N:-100}
MOCK_PORT=9101
GW_PORT=8787

pkill -f "evolveroute mock-upstream" 2>/dev/null || true
pkill -f "evo-router serve" 2>/dev/null || true
sleep 0.2

nohup $BIN mock-upstream --port $MOCK_PORT >/tmp/bench-mock.log 2>&1 &
MOCK_PID=$!
nohup $BIN serve --port $GW_PORT >/tmp/bench-serve.log 2>&1 &
GW_PID=$!
trap 'kill $MOCK_PID $GW_PID 2>/dev/null || true' EXIT

for i in $(seq 1 40); do
  curl -s -m 1 "http://127.0.0.1:$GW_PORT/healthz" >/dev/null 2>&1 && break
  sleep 0.25
done

BODY='{"model":"auto","messages":[{"role":"user","content":"hi"}]}'
DIRECT_BODY='{"model":"mock-mini","messages":[{"role":"user","content":"hi"}]}'

bench() { # url body n -> ms list on stdout
  local url=$1 body=$2 n=$3
  for _ in $(seq 1 "$n"); do
    curl -s -o /dev/null -w '%{time_total}\n' -X POST "$url" \
      -H 'content-type: application/json' -H 'x-ev-session: bench' -d "$body"
  done
}

echo "benchmark: $N requests, trivial 1-token chat, loopback"
direct_ms=$(bench "http://127.0.0.1:$MOCK_PORT/v1/chat/completions" "$DIRECT_BODY" "$N")
gw_ms=$(bench "http://127.0.0.1:$GW_PORT/v1/chat/completions" "$BODY" "$N")

stats() {
  python3 -c '
import sys, statistics
xs = sorted(float(x)*1000 for x in sys.stdin)
p = lambda q: xs[min(int(q*len(xs)), len(xs)-1)]
print(f"  p50={statistics.median(xs):.2f}ms  p95={p(0.95):.2f}ms  p99={p(0.99):.2f}ms  mean={statistics.mean(xs):.2f}ms")
'
}
echo "direct upstream:"
echo "$direct_ms" | stats
echo "via gateway (routing + rewrite + relay):"
echo "$gw_ms" | stats

python3 -c "
import sys, statistics
a = sorted(float(x)*1000 for x in '''$direct_ms'''.split())
b = sorted(float(x)*1000 for x in '''$gw_ms'''.split())
am, bm = statistics.median(a), statistics.median(b)
print(f'added latency (median): {bm-am:.2f}ms  (budget p50 < 5ms: {\"PASS\" if bm-am < 5 else \"FAIL\"})')
"
