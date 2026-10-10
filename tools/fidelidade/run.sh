#!/usr/bin/env bash
# Teste de fidelidade do xlsconvert contra o pandas do conversor de referência.
#
# Uso, na máquina que tem o pandas de referência e os arquivos a comparar:
#
#   run.sh <dir_de_ensaio> [--valores] <arquivo.xls>...
#
# <dir_de_ensaio> precisa conter o binário `xlsconvert` (musl) e o
# `compare.py`. O serviço sobe só em 127.0.0.1, numa porta de ensaio, com um
# token aleatório que não aparece na linha de comando, e é encerrado no fim.
# Nenhum arquivo de entrada é copiado ou movido: só lido. As saídas ficam num
# diretório temporário apagado no fim. O relatório agregado vai para a saída
# padrão; sem --valores ele não traz conteúdo de célula.
set -euo pipefail

DIR=${1:?informe o diretório de ensaio}
shift
VALORES=()
if [ "${1:-}" = "--valores" ]; then
  VALORES=(--valores)
  shift
fi
[ "$#" -gt 0 ] || { echo "informe ao menos um arquivo" >&2; exit 2; }

PORTA=${XLSCONVERT_ENSAIO_PORTA:-9329}
TMP=$(mktemp -d)
chmod 700 "$TMP"
TOKEN=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')

limpar() {
  [ -n "${PID:-}" ] && kill "$PID" 2>/dev/null || true
  rm -rf "$TMP"
}
trap limpar EXIT

XLSCONVERT_TOKEN="$TOKEN" XLSCONVERT_BIND="127.0.0.1:$PORTA" XLSCONVERT_LOG=warn \
  "$DIR/xlsconvert" &
PID=$!
for _ in $(seq 1 50); do
  curl -fsS "http://127.0.0.1:$PORTA/health" >/dev/null 2>&1 && break
  sleep 0.1
done

# O token vai por arquivo de header (-H @arquivo), fora da linha de comando.
printf 'X-Internal-Token: %s\n' "$TOKEN" > "$TMP/h"
: > "$TMP/pares.tsv"
N=0
T0=$(date +%s)
for f in "$@"; do
  N=$((N + 1))
  OUT="$TMP/saida-$N.xlsx"
  STATUS=$(curl -sS -o "$OUT" -w '%{http_code}' -H @"$TMP/h" \
    --data-binary @"$f" "http://127.0.0.1:$PORTA/convert" || true)
  [ "$STATUS" = "200" ] || rm -f "$OUT"
  printf '%s\t%s\n' "$f" "$OUT" >> "$TMP/pares.tsv"
done
echo "conversoes_rust: $N em $(( $(date +%s) - T0 ))s" >&2
RESULTADO=${XLSCONVERT_ENSAIO_RESULTADO:-$DIR/resultado.jsonl}
python3 -I "$DIR/compare.py" --manifesto "$TMP/pares.tsv" --resultado "$RESULTADO" "${VALORES[@]}"
