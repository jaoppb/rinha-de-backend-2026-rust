#!/usr/bin/env bash

set -e

# Configurações de arquivos
LOG_FILE="resource_usage.log"
STATS_FILE="test_stats.log"

echo "🚀 Iniciando stack..."
docker compose up -d

echo "📊 Monitoramento iniciado em $(date)"
echo -n "" >"$LOG_FILE"

monitor_containers() {
	while true; do
		docker compose stats --no-stream --no-trunc --format "json" >>"$LOG_FILE" 2>/dev/null || true
		sleep 1
	done
}

monitor_containers &
MONITOR_PID=$!

echo "🔥 Iniciando teste de carga (make test)..."
make test || echo "⚠️ Os testes retornaram erro, mas continuarei para gerar as estatísticas."

echo "🛑 Teste de carga finalizado. Parando monitoramento..."
kill $MONITOR_PID
wait $MONITOR_PID 2>/dev/null || true

echo "📈 Gerando estatísticas em $STATS_FILE..."
echo "--- Estatísticas de Uso de Recursos (Média e Máximo) ---" >"$STATS_FILE"

# Processamento JSON via JQ com formatação final mantida pelo awk
jq -r -s '
  # Filtra leituras inválidas ou iniciais (ex: "--")
  map(select(.CPUPerc != null and .CPUPerc != "--" and .MemPerc != "--")) |
  # Agrupa os objetos de log pelo nome do container
  group_by(.Name)[] |
  {
    name: .[0].Name,
    # Remove o caractere "%" e converte para número
    cpus: map(.CPUPerc | sub("%"; "") | tonumber),
    mems: map(.MemPerc | sub("%"; "") | tonumber)
  } |
  # Extrai os cálculos: nome, media cpu, max cpu, media mem, max mem
  [
    .name,
    (.cpus | add / length),
    (.cpus | max),
    (.mems | add / length),
    (.mems | max)
  ] | @tsv
' "$LOG_FILE" | awk -F'\t' '{
    # Retorna o printf original para manter o espaçamento visual perfeito (ex: %6.2f)
    printf "Container: %-25s | CPU Avg: %6.2f%% (Max: %6.2f%%) | Mem Avg: %6.2f%% (Max: %6.2f%%)\n", 
      $1, $2, $3, $4, $5
}' | sort >>"$STATS_FILE"

echo "--------------------------------------------------------" >>"$STATS_FILE"
echo "🔍 Diagnósticos de Saúde e Logs:" >>"$STATS_FILE"

EXITED_CONTAINERS=$(docker compose ps -a --format "{{.Name}}: {{.Status}}" | grep -E "Exited|Dead" || true)
if [ -n "$EXITED_CONTAINERS" ]; then
	echo "🚨 ATENÇÃO: Containers que finalizaram:" >>"$STATS_FILE"
	echo "$EXITED_CONTAINERS" >>"$STATS_FILE"
else
	echo "✅ Todos os containers permaneceram ativos." >>"$STATS_FILE"
fi

echo "--- Resumo de Erros nos Logs ---" >>"$STATS_FILE"
docker compose logs --tail 1000 | grep -Ei "error|panic|fatal|out of memory|oom-kill" | grep -v "warmup" | tail -n 10 >>"$STATS_FILE" || true
echo "--------------------------------------------------------" >>"$STATS_FILE"

if [ -f "test/results.json" ]; then
	echo "🏆 Score Final:" >>"$STATS_FILE"
	grep -E "\"p99\"|\"failure_rate\"|\"final_score\"" test/results.json | sed 's/[",]//g' >>"$STATS_FILE" || true
	echo "--------------------------------------------------------" >>"$STATS_FILE"
fi

cat "$STATS_FILE"
docker compose down
echo "✅ Concluído!"
