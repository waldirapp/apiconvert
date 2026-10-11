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

use calamine::{Data, DataRef, Range, Reader, SheetType, Xls, Xlsx};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

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
    limits.check_deadline()?;
    let range = match format {
        InputFormat::Xls => {
            validate_xls_allocations(input, limits)?;
            let mut wb: Xls<_> =
                Xls::new(Cursor::new(input)).map_err(|_| ConvertError::InvalidWorkbook)?;
            first_range(&mut wb)?
        }
        InputFormat::Xlsx => {
            return read_xlsx_bounded(input, limits);
        }
    };
    build_grid(&range, limits)
}

fn check_extent(height: u32, width: u32, limits: Limits) -> Result<(), ConvertError> {
    if height > limits.max_rows {
        return Err(ConvertError::TooLarge("linhas"));
    }
    if width > limits.max_cols {
        return Err(ConvertError::TooLarge("colunas"));
    }
    if u64::from(height) * u64::from(width) > limits.max_cells {
        return Err(ConvertError::TooLarge("células"));
    }
    Ok(())
}

/// Lê cada entrada com teto real, incluindo strings/estilos abertos pelo
/// calamine antes da aba. O tamanho anunciado no ZIP não basta.
fn validate_zip_expansion(input: &[u8], limits: Limits) -> Result<(), ConvertError> {
    let mut zip =
        zip::ZipArchive::new(Cursor::new(input)).map_err(|_| ConvertError::InvalidWorkbook)?;
    if zip.len() > 4096 {
        return Err(ConvertError::TooLarge("entradas ZIP"));
    }
    let mut total = 0u64;
    let mut buffer = [0u8; 8192];
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|_| ConvertError::InvalidWorkbook)?;
        if entry.size() > limits.max_expanded_bytes.saturating_sub(total) {
            return Err(ConvertError::TooLarge("expansão ZIP"));
        }
        loop {
            limits.check_deadline()?;
            let n = entry
                .read(&mut buffer)
                .map_err(|_| ConvertError::InvalidWorkbook)?;
            if n == 0 {
                break;
            }
            total = total
                .checked_add(n as u64)
                .ok_or(ConvertError::TooLarge("expansão ZIP"))?;
            if total > limits.max_expanded_bytes {
                return Err(ConvertError::TooLarge("expansão ZIP"));
            }
        }
    }
    Ok(())
}

fn read_xlsx_bounded(input: &[u8], limits: Limits) -> Result<Grid, ConvertError> {
    validate_zip_expansion(input, limits)?;
    let mut wb: Xlsx<_> =
        Xlsx::new(Cursor::new(input)).map_err(|_| ConvertError::InvalidWorkbook)?;
    let name = wb
        .sheet_names()
        .first()
        .ok_or(ConvertError::NoSheet)?
        .clone();
    let mut reader = wb
        .worksheet_cells_reader(&name)
        .map_err(|_| ConvertError::InvalidWorkbook)?;
    let mut cells = Vec::new();
    let (mut height, mut width) = (0u32, 0u32);
    let mut text_bytes = 0u64;
    while let Some(cell) = {
        limits.check_deadline()?;
        reader
            .next_cell()
            .map_err(|_| ConvertError::InvalidWorkbook)?
    } {
        let value = cell.get_value();
        if matches!(value, DataRef::Empty) {
            continue;
        }
        let (r, c) = cell.get_position();
        height = height.max(r.checked_add(1).ok_or(ConvertError::TooLarge("linhas"))?);
        width = width.max(c.checked_add(1).ok_or(ConvertError::TooLarge("colunas"))?);
        check_extent(height, width, limits)?;
        if cells.len() as u64 >= limits.max_cells {
            return Err(ConvertError::TooLarge("células"));
        }
        let bytes = match value {
            DataRef::String(s) | DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => s.len(),
            DataRef::SharedString(s) => s.len(),
            _ => 0,
        };
        text_bytes = text_bytes
            .checked_add(bytes as u64)
            .ok_or(ConvertError::TooLarge("texto"))?;
        if text_bytes > limits.max_expanded_bytes {
            return Err(ConvertError::TooLarge("texto"));
        }
        let data: Data = value.clone().into();
        cells.push(((r, c), parse_cell(&data)?));
    }
    let mut rows: Vec<Vec<Value>> = (0..height)
        .map(|_| vec![Value::Empty; width as usize])
        .collect();
    for ((r, c), value) in cells {
        rows[r as usize][c as usize] = value;
    }
    Ok(Grid {
        height,
        width,
        rows,
    })
}

/// O XLS abre todas as abas e reserva a grade de DIMENSIONS antes de devolver
/// a primeira. A cota da conversão vale para dados da primeira planilha;
/// DIMENSIONS e todas as abas usam o orçamento separado da abertura XLS.
fn validate_xls_allocations(input: &[u8], limits: Limits) -> Result<(), ConvertError> {
    let mut comp =
        cfb::CompoundFile::open(Cursor::new(input)).map_err(|_| ConvertError::InvalidWorkbook)?;
    let name = ["/Workbook", "/Book"]
        .into_iter()
        .find(|n| comp.is_stream(n))
        .ok_or(ConvertError::InvalidWorkbook)?;
    let mut stream = comp
        .open_stream(name)
        .map_err(|_| ConvertError::InvalidWorkbook)?;
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        limits.check_deadline()?;
        let n = stream
            .read(&mut buffer)
            .map_err(|_| ConvertError::InvalidWorkbook)?;
        if n == 0 {
            break;
        }
        if data.len() as u64 + n as u64 > limits.max_expanded_bytes {
            return Err(ConvertError::TooLarge("fluxo XLS"));
        }
        data.extend_from_slice(&buffer[..n]);
    }
    if !biff_bofs_ok(&data) {
        return Err(ConvertError::InvalidWorkbook);
    }
    let mut pos = 0;
    let mut offsets = Vec::new();
    while let Some((opcode, len)) = record_header(&data, pos) {
        let body = data
            .get(pos + 4..pos + 4 + len)
            .ok_or(ConvertError::InvalidWorkbook)?;
        if opcode == BOUNDSHEET {
            let offset = body.get(..4).ok_or(ConvertError::InvalidWorkbook)?;
            let is_worksheet =
                *body.get(5).ok_or(ConvertError::InvalidWorkbook)? == SHEET_TYPE_WORKSHEET;
            offsets.push((
                u32::from_le_bytes(offset.try_into().unwrap()) as usize,
                is_worksheet,
            ));
        }
        if opcode == EOF {
            break;
        }
        pos += 4 + len;
    }
    let mut total_cells = 0u64;
    let mut first_worksheet_seen = false;
    let parser_limits = Limits {
        max_cells: limits.max_xls_parse_cells,
        ..limits
    };
    for (offset, is_worksheet) in offsets {
        let converted_sheet = is_worksheet && !first_worksheet_seen;
        first_worksheet_seen |= is_worksheet;
        pos = offset;
        let (mut height, mut width) = (0u32, 0u32);
        let (mut data_height, mut data_width) = (0u32, 0u32);
        let mut records = 0u64;
        while let Some((opcode, len)) = record_header(&data, pos) {
            limits.check_deadline()?;
            let body = data
                .get(pos + 4..pos + 4 + len)
                .ok_or(ConvertError::InvalidWorkbook)?;
            if opcode == 0x0200 {
                let (rf, rl, cf, cl) = match body.len() {
                    14 => (
                        u32::from_le_bytes(body[0..4].try_into().unwrap()),
                        u32::from_le_bytes(body[4..8].try_into().unwrap()),
                        u16::from_le_bytes(body[8..10].try_into().unwrap()) as u32,
                        u16::from_le_bytes(body[10..12].try_into().unwrap()) as u32,
                    ),
                    10 => (
                        u16::from_le_bytes(body[0..2].try_into().unwrap()) as u32,
                        u16::from_le_bytes(body[2..4].try_into().unwrap()) as u32,
                        u16::from_le_bytes(body[4..6].try_into().unwrap()) as u32,
                        u16::from_le_bytes(body[6..8].try_into().unwrap()) as u32,
                    ),
                    _ => return Err(ConvertError::InvalidWorkbook),
                };
                if rl < rf || cl < cf {
                    return Err(ConvertError::InvalidWorkbook);
                }
                height = height.max(rl.max(rf.saturating_add(1)));
                width = width.max(cl.max(cf.saturating_add(1)));
            }
            if matches!(
                opcode,
                0x0203 | 0x0204 | 0x00D6 | 0x0205 | 0x027E | 0x00FD | 0x0006 | 0x00BD
            ) {
                if body.len() < 4 {
                    return Err(ConvertError::InvalidWorkbook);
                }
                let r = u16::from_le_bytes(body[0..2].try_into().unwrap()) as u32;
                let c = if opcode == 0x00BD {
                    u16::from_le_bytes(body[body.len() - 2..].try_into().unwrap()) as u32
                } else {
                    u16::from_le_bytes(body[2..4].try_into().unwrap()) as u32
                };
                height = height.max(r + 1);
                width = width.max(c + 1);
                data_height = data_height.max(r + 1);
                data_width = data_width.max(c + 1);
                if converted_sheet {
                    check_extent(data_height, data_width, limits)?;
                }
                records += 1;
                if records > limits.max_xls_parse_cells {
                    return Err(ConvertError::TooLarge("células XLS"));
                }
            }
            check_extent(height, width, parser_limits).map_err(|error| match error {
                ConvertError::TooLarge("células") => ConvertError::TooLarge("células XLS"),
                other => other,
            })?;
            if opcode == EOF {
                break;
            }
            pos += 4 + len;
        }
        total_cells += u64::from(height) * u64::from(width);
        if total_cells > limits.max_xls_parse_cells {
            return Err(ConvertError::TooLarge("abas XLS"));
        }
    }
    Ok(())
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
#[cfg(test)]
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
    let name = wb
        .sheets_metadata()
        .iter()
        .find(|sheet| sheet.typ == SheetType::WorkSheet)
        .ok_or(ConvertError::NoSheet)?
        .name
        .clone();
    wb.worksheet_range(&name)
        .map_err(|_| ConvertError::InvalidWorkbook)
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
    for (i, (r, c, cell)) in range.used_cells().enumerate() {
        if i % 4096 == 0 {
            limits.check_deadline()?;
        }
        let row = start_row as usize + r;
        let col = start_col as usize + c;
        rows[row][col] = parse_cell(cell)?;
    }
    Ok(Grid {
        height,
        width,
        rows,
    })
}

/// `pandas/io/excel/_xlrd.py::_parse_cell`.
fn parse_cell(cell: &Data) -> Result<Value, ConvertError> {
    Ok(match cell {
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
        Data::DateTimeIso(s) => parse_iso_date(s)?,
        // Duração é um tipo distinto de data/hora; preserva o tratamento
        // existente, sem reinterpretá-la como um horário do dia.
        Data::DurationIso(s) => Value::Str(s.clone()),
    })
}

/// XLSX `t="d"`: data/data-hora vira datetime, hora isolada vira time.
/// O openpyxl lê a fração ISO com resolução de milissegundos.
fn parse_iso_date(s: &str) -> Result<Value, ConvertError> {
    let s = s.strip_suffix('Z').unwrap_or(s);
    if let Some((date, time)) = s.split_once('T') {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|_| ConvertError::InvalidWorkbook)?;
        return Ok(Value::DateTime(date.and_time(parse_iso_time(time)?)));
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Value::DateTime(date.and_hms_opt(0, 0, 0).unwrap()));
    }
    Ok(Value::Time(parse_iso_time(s)?))
}

fn parse_iso_time(s: &str) -> Result<NaiveTime, ConvertError> {
    let time = ["%H:%M:%S%.f", "%H:%M"]
        .into_iter()
        .find_map(|fmt| NaiveTime::parse_from_str(s, fmt).ok())
        .ok_or(ConvertError::InvalidWorkbook)?;
    time.with_nanosecond(time.nanosecond() / 1_000_000 * 1_000_000)
        .ok_or(ConvertError::InvalidWorkbook)
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
