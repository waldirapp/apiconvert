//! Leitura da primeira aba com a semântica do `xlrd` 2.0.1 + pandas 2.2.3.
//!
//! Fontes reproduzidas:
//! - `pandas/io/excel/_xlrd.py::get_sheet_data`: lê `sheet.nrows` linhas a
//!   partir da linha 0 e da coluna 0 (posições absolutas, inclusive linhas e
//!   colunas vazias à esquerda e acima dos dados).
//! - `pandas/io/excel/_xlrd.py::_parse_cell`: número inteiro vira `int`;
//!   data vira `datetime` (ou `time` no dia-época); erro vira NaN; booleano
//!   vira `bool`.
//! - `xlrd/xldate.py::xldate_as_datetime`: conversão do número serial.
//! - `xlrd/book.py::Book.getbof`: o fluxo `Workbook` (ou `Book`) do contêiner
//!   OLE2 precisa começar com um registro BOF; senão o xlrd levanta
//!   `XLRDError: Expected BOF record` e o pandas não converte. O calamine é mais
//!   tolerante, então a checagem é feita antes dele.
//!
//! Para `.xlsx` o pandas usa o openpyxl; a regra de conversão de células é a
//! mesma nos casos que importam aqui (número, texto, data, booleano, erro).

use std::io::{Cursor, Read};

use calamine::{Data, Range, Reader, Xls, Xlsx};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike};

use super::{ConvertError, Limits, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Xls,
    Xlsx,
}

/// Assinatura do Compound File Binary (formato do `.xls` BIFF8).
const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
/// Assinatura de arquivo ZIP (formato do `.xlsx`).
const ZIP_MAGIC: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

/// Detecta o formato pelo conteúdo, como o `pandas.io.excel.inspect_excel_format`.
/// Qualquer outro conteúdo (HTML salvo como `.xls`, CSV etc.) também falha
/// no pandas, então é recusado.
pub fn detect(input: &[u8]) -> Option<InputFormat> {
    if input.starts_with(&CFB_MAGIC) {
        Some(InputFormat::Xls)
    } else if input.starts_with(&ZIP_MAGIC) {
        Some(InputFormat::Xlsx)
    } else {
        None
    }
}

/// Grade da primeira aba em posições absolutas (linha 0 = cabeçalho).
pub(crate) struct Grid {
    pub height: u32,
    pub width: u32,
    /// Linhas em ordem; cada uma com `width` valores.
    pub rows: Vec<Vec<Value>>,
}

impl Grid {
    /// Linhas de dados, sem o cabeçalho (linha 0).
    pub fn data_rows(&self) -> u32 {
        self.height.saturating_sub(1)
    }
}

pub(crate) fn read_first_sheet(
    input: &[u8],
    format: InputFormat,
    limits: Limits,
) -> Result<Grid, ConvertError> {
    let range = match format {
        InputFormat::Xls => {
            if !xlrd_accepts_ole2(input) {
                return Err(ConvertError::InvalidWorkbook);
            }
            let mut wb: Xls<_> =
                Xls::new(Cursor::new(input)).map_err(|_| ConvertError::InvalidWorkbook)?;
            first_range(&mut wb)?
        }
        InputFormat::Xlsx => {
            let mut wb: Xlsx<_> =
                Xlsx::new(Cursor::new(input)).map_err(|_| ConvertError::InvalidWorkbook)?;
            first_range(&mut wb)?
        }
    };
    build_grid(&range, limits)
}

/// Opcodes de BOF aceitos pelo xlrd (`bofcodes`: BIFF8, BIFF4, BIFF3, BIFF2).
const BOF_OPCODES: [u16; 4] = [0x0809, 0x0409, 0x0209, 0x0009];

/// Opcode do registro BOUNDSHEET (BIFF5/7/8) e do EOF.
const BOUNDSHEET: u16 = 0x0085;
const EOF: u16 = 0x000A;
/// Tipo de folha "worksheet" no BOUNDSHEET; o xlrd ignora gráfico e módulo VB.
const SHEET_TYPE_WORKSHEET: u8 = 0;

/// Reprodução das checagens de BOF do xlrd 2.0.1 para `.xls`, que o pandas
/// dispara ao abrir o arquivo (`open_workbook` sem `on_demand`, que carrega
/// todas as abas):
///
/// 1. localizar o fluxo `Workbook` ou `Book` (nessa ordem);
/// 2. `Book.getbof` no início do fluxo: opcode em `bofcodes` e tamanho entre
///    4 e 20 bytes;
/// 3. para cada BOUNDSHEET de planilha (tipo 0), `get_sheet` posiciona no
///    deslocamento indicado e repete o `getbof`.
///
/// Se qualquer passo falha, o xlrd levanta `XLRDError` e o pandas não
/// converte o arquivo; o calamine aceitaria alguns desses arquivos.
fn xlrd_accepts_ole2(input: &[u8]) -> bool {
    let Ok(mut comp) = cfb::CompoundFile::open(Cursor::new(input)) else {
        return false;
    };
    for name in ["/Workbook", "/Book"] {
        if !comp.is_stream(name) {
            continue;
        }
        let Ok(mut stream) = comp.open_stream(name) else {
            return false;
        };
        let mut data = Vec::new();
        if stream.read_to_end(&mut data).is_err() {
            return false;
        }
        return biff_bofs_ok(&data);
    }
    false
}

/// Confere o BOF do início do fluxo e o de cada planilha do BOUNDSHEET.
fn biff_bofs_ok(stream: &[u8]) -> bool {
    if !bof_at(stream, 0) {
        return false;
    }
    let mut pos = 0usize;
    // Percorre os registros da seção global até o EOF.
    while let Some((opcode, len)) = record_header(stream, pos) {
        let body = pos + 4;
        let Some(end) = body.checked_add(len).filter(|e| *e <= stream.len()) else {
            return false;
        };
        if opcode == BOUNDSHEET && len >= 6 {
            let offset = u32::from_le_bytes([
                stream[body],
                stream[body + 1],
                stream[body + 2],
                stream[body + 3],
            ]) as usize;
            let sheet_type = stream[body + 5];
            if sheet_type == SHEET_TYPE_WORKSHEET && !bof_at(stream, offset) {
                return false;
            }
        }
        if opcode == EOF {
            break;
        }
        pos = end;
    }
    true
}

fn record_header(stream: &[u8], pos: usize) -> Option<(u16, usize)> {
    let h = stream.get(pos..pos.checked_add(4)?)?;
    Some((
        u16::from_le_bytes([h[0], h[1]]),
        usize::from(u16::from_le_bytes([h[2], h[3]])),
    ))
}

/// `Book.getbof`: opcode em `bofcodes`, tamanho entre 4 e 20 e corpo completo.
fn bof_at(stream: &[u8], pos: usize) -> bool {
    let Some((opcode, len)) = record_header(stream, pos) else {
        return false;
    };
    BOF_OPCODES.contains(&opcode) && (4..=20).contains(&len) && pos + 4 + len <= stream.len()
}

fn first_range<R, W>(wb: &mut W) -> Result<Range<Data>, ConvertError>
where
    R: std::io::Read + std::io::Seek,
    W: Reader<R>,
{
    match wb.worksheet_range_at(0) {
        Some(Ok(range)) => Ok(range),
        Some(Err(_)) => Err(ConvertError::InvalidWorkbook),
        None => Err(ConvertError::NoSheet),
    }
}

fn build_grid(range: &Range<Data>, limits: Limits) -> Result<Grid, ConvertError> {
    let Some((start_row, start_col)) = range.start() else {
        return Ok(Grid {
            height: 0,
            width: 0,
            rows: Vec::new(),
        });
    };
    let (h, w) = range.get_size();
    let height = start_row
        .checked_add(u32::try_from(h).map_err(|_| ConvertError::TooLarge("linhas"))?)
        .ok_or(ConvertError::TooLarge("linhas"))?;
    let width = start_col
        .checked_add(u32::try_from(w).map_err(|_| ConvertError::TooLarge("colunas"))?)
        .ok_or(ConvertError::TooLarge("colunas"))?;
    if height > limits.max_rows {
        return Err(ConvertError::TooLarge("linhas"));
    }
    if width > limits.max_cols {
        return Err(ConvertError::TooLarge("colunas"));
    }
    if u64::from(height) * u64::from(width) > limits.max_cells {
        return Err(ConvertError::TooLarge("células"));
    }

    let mut rows: Vec<Vec<Value>> = (0..height)
        .map(|_| vec![Value::Empty; width as usize])
        .collect();
    for (r, c, cell) in range.used_cells() {
        let row = start_row as usize + r;
        let col = start_col as usize + c;
        rows[row][col] = parse_cell(cell);
    }
    Ok(Grid {
        height,
        width,
        rows,
    })
}

/// `pandas/io/excel/_xlrd.py::_parse_cell`.
fn parse_cell(cell: &Data) -> Value {
    match cell {
        Data::Empty => Value::Empty,
        Data::String(s) => Value::Str(s.clone()),
        Data::Float(f) => number(*f),
        Data::Int(i) => Value::Int(i128::from(*i)),
        Data::Bool(b) => Value::Bool(*b),
        Data::Error(_) => Value::Float(f64::NAN),
        Data::DateTime(dt) => {
            let serial = dt.as_f64();
            let is_1904 = workbook_is_1904(dt, serial);
            date_cell(serial, is_1904)
        }
        // Datas e durações ISO só aparecem em .xlsx com o tipo "d", raro;
        // o openpyxl as entrega como texto quando não reconhece o formato.
        Data::DateTimeIso(s) | Data::DurationIso(s) => Value::Str(s.clone()),
    }
}

/// Regra GH5394 do pandas: número finito com `int(x) == x` vira `int`.
fn number(f: f64) -> Value {
    if f.is_finite() && f.trunc() == f {
        if f.abs() < 1.7e38 {
            Value::Int(f as i128)
        } else {
            Value::BigInt(f)
        }
    } else {
        Value::Float(f)
    }
}

/// O calamine não expõe o modo de data do arquivo (1900 ou 1904), mas aplica
/// esse modo no `as_datetime`. A diferença entre os dois é de 1462 dias, então
/// comparar com a conversão pelo sistema 1900 revela o modo.
fn workbook_is_1904(dt: &calamine::ExcelDateTime, serial: f64) -> bool {
    let Some(calamine_dt) = dt.as_datetime() else {
        return false;
    };
    let Some(as_1900) = epoch_1900_minus_1().checked_add_signed(Duration::days(serial as i64))
    else {
        return false;
    };
    (calamine_dt.date() - as_1900.date()).num_days().abs() > 1000
}

fn epoch_1900_minus_1() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(1899, 12, 30)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("data constante válida")
}

/// Célula com formato de data: `xldate_as_datetime` + ajuste de hora do pandas.
fn date_cell(serial: f64, is_1904: bool) -> Value {
    let Some(dt) = xldate_as_datetime(serial, is_1904) else {
        // OverflowError no Python: o pandas mantém o float original, sem a
        // conversão para int.
        return Value::Float(serial);
    };
    let (y, m, d) = (dt.year(), dt.month(), dt.day());
    let epoch_day = if is_1904 {
        (y, m, d) == (1904, 1, 1)
    } else {
        (y, m, d) == (1899, 12, 31)
    };
    if epoch_day {
        Value::Time(dt.time())
    } else {
        Value::DateTime(dt)
    }
}

/// Reprodução de `xlrd.xldate.xldate_as_datetime` (xlrd 2.0.1):
///
/// ```python
/// if datemode: epoch = datetime(1904, 1, 1)
/// elif xldate < 60: epoch = datetime(1899, 12, 31)
/// else: epoch = datetime(1899, 12, 30)
/// days = int(xldate)
/// fraction = xldate - days
/// seconds = int(round(fraction * 86400000.0))
/// seconds, milliseconds = divmod(seconds, 1000)
/// return epoch + timedelta(days, seconds, 0, milliseconds)
/// ```
///
/// Devolve `None` onde o Python levantaria `OverflowError` (fora de
/// `datetime.min`..`datetime.max`, anos 1 a 9999).
pub(crate) fn xldate_as_datetime(xldate: f64, is_1904: bool) -> Option<NaiveDateTime> {
    if !xldate.is_finite() {
        return None;
    }
    let epoch = if is_1904 {
        NaiveDate::from_ymd_opt(1904, 1, 1)?
    } else if xldate < 60.0 {
        NaiveDate::from_ymd_opt(1899, 12, 31)?
    } else {
        NaiveDate::from_ymd_opt(1899, 12, 30)?
    }
    .and_hms_opt(0, 0, 0)?;
    // int() do Python trunca em direção a zero.
    let days = xldate.trunc();
    if days.abs() > 4_000_000.0 {
        return None;
    }
    let fraction = xldate - days;
    // round() do Python arredonda empates para o par.
    let total_ms = (fraction * 86_400_000.0).round_ties_even() as i64;
    // divmod do Python usa divisão com piso (igual a euclidiana para divisor positivo).
    let seconds = total_ms.div_euclid(1000);
    let millis = total_ms.rem_euclid(1000);
    let delta =
        Duration::days(days as i64) + Duration::seconds(seconds) + Duration::milliseconds(millis);
    let dt = epoch.checked_add_signed(delta)?;
    if dt.year() < 1 || dt.year() > 9999 {
        return None;
    }
    // Garante a resolução de milissegundos do xlrd (sem resto em nanossegundos).
    debug_assert_eq!(dt.nanosecond() % 1_000_000, 0);
    Some(dt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32, ms: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_milli_opt(h, mi, s, ms)
            .unwrap()
    }

    #[test]
    fn detecta_formato_pela_assinatura() {
        assert_eq!(detect(&CFB_MAGIC), Some(InputFormat::Xls));
        assert_eq!(detect(b"PK\x03\x04resto"), Some(InputFormat::Xlsx));
        assert_eq!(detect(b"<html><table>"), None);
        assert_eq!(detect(b""), None);
    }

    #[test]
    fn ole2_sem_bof_no_workbook_e_recusado() {
        fn ole2(stream: &[u8]) -> Vec<u8> {
            let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
            {
                use std::io::Write;
                let mut s = comp.create_stream("/Workbook").unwrap();
                s.write_all(stream).unwrap();
            }
            comp.into_inner().into_inner()
        }
        // BOF BIFF8 (0x0809) com tamanho 16: aceito.
        let mut ok = vec![0x09, 0x08, 0x10, 0x00];
        ok.extend_from_slice(&[0u8; 16]);
        assert!(xlrd_accepts_ole2(&ole2(&ok)));
        // Primeiro registro que não é BOF: recusado, como no xlrd.
        let mut ruim = vec![0x01, 0x02, 0x10, 0x00];
        ruim.extend_from_slice(&[0u8; 16]);
        assert!(!xlrd_accepts_ole2(&ole2(&ruim)));
        // BOF com tamanho fora de 4..=20: recusado.
        assert!(!xlrd_accepts_ole2(&ole2(&[
            0x09, 0x08, 0x40, 0x00, 0, 0, 0, 0
        ])));
        // BOUNDSHEET de planilha apontando para fora de um BOF: recusado
        // (caso real de arquivo que o pandas não converte).
        let mut globals = vec![0x09, 0x08, 0x10, 0x00];
        globals.extend_from_slice(&[0u8; 16]);
        let sheet_off = (globals.len() + 4 + 8 + 4) as u32; // após BOUNDSHEET e EOF
        let mut bs = vec![0x85, 0x00, 0x08, 0x00];
        bs.extend_from_slice(&sheet_off.to_le_bytes());
        bs.extend_from_slice(&[0x00, 0x00, 0x01, 0x00]); // visível, planilha, nome de 1 char vazio
        globals.extend_from_slice(&bs);
        globals.extend_from_slice(&[0x0A, 0x00, 0x00, 0x00]); // EOF
        let mut com_bof = globals.clone();
        com_bof.extend_from_slice(&[0x09, 0x08, 0x10, 0x00]);
        com_bof.extend_from_slice(&[0u8; 16]);
        assert!(xlrd_accepts_ole2(&ole2(&com_bof)));
        let mut sem_bof = globals;
        sem_bof.extend_from_slice(&[0x01, 0x02, 0x06, 0x00, 0, 0, 0, 0, 0, 0]);
        assert!(!xlrd_accepts_ole2(&ole2(&sem_bof)));
        // OLE2 sem fluxo Workbook/Book: recusado.
        let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        comp.create_stream("/Outro").unwrap();
        assert!(!xlrd_accepts_ole2(&comp.into_inner().into_inner()));
    }

    #[test]
    fn numero_integral_vira_int_e_fracionario_fica_float() {
        assert_eq!(number(123.0), Value::Int(123));
        assert_eq!(number(-4.0), Value::Int(-4));
        assert_eq!(number(1.5), Value::Float(1.5));
        assert!(matches!(number(f64::NAN), Value::Float(f) if f.is_nan()));
        assert_eq!(number(f64::INFINITY), Value::Float(f64::INFINITY));
        assert_eq!(number(1e300), Value::BigInt(1e300));
    }

    #[test]
    fn xldate_sistema_1900() {
        // 45000 = 2023-03-15
        assert_eq!(
            xldate_as_datetime(45000.0, false),
            Some(dt(2023, 3, 15, 0, 0, 0, 0))
        );
        // meio-dia
        assert_eq!(
            xldate_as_datetime(45000.5, false),
            Some(dt(2023, 3, 15, 12, 0, 0, 0))
        );
        // abaixo de 60 usa a época 1899-12-31 (bug do ano bissexto de 1900)
        assert_eq!(
            xldate_as_datetime(1.0, false),
            Some(dt(1900, 1, 1, 0, 0, 0, 0))
        );
        assert_eq!(
            xldate_as_datetime(59.0, false),
            Some(dt(1900, 2, 28, 0, 0, 0, 0))
        );
        assert_eq!(
            xldate_as_datetime(61.0, false),
            Some(dt(1900, 3, 1, 0, 0, 0, 0))
        );
    }

    #[test]
    fn xldate_sistema_1904() {
        assert_eq!(
            xldate_as_datetime(0.0, true),
            Some(dt(1904, 1, 1, 0, 0, 0, 0))
        );
        assert_eq!(
            xldate_as_datetime(1.25, true),
            Some(dt(1904, 1, 2, 6, 0, 0, 0))
        );
    }

    #[test]
    fn xldate_arredonda_milissegundos_como_python() {
        // 0.5 ms de fração: round() do Python arredonda o empate para o par.
        let half_ms = 0.5 / 86_400_000.0;
        let r = xldate_as_datetime(45000.0 + half_ms, false).unwrap();
        assert_eq!(r.and_utc().timestamp_subsec_millis(), 0);
    }

    #[test]
    fn xldate_fora_do_intervalo_do_python() {
        assert_eq!(xldate_as_datetime(3_000_000.0, false), None);
        assert_eq!(xldate_as_datetime(f64::NAN, false), None);
    }

    #[test]
    fn data_no_dia_epoca_vira_hora() {
        // 0.75 no sistema 1900: 1899-12-31 18:00 → time
        assert_eq!(
            date_cell(0.75, false),
            Value::Time(NaiveTime::from_hms_opt(18, 0, 0).unwrap())
        );
        // 0.75 no sistema 1904: 1904-01-01 18:00 → time
        assert_eq!(
            date_cell(0.75, true),
            Value::Time(NaiveTime::from_hms_opt(18, 0, 0).unwrap())
        );
        // fora do intervalo: float original, sem conversão para int
        assert_eq!(date_cell(3_000_000.0, false), Value::Float(3_000_000.0));
    }
}
