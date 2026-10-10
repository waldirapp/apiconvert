//! Modo `apiconvert`: contrato HTTP idêntico ao do conversor legado, para
//! substituí-lo sem mudar os clientes.
//!
//! Contrato capturado do serviço legado (mesmas rotas, status, corpos e
//! headers relevantes):
//!
//! | Requisição | Resposta |
//! |---|---|
//! | `POST /convert` multipart com um arquivo | `200`, `Content-Type` do xlsx, corpo = nome do arquivo enviado com a última extensão trocada por `.xlsx`, em Latin-1 |
//! | qualquer falha no `POST` | `500`, `text/html; charset=UTF-8`, `Erro ao gravar o arquivo` |
//! | `GET /convert?file=<nome>` | `200`, xlsx, `Content-Disposition: attachment; filename="<nome>"` (pode repetir) |
//! | `GET` de nome inexistente ou sem `file` | `404`, `text/html; charset=utf-8`, `<HTML><BODY><B>404 Not Found</B></BODY></HTML>`, `Connection: close` |
//! | outra rota | `404`, `text/html; charset=UTF-8`, `Not Found`, `Connection: close` |
//! | outro método em `/convert` | `405`, `text/html; charset=UTF-8`, `Method Not Allowed` |
//!
//! O cliente legado lê o nome do arquivo do header `Content-Disposition`
//! (texto depois da primeira aspa, sem o último caractere), então esse header
//! é reproduzido exatamente.
//!
//! Diferenças intencionais, todas em caminho de falha ou de segurança:
//! - o serviço legado grava o arquivo em disco com o nome enviado pelo
//!   cliente e serve qualquer arquivo da pasta pelo `?file=`; aqui o
//!   resultado fica em memória, indexado pelo nome, e o `GET` só devolve o
//!   que o próprio serviço converteu. Nenhum nome vira caminho de arquivo;
//! - os resultados expiram (`XLSCONVERT_COMPAT_TTL_SECS`, padrão 5 minutos) e
//!   há teto de memória; o legado acumulava arquivos indefinidamente;
//! - o resultado só é entregue ao mesmo IP que fez o `POST`
//!   (`XLSCONVERT_COMPAT_BIND_IP`) e uma única vez
//!   (`XLSCONVERT_COMPAT_ONE_TIME`); um IP que acumula nomes inexistentes no
//!   `GET` passa a receber 404 sem consulta até o fim da janela
//!   (`XLSCONVERT_COMPAT_MAX_MISSES_PER_MIN`). O nome do arquivo vem do
//!   temporário do Delphi (`tmpXXXX`, 65.536 combinações), e sem isso
//!   qualquer um baixaria a planilha de outro cliente varrendo os nomes
//!   (GHSA-jc42-3ccg-wf85). O cliente (`dUploadXlsApi.UploadXlsApi`) faz o
//!   `POST` e um único `GET` em seguida, da mesma máquina;
//! - nome com acento: o legado devolvia o nome em Latin-1 e depois falhava no
//!   `GET` (500); aqui o `GET` aceita o nome em UTF-8 ou Latin-1;
//! - nome com caractere de controle ou aspas é recusado no `POST` (500), para
//!   não injetar no header `Content-Disposition`;
//! - resultado que sozinho passa do teto de armazenamento responde 500 em vez
//!   de devolver um nome que o `GET` não encontraria.
//!
//! Divergência conhecida, sem efeito prático esperado: o openpyxl grava como
//! célula do tipo erro (`t="e"`) o texto que é exatamente um código de erro
//! (`#NULL!`, `#DIV/0!`, `#VALUE!`, `#REF!`, `#NAME?`, `#NUM!`, `#N/A`); o
//! rust_xlsxwriter não tem API pública para célula de erro, então esse texto
//! sai como texto. O valor lido é o mesmo.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::multipart::MultipartRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Multipart, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use tokio::sync::Semaphore;

use crate::app::XLSX_MIME;
use crate::config::{CompatStore, Config};

const ERRO_GRAVAR: &str = "Erro ao gravar o arquivo";
const NOT_FOUND_FILE: &str = "<HTML><BODY><B>404 Not Found</B></BODY></HTML>";
const NOT_FOUND_ROUTE: &str = "Not Found";
const METHOD_NOT_ALLOWED: &str = "Method Not Allowed";

struct Entry {
    xlsx: Bytes,
    created: Instant,
}

#[derive(Default)]
struct Entries {
    /// Resultado por nome primário (o nome de saída derivado do enviado).
    primary: HashMap<String, Entry>,
    /// Nome devolvido ao cliente (Latin-1, com '?') -> nome primário. Não
    /// conta nos tetos e nunca substitui um resultado existente.
    aliases: HashMap<String, String>,
}

impl Entries {
    fn purge(&mut self, ttl: Duration) {
        self.primary.retain(|_, e| e.created.elapsed() < ttl);
        self.drop_orphan_aliases();
    }

    fn drop_orphan_aliases(&mut self) {
        let primary = &self.primary;
        self.aliases.retain(|_, p| primary.contains_key(p));
    }
}

/// Resultados convertidos, por nome, à espera do `GET`.
pub(crate) struct Store {
    cfg: CompatStore,
    entries: Mutex<Entries>,
}

impl Store {
    fn new(cfg: CompatStore) -> Self {
        Self {
            cfg,
            entries: Mutex::new(Entries::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        e.purge(self.cfg.ttl);
        e
    }

    /// Guarda o resultado; `false` quando ele sozinho passa do teto de bytes.
    fn put(&self, name: String, xlsx: Bytes) -> bool {
        if xlsx.len() > self.cfg.max_bytes {
            return false;
        }
        let mut entries = self.lock();
        let map = &mut entries.primary;
        // O mesmo nome substitui o anterior, como o arquivo sobrescrito no legado.
        map.remove(&name);
        let mut total: usize = map.values().map(|e| e.xlsx.len()).sum();
        while !map.is_empty()
            && (map.len() + 1 > self.cfg.max_entries || total + xlsx.len() > self.cfg.max_bytes)
        {
            let oldest = map
                .iter()
                .min_by_key(|(_, e)| e.created)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                if let Some(e) = map.remove(&k) {
                    total -= e.xlsx.len();
                }
            } else {
                break;
            }
        }
        map.insert(
            name,
            Entry {
                xlsx,
                created: Instant::now(),
            },
        );
        entries.drop_orphan_aliases();
        true
    }

    /// Registra `alias` para o resultado `primary`, se o nome estiver livre:
    /// um resultado ou alias existente com esse nome é preservado.
    fn put_alias(&self, alias: String, primary: &str) {
        let mut entries = self.lock();
        if entries.primary.contains_key(&alias) || entries.aliases.contains_key(&alias) {
            return;
        }
        if entries.primary.contains_key(primary) {
            entries.aliases.insert(alias, primary.to_string());
        }
    }

    /// Resultado guardado sob `name` (nome primário ou alias). Com
    /// `consume`, o resultado e os aliases dele saem do armazenamento.
    fn take(&self, name: &str, consume: bool) -> Option<Bytes> {
        let mut entries = self.lock();
        let key = if entries.primary.contains_key(name) {
            name.to_string()
        } else {
            entries.aliases.get(name)?.clone()
        };
        if consume {
            let e = entries.primary.remove(&key)?;
            entries.drop_orphan_aliases();
            Some(e.xlsx)
        } else {
            entries.primary.get(&key).map(|e| e.xlsx.clone())
        }
    }
}

/// IP de quem chamou (`ConnectInfo`); `None` sem a informação (testes).
pub(crate) struct ClientIp(pub Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip()),
        ))
    }
}

/// Nomes inexistentes pedidos por IP numa janela de 1 minuto.
struct Misses {
    max: u32,
    map: Mutex<HashMap<Option<IpAddr>, (Instant, u32)>>,
}

impl Misses {
    fn bloqueado(&self, ip: Option<IpAddr>) -> bool {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, (inicio, _)| inicio.elapsed() < Duration::from_secs(60));
        map.get(&ip).is_some_and(|(_, n)| *n >= self.max)
    }

    fn registrar(&self, ip: Option<IpAddr>) {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(ip).or_insert((Instant::now(), 0)).1 += 1;
    }
}

/// Chave do resultado: com `bind_ip`, o IP de origem faz parte dela, então
/// outro IP não alcança o resultado nem o substitui.
fn store_key(cfg: &CompatStore, ip: Option<IpAddr>, name: &str) -> String {
    if cfg.bind_ip {
        let ip = ip.map(|i| i.to_string()).unwrap_or_default();
        format!("{ip}\u{0}{name}")
    } else {
        name.to_string()
    }
}

#[derive(Clone)]
struct CompatState {
    cfg: Arc<Config>,
    permits: Arc<Semaphore>,
    store: Arc<Store>,
    misses: Arc<Misses>,
}

pub fn router(cfg: Config) -> Router {
    let admission_cfg = cfg.clone();
    let max_body = cfg.max_body_bytes;
    let permits = Arc::new(Semaphore::new(cfg.max_concurrent));
    let imagem = crate::imagem_router(&cfg, permits.clone());
    let ui = crate::ui_router(&cfg, permits.clone());
    let state = CompatState {
        permits,
        store: Arc::new(Store::new(cfg.compat)),
        misses: Arc::new(Misses {
            max: cfg.compat.max_misses_per_min,
            map: Mutex::new(HashMap::new()),
        }),
        cfg: Arc::new(cfg),
    };
    let router = Router::new()
        .route(
            "/convert",
            post(post_convert)
                .get(get_convert)
                .head(head_convert)
                .fallback(method_not_allowed),
        )
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state);
    // /webp (com token) e as telas entram antes do fallback de 404 do legado.
    let router = [imagem, ui]
        .into_iter()
        .flatten()
        .fold(router, |acc, r| acc.merge(r))
        .fallback(route_not_found);
    crate::admission::apply(router, &admission_cfg)
}

/// Resposta em HTML com o `Content-Type` exato do legado.
fn html(
    status: StatusCode,
    content_type: &'static str,
    body: &'static str,
    close: bool,
) -> Response {
    let mut resp = (status, body).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(
        header::CONNECTION,
        HeaderValue::from_static(if close { "close" } else { "keep-alive" }),
    );
    resp
}

pub(crate) fn erro_gravar() -> Response {
    html(
        StatusCode::INTERNAL_SERVER_ERROR,
        "text/html; charset=UTF-8",
        ERRO_GRAVAR,
        false,
    )
}

async fn method_not_allowed() -> Response {
    html(
        StatusCode::METHOD_NOT_ALLOWED,
        "text/html; charset=UTF-8",
        METHOD_NOT_ALLOWED,
        false,
    )
}

async fn route_not_found() -> Response {
    html(
        StatusCode::NOT_FOUND,
        "text/html; charset=UTF-8",
        NOT_FOUND_ROUTE,
        true,
    )
}

fn file_not_found() -> Response {
    html(
        StatusCode::NOT_FOUND,
        "text/html; charset=utf-8",
        NOT_FOUND_FILE,
        true,
    )
}

/// `ExtractFileName` + `ChangeFileExt(nome, '.xlsx')` do Delphi: tira o
/// diretório (`/` ou `\`) e troca a última extensão (ou acrescenta, se não
/// houver ponto).
pub(crate) fn output_name(uploaded: &str) -> Option<String> {
    let base = uploaded.rsplit(['/', '\\']).next().unwrap_or(uploaded);
    if base.is_empty() || base.chars().any(|c| c.is_control() || c == '"') {
        return None;
    }
    let stem = match base.rfind('.') {
        Some(i) => &base[..i],
        None => base,
    };
    Some(format!("{stem}.xlsx"))
}

/// O legado escreve o corpo em Latin-1; caractere fora dele vira `?`.
pub(crate) fn latin1(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

async fn post_convert(
    State(st): State<CompatState>,
    ClientIp(ip): ClientIp,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let Ok(mut multipart) = multipart else {
        return erro_gravar();
    };

    // Primeira parte com nome de arquivo, como o legado.
    let mut upload: Option<(String, Bytes)> = None;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let Some(name) = field.file_name().map(str::to_owned) else {
                    continue;
                };
                match field.bytes().await {
                    Ok(bytes) => {
                        upload = Some((name, bytes));
                        break;
                    }
                    Err(_) => return erro_gravar(),
                }
            }
            Ok(None) => break,
            Err(_) => return erro_gravar(),
        }
    }
    let Some((uploaded, bytes)) = upload else {
        return erro_gravar();
    };
    let Some(out_name) = output_name(&uploaded) else {
        return erro_gravar();
    };

    let started = Instant::now();
    let size = bytes.len();
    let Ok(permit) = tokio::time::timeout(st.cfg.timeout, st.permits.clone().acquire_owned()).await
    else {
        tracing::warn!(bytes = size, "conversão aguardou vaga além do tempo limite");
        return erro_gravar();
    };
    let Ok(permit) = permit else {
        return erro_gravar();
    };
    let limits = st.cfg.limits.with_timeout(st.cfg.timeout);
    let job = crate::conversion_worker::run(bytes, limits, &st.cfg, Arc::new(permit));
    let outcome = tokio::time::timeout(st.cfg.timeout, job).await;
    let elapsed_ms = started.elapsed().as_millis();

    match outcome {
        Ok(Ok(xlsx)) => {
            tracing::info!(
                bytes = size,
                saida = xlsx.len(),
                elapsed_ms,
                "conversão concluída"
            );
            let xlsx = Bytes::from(xlsx);
            let returned = latin1(&out_name);
            // O nome devolvido é o Latin-1 do nome enviado (caractere fora dele
            // vira '?'). Esse nome exato vira um alias do resultado, para o GET
            // de quem usa o nome recebido funcionar, sem tomar o lugar de outro
            // resultado que já tenha esse nome.
            let alias: String = returned.iter().map(|&b| char::from(b)).collect();
            let key = store_key(&st.cfg.compat, ip, &out_name);
            if !st.store.put(key.clone(), xlsx.clone()) {
                tracing::warn!(
                    saida = xlsx.len(),
                    "resultado acima do teto de armazenamento"
                );
                return erro_gravar();
            }
            if alias != out_name {
                st.store
                    .put_alias(store_key(&st.cfg.compat, ip, &alias), &key);
            }
            let mut resp = (StatusCode::OK, returned).into_response();
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(XLSX_MIME));
            h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
            resp
        }
        Ok(Err(e)) => {
            tracing::info!(bytes = size, elapsed_ms, "conversão recusada: {e}");
            erro_gravar()
        }
        Err(_) => {
            tracing::warn!(bytes = size, elapsed_ms, "conversão excedeu o tempo limite");
            erro_gravar()
        }
    }
}

/// Valor de `file` na query, com percent-decoding; aceita UTF-8 ou Latin-1.
pub(crate) fn query_file(uri: &Uri) -> Option<Vec<String>> {
    let query = uri.query()?;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k != "file" {
            continue;
        }
        let raw = percent_decode(v.as_bytes());
        let mut candidates = Vec::new();
        match String::from_utf8(raw.clone()) {
            Ok(s) => candidates.push(s),
            Err(_) => candidates.push(raw.iter().map(|&b| char::from(b)).collect()),
        }
        // '+' como espaço, para clientes que codificam a query como formulário.
        if v.contains('+') {
            let spaced = percent_decode(v.replace('+', " ").as_bytes());
            if let Ok(s) = String::from_utf8(spaced) {
                candidates.push(s);
            }
        }
        return Some(candidates);
    }
    None
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            let hex = std::str::from_utf8(&input[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

async fn get_convert(State(st): State<CompatState>, ClientIp(ip): ClientIp, uri: Uri) -> Response {
    let consume = st.cfg.compat.one_time;
    download(st, ip, uri, consume)
}

async fn head_convert(State(st): State<CompatState>, ClientIp(ip): ClientIp, uri: Uri) -> Response {
    // Axum remove o corpo da resposta HEAD; consultar não consome o arquivo.
    download(st, ip, uri, false)
}

fn download(st: CompatState, ip: Option<IpAddr>, uri: Uri, consume: bool) -> Response {
    let Some(candidates) = query_file(&uri) else {
        return file_not_found();
    };
    if st.misses.bloqueado(ip) {
        tracing::warn!("GET recusado: IP acima do limite de nomes inexistentes");
        return file_not_found();
    }
    for name in candidates {
        let key = store_key(&st.cfg.compat, ip, &name);
        if let Some(xlsx) = st.store.take(&key, consume) {
            let mut disposition = b"attachment; filename=\"".to_vec();
            disposition.extend_from_slice(&latin1(&name));
            disposition.push(b'"');
            let Ok(disposition) = HeaderValue::from_bytes(&disposition) else {
                return file_not_found();
            };
            let mut resp = (StatusCode::OK, xlsx).into_response();
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(XLSX_MIME));
            h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
            h.insert(header::CONTENT_DISPOSITION, disposition);
            return resp;
        }
    }
    st.misses.registrar(ip);
    file_not_found()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nome_de_saida_como_changefileext() {
        assert_eq!(output_name("tmp33E7.xls").as_deref(), Some("tmp33E7.xlsx"));
        assert_eq!(output_name("semext").as_deref(), Some("semext.xlsx"));
        assert_eq!(
            output_name("captura.dois.XLS").as_deref(),
            Some("captura.dois.xlsx")
        );
        assert_eq!(output_name("C:\\pasta\\a.xls").as_deref(), Some("a.xlsx"));
        assert_eq!(
            output_name("../../etc/passwd").as_deref(),
            Some("passwd.xlsx")
        );
        assert_eq!(output_name("").as_deref(), None);
        assert_eq!(output_name("pasta/").as_deref(), None);
        assert_eq!(output_name("a\r\nb.xls").as_deref(), None);
        assert_eq!(output_name("a\"b.xls").as_deref(), None);
    }

    #[test]
    fn latin1_como_o_legado() {
        assert_eq!(latin1("ação.xlsx"), b"a\xe7\xe3o.xlsx".to_vec());
        assert_eq!(latin1("€.xlsx"), b"?.xlsx".to_vec());
    }

    #[test]
    fn query_file_decodifica() {
        let u: Uri = "/convert?file=a%20b.xlsx".parse().unwrap();
        assert_eq!(query_file(&u).unwrap()[0], "a b.xlsx");
        let u: Uri = "/convert?file=a%C3%A7.xlsx".parse().unwrap();
        assert_eq!(query_file(&u).unwrap()[0], "aç.xlsx");
        let u: Uri = "/convert?file=a%E7.xlsx".parse().unwrap();
        assert_eq!(query_file(&u).unwrap()[0], "aç.xlsx");
        let u: Uri = "/convert?outra=1".parse().unwrap();
        assert!(query_file(&u).is_none());
        let u: Uri = "/convert?file=a%2".parse().unwrap();
        assert_eq!(query_file(&u).unwrap()[0], "a%2");
    }

    #[test]
    fn store_expira_e_respeita_tetos() {
        let store = Store::new(CompatStore {
            ttl: Duration::from_secs(3600),
            max_bytes: 10,
            max_entries: 2,
            ..CompatStore::default()
        });
        store.put("a".into(), Bytes::from_static(b"1234"));
        store.put("b".into(), Bytes::from_static(b"1234"));
        store.put("c".into(), Bytes::from_static(b"1234"));
        assert!(
            store.take("a", false).is_none(),
            "o mais antigo sai pelo teto de entradas"
        );
        assert!(store.take("b", false).is_some() && store.take("c", false).is_some());
        assert!(
            !store.put("grande".into(), Bytes::from(vec![0u8; 11])),
            "acima do teto de bytes não é guardado e o chamador fica sabendo"
        );
        assert!(store.take("grande", false).is_none());

        let curto = Store::new(CompatStore {
            ttl: Duration::from_millis(1),
            max_bytes: 100,
            max_entries: 10,
            ..CompatStore::default()
        });
        curto.put("x".into(), Bytes::from_static(b"1"));
        std::thread::sleep(Duration::from_millis(5));
        assert!(curto.take("x", false).is_none(), "expirado");
    }
}
