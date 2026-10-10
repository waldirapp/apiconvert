#!/usr/bin/env bash
# Troca o conversor legado (unit apiconvert) pelo xlsconvert em modo
# apiconvert na mesma porta, com verificação e rollback automáticos.
#
# Uso, como root, com o binário novo já em /opt/xlsconvert/xlsconvert.new e a
# unit xlsconvert-apiconvert instalada (ver deploy/README.md):
#
#   bash trocar.sh <planilha_de_teste.xls>
#
# A planilha de teste é enviada ao serviço novo depois da troca; se o POST ou
# o GET falharem, o legado volta automaticamente.
set -Eeuo pipefail

TESTE=${1:?informe uma planilha .xls de teste}
LEGADO=apiconvert
NOVO=xlsconvert-apiconvert
BIN=/opt/xlsconvert/xlsconvert
PORTA=9098

log() { echo "[$(date +%T)] $*"; }

rollback() {
  log "FALHA -- voltando para o $LEGADO"
  systemctl disable --now "$NOVO" 2>/dev/null || true
  [ -f "$BIN.anterior" ] && mv -f "$BIN.anterior" "$BIN"
  systemctl enable --now "$LEGADO"
  sleep 2
  systemctl is-active "$LEGADO"
  log "rollback concluído"
  exit 1
}
trap rollback ERR

[ -x "$BIN.new" ] || { echo "binário novo ausente em $BIN.new"; exit 2; }
[ -f "/etc/systemd/system/$NOVO.service" ] || { echo "unit $NOVO ausente"; exit 2; }

log "1/5 binário novo: $(sha256sum "$BIN.new" | cut -c1-16)"
[ -f "$BIN" ] && cp -a "$BIN" "$BIN.anterior"
install -m 755 "$BIN.new" "$BIN"
rm -f "$BIN.new"
systemctl daemon-reload

log "2/5 parando o $LEGADO"
systemctl disable --now "$LEGADO"
for _ in $(seq 1 20); do ss -tln | grep -q ":$PORTA " || break; sleep 0.5; done
! ss -tln | grep -q ":$PORTA "

log "3/5 subindo o $NOVO"
systemctl enable --now "$NOVO"
for _ in $(seq 1 20); do ss -tln | grep -q ":$PORTA " && break; sleep 0.5; done
systemctl is-active "$NOVO"

log "4/5 teste ponta a ponta na porta $PORTA"
NOME=$(curl -fsS -F "x=@$TESTE;filename=troca-verificacao.xls" "http://127.0.0.1:$PORTA/convert")
[ "$NOME" = "troca-verificacao.xlsx" ]
OUT=$(mktemp)
curl -fsS -D "$OUT.h" -o "$OUT" "http://127.0.0.1:$PORTA/convert?file=$NOME"
grep -qi '^content-disposition: attachment; filename="troca-verificacao.xlsx"' "$OUT.h"
head -c 4 "$OUT" | od -An -tx1 | grep -q "50 4b 03 04"
rm -f "$OUT" "$OUT.h"

log "5/5 concluído: $NOVO ativo na porta $PORTA, $LEGADO desabilitado"
trap - ERR
systemctl status "$NOVO" --no-pager | head -5
