# xlsconvert

Serviço HTTP em Rust que converte planilha `.xls` ou `.xlsx` em `.xlsx` com **o mesmo resultado** desta conversão em pandas:

```python
df = pd.read_excel(arquivo)
df.to_excel(saida, index=False)
```

O alvo é pandas 2.2.3 com xlrd 2.0.1 (para `.xls`) e openpyxl 3.1.5. O serviço substitui um conversor em Python sem mudar o arquivo entregue a quem o consome: binário único, estático (musl), sem Python, sem gravar arquivo em disco.

## Contrato da conversão

O resultado reproduz o comportamento observável daquelas duas linhas, inclusive as peculiaridades:

| Regra | Efeito |
|---|---|
| Só a primeira aba | As outras abas são ignoradas; a saída tem uma aba `Sheet1` |
| A 1ª linha vira nomes de coluna | Gravados na 1ª linha da saída em negrito, com borda fina e centralizados. Vazio vira `Unnamed: N`; repetido ganha sufixo (`X`, `X.1`, `X.2`, pulando nomes que já existem); número e data mantêm o tipo |
| Posições absolutas | Linhas e colunas vazias à esquerda, acima e no meio dos dados são mantidas (a coluna vazia à esquerda vira `Unnamed: N` no cabeçalho) |
| Nulos | Vazio, `NA`, `N/A`, `NULL`, `nan`, `None`, `#N/A` e similares viram célula vazia |
| Coluna só com números ou texto numérico | Vira número: `"00123"` vira `123`. Com nulo, vira float |
| Inteiro fora de `uint64` em texto | A coluna continua texto (ex.: chave de NF-e com 44 dígitos) |
| Coluna só com booleanos ou `True`/`False` em texto | Vira booleano |
| Datas | Número serial com formato `YYYY-MM-DD HH:MM:SS` |
| Horas (data no dia-época) | **Viram texto** `HH:MM:SS` |
| `inf` / `-inf` | Viram os textos `inf` e `-inf` |
| Texto começando com `=` | Gravado como fórmula (comportamento do openpyxl) |
| Caractere de controle ilegal no texto | Recusa (no openpyxl é `IllegalCharacterError`) |
| Números e datas | Gravados com 16 dígitos significativos (`"%.16g"` do openpyxl) |
| `.xls` cujo fluxo `Workbook` ou alguma aba não começa com BOF | Recusa, como o xlrd (`Expected BOF record`) |

Cada regra cita, no código, a função do pandas, do xlrd ou do openpyxl que reproduz (`src/convert/read.rs`, `header.rs`, `infer.rs` e `write.rs`).

## Fidelidade

`tools/fidelidade/` compara o serviço com o pandas de referência, célula a célula, nos arquivos que se quer validar:

```bash
bash tools/fidelidade/run.sh <dir_com_binario_e_compare.py> arquivo1.xls arquivo2.xls ...
```

O script sobe o serviço só em `127.0.0.1`, com token aleatório fora da linha de comando, converte tudo, compara com o pandas num único processo Python e imprime um resumo agregado. Sem `--valores`, o relatório não traz conteúdo de célula, o que permite rodá-lo sobre arquivos sensíveis sem tirá-los da máquina.

Numa base de 1.150 arquivos `.xls` reais, o resultado foi idêntico ao do pandas em todos: 1.146 convertidos sem nenhuma diferença de célula e 4 recusados pelos dois lados. Contra a saída do próprio conversor legado em produção (mesmo arquivo enviado aos dois), não houve diferença de valor, tipo, formato de número, negrito, borda nem alinhamento.

## Modos

| `XLSCONVERT_MODE` | Uso |
|---|---|
| `interno` (padrão) | API própria, descrita abaixo, para ficar atrás de outro serviço na mesma máquina, com token obrigatório |
| `apiconvert` | Contrato HTTP idêntico ao do conversor legado (multipart + download por nome), para substituí-lo sem mudar os clientes. Ver `src/compat.rs` e [deploy/README.md](deploy/README.md) |

## HTTP (modo interno)

| Rota | Autenticação | Resposta |
|---|---|---|
| `GET /health` | Nenhuma | `{"status":"ok"}` |
| `POST /convert` | Header `X-Internal-Token` | Corpo: bytes do `.xls`/`.xlsx`. Resposta 200: bytes do `.xlsx` (`no-store`) |

Erros em JSON `{"erro": "...", "codigo": "..."}`:

| Status | `codigo` |
|---|---|
| 400 | `arquivo_vazio` |
| 401 | `nao_autorizado` |
| 413 | `planilha_grande_demais` (ou corpo acima do limite) |
| 415 | `formato_nao_suportado` (só `.xls` BIFF e `.xlsx` ZIP, detectados pelo conteúdo) |
| 422 | `planilha_invalida`, `conteudo_invalido` |
| 503 | `ocupado` (conversões simultâneas no limite) |
| 504 | `tempo_esgotado` |

O serviço foi feito para ficar atrás de outro serviço na mesma máquina, que cuida da autenticação dos usuários. Ele não deve ser exposto diretamente na rede.

## Configuração

| Variável | Padrão | Uso |
|---|---|---|
| `XLSCONVERT_TOKEN` | — (obrigatória, mínimo 32 caracteres) | Token interno compartilhado com o serviço chamador. Sem ela o serviço não sobe |
| `XLSCONVERT_BIND` | `127.0.0.1:9320` | Endereço de escuta. Fora do loopback, o serviço avisa no log |
| `XLSCONVERT_MAX_BODY_MB` | `20` | Tamanho máximo do upload |
| `XLSCONVERT_MAX_CONCURRENT` | `2` | Conversões simultâneas |
| `XLSCONVERT_TIMEOUT_SECS` | `60` | Tempo máximo de resposta de uma conversão |
| `XLSCONVERT_MAX_CELLS` | `5000000` | Teto de células da primeira aba |
| `XLSCONVERT_LOG` | `info` | Filtro de log (`tracing`) |

O log registra tamanho, duração e resultado, nunca o conteúdo nem o nome original do arquivo.

## Build e testes

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --target x86_64-unknown-linux-musl
```

O binário musl é estático e não depende da glibc do sistema. As fixtures de teste são sintéticas; `tests/fixtures/tipos.xls` é gerado de `tests/fixtures/tipos.fods` com `libreoffice --headless --convert-to 'xls:MS Excel 97' tipos.fods`.

## Licença

[MIT](LICENSE)
