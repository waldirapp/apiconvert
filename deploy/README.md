# Implantação no lugar do conversor legado (modo `apiconvert`)

O modo `apiconvert` reproduz o contrato HTTP do conversor legado (multipart em `POST /convert`, download em `GET /convert?file=`), então os clientes não mudam. A diferença é interna: sem Python, sem gravar arquivo em disco, sem acumular temporários, sem root e sem transformar o nome enviado pelo cliente em caminho de arquivo.

## Pré-requisito

O binário `x86_64-unknown-linux-musl` (estático, roda em qualquer Linux x86-64):

```bash
cargo build --release --locked --target x86_64-unknown-linux-musl
```

## Instalação, como root

```bash
useradd -r -s /usr/sbin/nologin -d /nonexistent xlsconvert
install -d -m 755 /opt/xlsconvert
install -m 755 target/x86_64-unknown-linux-musl/release/xlsconvert /opt/xlsconvert/xlsconvert
install -m 644 deploy/xlsconvert-apiconvert.service /etc/systemd/system/xlsconvert-apiconvert.service
systemctl daemon-reload
```

## Ensaio antes da troca

Sobe o serviço numa porta paralela, sem tocar no legado, e confirma que a unit funciona com o endurecimento do systemd no servidor:

```bash
systemd-run --unit=xlsconvert-ensaio --property=User=xlsconvert --property=ProtectSystem=strict \
  --setenv=XLSCONVERT_MODE=apiconvert --setenv=XLSCONVERT_BIND=127.0.0.1:9198 /opt/xlsconvert/xlsconvert
curl -sS -F "x=@planilha.xls;filename=ensaio.xls" http://127.0.0.1:9198/convert
systemctl stop xlsconvert-ensaio
```

## Troca

`deploy/trocar.sh` faz a troca com verificação ponta a ponta e volta ao legado sozinho se algo falhar (binário novo em `/opt/xlsconvert/xlsconvert.new`):

```bash
bash deploy/trocar.sh planilha_de_teste.xls
```

Passo a passo manual equivalente:

```bash
systemctl stop <servico-legado> && systemctl disable <servico-legado>
systemctl enable --now xlsconvert-apiconvert
systemctl status xlsconvert-apiconvert --no-pager
```

## Rollback

```bash
systemctl disable --now xlsconvert-apiconvert
systemctl enable --now <servico-legado>
```

## Rota de imagem (`/webp`)

A rota só existe com `XLSCONVERT_WEBP_TOKEN` definido. O token fica num arquivo legível só pelo root, carregado pela unit. O comando abaixo troca só essa linha e preserva as outras variáveis do arquivo:

```bash
install -d -m 755 /etc/xlsconvert
f=/etc/xlsconvert/xlsconvert.env
(umask 077; touch "$f"; sed -i '/^XLSCONVERT_WEBP_TOKEN=/d' "$f"; printf 'XLSCONVERT_WEBP_TOKEN=%s\n' "$(openssl rand -hex 32)" >> "$f")
systemctl restart xlsconvert-apiconvert
journalctl -u xlsconvert-apiconvert -n 5 --no-pager   # "rota /webp ativa"
```

O mesmo valor vai na configuração do cliente, que o envia no header `X-Internal-Token`.

## Telas web

Com o modo `apiconvert`, `http://<servidor>:9098/` abre as telas de teste de planilha e de imagem (ver o README principal). Para desligar, `XLSCONVERT_UI=0` em `/etc/xlsconvert/xlsconvert.env` e `systemctl restart xlsconvert-apiconvert`.

## Operação

- Logs: `journalctl -u xlsconvert-apiconvert -f` (tamanho, duração e resultado de cada conversão; nunca o conteúdo nem o nome do arquivo).
- Os resultados ficam disponíveis para `GET` por 1 hora (`XLSCONVERT_COMPAT_TTL_SECS`), com teto de 256 MB (`XLSCONVERT_COMPAT_MAX_STORE_MB`) e 2.000 arquivos (`XLSCONVERT_COMPAT_MAX_ENTRIES`). Reiniciar o serviço descarta os resultados ainda não baixados.
