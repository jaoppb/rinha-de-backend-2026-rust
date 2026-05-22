#!/usr/bin/env bash

set -euo pipefail

LOG_FILE="monitor_logs.log"
STATS_FILE="test_stats.log"
START_TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
TEST_STATUS=0

export LOG_TRANSPORT="${LOG_TRANSPORT:-json}"

echo "🚀 Iniciando stack..."
docker compose up -d

echo "⏳ Aguardando serviços ficarem prontos (smoke test)..."
until make smoke > /dev/null 2>&1; do
	echo "   ...aguardando backend responder corretamente..."
	sleep 2
done
echo "✅ Serviços prontos!"

echo "📊 Monitoramento iniciado em $(date)"
: >"$LOG_FILE"

echo "🔥 Iniciando teste de carga (make test)..."
make test || TEST_STATUS=$?
if [[ "$TEST_STATUS" -ne 0 ]]; then
	echo "⚠️ Os testes retornaram erro, mas continuarei para gerar as estatísticas."
fi

echo "🛑 Teste de carga finalizado. Coletando logs..."
docker compose logs --no-color --since "$START_TS" lb api1 api2 >"$LOG_FILE" || true

python3 - "$LOG_FILE" "$STATS_FILE" <<'PY'
import json
import re
import sys

log_file, stats_file = sys.argv[1], sys.argv[2]
service_order = ["lb", "api1", "api2"]
line_re = re.compile(r"^(?P<service>[^|]+?)\s*\|\s*(?P<payload>\{.*\})$")
samples = {}

with open(log_file, "r", encoding="utf-8", errors="replace") as fh:
    for raw in fh:
        raw = raw.rstrip("\n")
        match = line_re.match(raw)
        if not match:
            continue
        service = match.group("service").strip().split("-", 1)[0]
        try:
            payload = json.loads(match.group("payload"))
        except json.JSONDecodeError:
            continue
        if payload.get("event") != "timing":
            continue

        op = payload.get("op")
        elapsed = payload.get("elapsed_us")
        if not op or elapsed is None:
            continue

        category = payload.get("category", "")
        key = (service, op)
        agg = samples.setdefault(
            key,
            {"count": 0, "sum": 0.0, "min": None, "max": None, "categories": set()},
        )
        value = float(elapsed)
        agg["count"] += 1
        agg["sum"] += value
        agg["categories"].add(category)
        agg["min"] = value if agg["min"] is None else min(agg["min"], value)
        agg["max"] = value if agg["max"] is None else max(agg["max"], value)

with open(stats_file, "w", encoding="utf-8") as out:
    out.write("--- Timing Statistics (elapsed_us) ---\n")
    if not samples:
        out.write("No timing log entries found.\n")

    for service in service_order:
        out.write(f"\nService: {service}\n")
        service_keys = [key for key in sorted(samples) if key[0] == service]
        if not service_keys:
            out.write("  no timing samples found\n")
            continue

        for _, op in service_keys:
            agg = samples[(service, op)]
            avg = agg["sum"] / agg["count"]
            categories = ",".join(sorted(c for c in agg["categories"] if c)) or "-"
            out.write(
                f"  op={op:<24} categories={categories:<12} "
                f"samples={agg['count']:<4} avg_us={avg:.2f} "
                f"min_us={agg['min']:.0f} max_us={agg['max']:.0f}\n"
            )
PY

cat "$STATS_FILE"
echo "✅ Concluído!"

exit "$TEST_STATUS"
