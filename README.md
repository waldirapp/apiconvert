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

Divergência conhecida: o openpyxl grava como célula do tipo erro o texto que é exatamente um código de erro (`#NULL!`, `#DIV/0!`, `#VALUE!`, `#REF!`, `#NAME?`, `#NUM!`); o rust_xlsxwriter não tem API pública para célula de erro, então esse texto sai como texto, com o mesmo valor lido.

## Imagem em WebP (`POST /webp`)

Converte uma imagem em WebP com o resultado do `cwebp` que o x-duall usa (`cwebp -q 50 -m 6 -jpeg_like -mt -resize`, exceto `-m 4` em imagem com transparência, ver abaixo; com o lado maior em 400 px e o outro proporcional; imagem quadrada vai a 400×400 e imagem menor é ampliada) e devolve o WebP em base64, o formato que o x-duall grava no campo da imagem. A libwebp é compilada dentro do binário; não há processo externo.

| Item | Contrato |
|---|---|
| Autenticação | Header `X-Internal-Token`. No modo `interno`, o `XLSCONVERT_TOKEN`; no modo `apiconvert`, o `XLSCONVERT_WEBP_TOKEN`, e sem ele a rota não existe |
| Corpo | Multipart com o arquivo (o primeiro campo com conteúdo) ou os bytes da imagem direto |
| Entrada | JPEG, PNG, GIF, BMP, TIFF, WebP e ICO, detectados pelo conteúdo |
| Query | `max`: lado maior (padrão `400`; `0` mantém o tamanho; até `4096`). `q`: qualidade de 0 a 100 (padrão `50`) |
| 200 | `text/plain; charset=utf-8`: WebP em base64 padrão, sem quebra de linha |
| Erros | JSON `{"erro", "codigo"}`: 400 `imagem_vazia` ou `parametro_invalido`, 401 `nao_autorizado`, 413 `imagem_grande_demais`, 415 `formato_nao_suportado`, 422 `imagem_invalida`, 503 `ocupado`, 504 `tempo_esgotado` |

Divergência intencional: imagem com transparência usa `-m 4` em vez de `-m 6`. No método 6 a compressão sem perdas do canal alfa levava de 1,4 a 6 s por imagem no servidor (o próprio `cwebp` do legado leva 3,5 s numa delas); no 4 leva milissegundos, com arquivo cerca de 5% maior e sem diferença visível. Imagem sem transparência (inclusive PNG com alfa todo opaco) segue no método 6.

Comparado com o `cwebp` 1.2.1 do legado nas mesmas 14 imagens (PNG e JPEG, de 32×32 a 3840×2160), as dimensões foram iguais em todas e o tamanho variou menos de 0,5%. Nas PNG o resultado decodificado foi idêntico pixel a pixel; nas JPEG a diferença vem do decodificador de JPEG, com PSNR entre as duas saídas de 36 a 46 dB. `examples/webp_fidelidade.rs` refaz a comparação.

## Telas web

No modo `apiconvert` o serviço também serve duas telas para testar as conversões no navegador, feitas com askama e htmx 4 e embutidas no binário:

| Página | Uso |
|---|---|
| `/planilha` | Envia um `.xls`/`.xlsx`, mostra linhas, colunas, tempo e uma prévia das primeiras células, e baixa o `.xlsx` |
| `/imagem` | Envia uma imagem, mostra o original e o WebP lado a lado, tamanho, redução, se houve transparência (método 4 ou 6), o base64 com botão de copiar, e baixa o `.webp` |

As telas são abertas, sem login: não há segredo envolvido, e a conversão não acessa banco, disco nem outro serviço. O resultado fica em memória por 10 minutos e só pode ser baixado pelo id aleatório (128 bits) devolvido a quem enviou; a tela não lista conversões e não usa o download por nome do contrato legado. As conversões pela tela têm limite por IP (`XLSCONVERT_UI_RATE_PER_MIN`) e dividem o teto de conversões simultâneas com as rotas de planilha e imagem. As respostas levam `Content-Security-Policy` restritiva (só scripts e estilos do próprio serviço), `nosniff` e `frame-ancestors 'none'`.

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
| `XLSCONVERT_MAX_CONCURRENT` | `2` | Conversões simultâneas, somando planilha e imagem |
| `XLSCONVERT_TIMEOUT_SECS` | `60` | Tempo máximo de resposta de uma conversão |
| `XLSCONVERT_MAX_CELLS` | `5000000` | Teto de células da primeira aba |
| `XLSCONVERT_LOG` | `info` | Filtro de log (`tracing`) |
| `XLSCONVERT_WEBP_TOKEN` | — | Só no modo `apiconvert`: token da rota `/webp` (mínimo 32 caracteres). Sem ele a rota não é registrada |
| `XLSCONVERT_UI` | `1` no `apiconvert`, `0` no `interno` | Liga ou desliga as telas web |
| `XLSCONVERT_UI_RATE_PER_MIN` | `30` | Conversões por minuto, por IP, nas telas |
| `XLSCONVERT_WEBP_MAX_PIXELS` | `25000000` (5000×5000; cabe com folga no `MemoryMax=1G` da unit com 2 conversões simultâneas) | Teto de largura × altura da imagem recebida em `/webp` |

O log registra tamanho, duração e resultado, nunca o conteúdo nem o nome original do arquivo.

## Build e testes

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --target x86_64-unknown-linux-musl
```

O build musl compila a libwebp em C e precisa do `musl-gcc` (pacote `musl-tools` no Ubuntu); a CI publica o binário como artefato `xlsconvert-x86_64-linux-musl`.

O binário musl é estático e não depende da glibc do sistema. As fixtures de teste são sintéticas; `tests/fixtures/tipos.xls` é gerado de `tests/fixtures/tipos.fods` com `libreoffice --headless --convert-to 'xls:MS Excel 97' tipos.fods`.

## Licença

[MIT](LICENSE)
