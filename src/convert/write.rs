//! Escrita do `.xlsx` como `DataFrame.to_excel(index=False)` com o motor
//! openpyxl: 1ª linha com os nomes das colunas, dados a partir da 2ª.
//!
//! Fontes reproduzidas:
//! - `pandas/io/formats/excel.py::ExcelFormatter._format_value`: nulo vira
//!   `na_rep` (`''`, célula vazia); `+inf`/`-inf` viram os textos `inf` e
//!   `-inf`.
//! - `pandas/io/excel/_base.py::ExcelWriter._value_with_fmt`: `datetime` com
//!   o formato `YYYY-MM-DD HH:MM:SS`; qualquer outro objeto (inclusive
//!   `datetime.time`) vira `str(val)`.
//! - openpyxl 3.1.5 `compat/strings.py::safe_string`: todo número (inclusive o
//!   serial de data) é gravado com `"%.16g"`, ou seja, arredondado para 16
//!   dígitos significativos.
//! - openpyxl 3.1.5 `Cell`: texto começando com `=` (mais de 1 caractere) é
//!   gravado como fórmula; caractere de controle ilegal levanta
//!   `IllegalCharacterError`; `utils/datetime.py::to_excel` para o número
//!   serial da data.
//! - `pandas/io/formats/excel.py::ExcelFormatter.header_style`: cabeçalho em
//!   negrito, borda fina nos quatro lados, centralizado na horizontal e
//!   alinhado ao topo.
//! - Aba única chamada `Sheet1` (padrão do `to_excel`).

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use rust_xlsxwriter::{Format, FormatAlign, FormatBorder, Formula, Workbook, Worksheet};

use super::header::Label;
use super::infer::{Column, ObjValue};
use super::{ConvertError, Value};

const SHEET_NAME: &str = "Sheet1";
const DATETIME_FORMAT: &str = "YYYY-MM-DD HH:MM:SS";
const MAX_STRING_LEN: usize = 32_767;

pub(crate) fn write_xlsx(
    labels: &[Label],
    columns: &[Column],
    data_rows: u32,
) -> Result<Vec<u8>, ConvertError> {
    let mut workbook = Workbook::new();
    let datetime_fmt = Format::new().set_num_format(DATETIME_FORMAT);
    let header_fmt = Format::new()
        .set_bold()
        .set_border(FormatBorder::Thin)
        .set_align(FormatAlign::Center)
        .set_align(FormatAlign::Top);
    let header_datetime_fmt = header_fmt.clone().set_num_format(DATETIME_FORMAT);
    let sheet = workbook.add_worksheet();
    sheet
        .set_name(SHEET_NAME)
        .map_err(|_| ConvertError::Write)?;

    for (c, label) in labels.iter().enumerate() {
        let col = u16::try_from(c).map_err(|_| ConvertError::TooLarge("colunas"))?;
        let cell = label_cell(label);
        let fmt = if matches!(cell, OutCell::DateTime(_)) {
            &header_datetime_fmt
        } else {
            &header_fmt
        };
        write_cell(sheet, 0, col, cell, fmt, Some(fmt))?;
    }

    for (c, column) in columns.iter().enumerate() {
        let col = u16::try_from(c).map_err(|_| ConvertError::TooLarge("colunas"))?;
        for r in 0..data_rows {
            let cell = cell_at(column, r as usize);
            write_cell(sheet, r + 1, col, cell, &datetime_fmt, None)?;
        }
    }

    workbook.save_to_buffer().map_err(|_| ConvertError::Write)
}

/// Rótulo de coluna gravado no cabeçalho (`_value_with_fmt` sem
/// `_format_value`: float não finito vira célula vazia pelo `safe_string`).
pub(crate) fn label_cell(label: &Label) -> OutCell {
    match label {
        Label::Str(s) => object_cell(&Value::Str(s.clone())),
        Label::Int(i) => OutCell::Number(*i as f64),
        Label::Float(f) if f.is_finite() => OutCell::Number(*f),
        Label::Float(_) => OutCell::Blank,
        Label::Bool(b) => OutCell::Bool(*b),
        Label::DateTime(dt) => OutCell::DateTime(openpyxl_to_excel(dt)),
        Label::Time(t) => OutCell::Text(python_time_str(t)),
    }
}

/// Célula de saída já no tipo que o openpyxl gravaria.
#[derive(Debug, PartialEq)]
pub(crate) enum OutCell {
    Blank,
    Number(f64),
    Text(String),
    Bool(bool),
    DateTime(f64),
    Formula(String),
}

pub(crate) fn cell_at(column: &Column, r: usize) -> OutCell {
    match column {
        Column::Float(v) => float_cell(v[r]),
        Column::Int(v) => OutCell::Number(v[r] as f64),
        Column::Bool(v) => OutCell::Bool(v[r]),
        Column::Object(v) => match &v[r] {
            ObjValue::Nan => OutCell::Blank,
            ObjValue::Value(val) => object_cell(val),
        },
    }
}

fn float_cell(f: f64) -> OutCell {
    if f.is_nan() {
        OutCell::Blank
    } else if f == f64::INFINITY {
        OutCell::Text("inf".to_string())
    } else if f == f64::NEG_INFINITY {
        OutCell::Text("-inf".to_string())
    } else {
        OutCell::Number(f)
    }
}

fn object_cell(val: &Value) -> OutCell {
    match val {
        // '' só sobra na coluna `object` do caminho de conflito uint64; o
        // openpyxl grava a célula sem valor.
        Value::Empty => OutCell::Blank,
        Value::Float(f) | Value::BigInt(f) => float_cell(*f),
        Value::Int(i) => OutCell::Number(*i as f64),
        Value::Bool(b) => OutCell::Bool(*b),
        Value::DateTime(dt) => OutCell::DateTime(openpyxl_to_excel(dt)),
        Value::Time(t) => OutCell::Text(python_time_str(t)),
        Value::Str(s) => {
            if s.chars().count() > 1 && s.starts_with('=') {
                OutCell::Formula(s.clone())
            } else if s.is_empty() {
                OutCell::Blank
            } else {
                OutCell::Text(s.clone())
            }
        }
    }
}

fn write_cell(
    sheet: &mut Worksheet,
    row: u32,
    col: u16,
    cell: OutCell,
    datetime_fmt: &Format,
    style: Option<&Format>,
) -> Result<(), ConvertError> {
    let result = match (cell, style) {
        (OutCell::Blank, None) => return Ok(()),
        (OutCell::Blank, Some(f)) => sheet.write_blank(row, col, f).map(|_| ()),
        (OutCell::Number(n), None) => sheet.write_number(row, col, openpyxl_number(n)).map(|_| ()),
        (OutCell::Number(n), Some(f)) => sheet
            .write_number_with_format(row, col, openpyxl_number(n), f)
            .map(|_| ()),
        (OutCell::Bool(b), None) => sheet.write_boolean(row, col, b).map(|_| ()),
        (OutCell::Bool(b), Some(f)) => sheet.write_boolean_with_format(row, col, b, f).map(|_| ()),
        (OutCell::DateTime(serial), _) => sheet
            .write_number_with_format(row, col, openpyxl_number(serial), datetime_fmt)
            .map(|_| ()),
        (OutCell::Text(s), style) => {
            check_string(&s, row, col)?;
            match style {
                None => sheet.write_string(row, col, s).map(|_| ()),
                Some(f) => sheet.write_string_with_format(row, col, s, f).map(|_| ()),
            }
        }
        (OutCell::Formula(s), style) => {
            check_string(&s, row, col)?;
            match style {
                None => sheet.write_formula(row, col, Formula::new(s)).map(|_| ()),
                Some(f) => sheet
                    .write_formula_with_format(row, col, Formula::new(s), f)
                    .map(|_| ()),
            }
        }
    };
    result.map_err(|_| ConvertError::Write)
}

/// `safe_string` do openpyxl grava números com `"%.16g"`: o valor lido de
/// volta é o float mais próximo de 16 dígitos significativos.
pub(crate) fn openpyxl_number(f: f64) -> f64 {
    if !f.is_finite() {
        return f;
    }
    format!("{f:.15e}").parse().unwrap_or(f)
}

/// `openpyxl.cell.cell.ILLEGAL_CHARACTERS_RE` = `[\000-\010]|[\013-\014]|[\016-\037]`.
fn check_string(s: &str, row: u32, col: u16) -> Result<(), ConvertError> {
    let illegal = s.chars().any(|ch| {
        let c = ch as u32;
        c <= 0x08 || (0x0B..=0x0C).contains(&c) || (0x0E..=0x1F).contains(&c)
    });
    if illegal {
        return Err(ConvertError::IllegalCharacter { row: row + 1, col });
    }
    if s.chars().count() > MAX_STRING_LEN {
        return Err(ConvertError::StringTooLong { row: row + 1, col });
    }
    Ok(())
}

/// `openpyxl.utils.datetime.to_excel` (época Windows 1899-12-30):
///
/// ```python
/// days = (dt - epoch).days
/// if 0 < days <= 60 and epoch == WINDOWS_EPOCH:
///     days -= 1
/// return days + time_to_days(dt)
/// ```
pub(crate) fn openpyxl_to_excel(dt: &NaiveDateTime) -> f64 {
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30).expect("data constante válida");
    let mut days = (dt.date() - epoch).num_days();
    if 0 < days && days <= 60 {
        days -= 1;
    }
    days as f64 + time_to_days(&dt.time())
}

/// `openpyxl.utils.datetime.time_to_days`.
fn time_to_days(t: &NaiveTime) -> f64 {
    let micros = f64::from(t.nanosecond() / 1_000);
    (f64::from(t.hour()) * 3600.0
        + f64::from(t.minute()) * 60.0
        + f64::from(t.second())
        + micros / 1_000_000.0)
        / 86_400.0
}

/// `str(datetime.time)`: `HH:MM:SS`, ou `HH:MM:SS.ffffff` com microssegundos.
pub(crate) fn python_time_str(t: &NaiveTime) -> String {
    let micros = t.nanosecond() / 1_000;
    if micros == 0 {
        format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second())
    } else {
        format!(
            "{:02}:{:02}:{:02}.{:06}",
            t.hour(),
            t.minute(),
            t.second(),
            micros
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_de_data_como_openpyxl() {
        let d = |y, m, dd| {
            NaiveDate::from_ymd_opt(y, m, dd)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        };
        assert_eq!(openpyxl_to_excel(&d(2023, 3, 15)), 45000.0);
        assert_eq!(openpyxl_to_excel(&d(1900, 3, 1)), 61.0);
        // abaixo de 1900-03-01 o openpyxl desconta 1 dia (bug de 1900)
        assert_eq!(openpyxl_to_excel(&d(1900, 1, 1)), 1.0);
        let meio_dia = NaiveDate::from_ymd_opt(2023, 3, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        assert_eq!(openpyxl_to_excel(&meio_dia), 45000.5);
    }

    #[test]
    fn numero_arredondado_para_16_digitos_como_openpyxl() {
        // "%.16g" % 0.1 == "0.1"
        assert_eq!(openpyxl_number(0.1), 0.1);
        // 17 dígitos: perde o último
        assert_eq!(openpyxl_number(12345678901234567.0), 12345678901234570.0);
        // valor com ruído na 17ª casa volta ao arredondado em 16
        let v = 123456.78901234567_f64;
        assert_eq!(
            openpyxl_number(v),
            "123456.7890123457".parse::<f64>().unwrap()
        );
        assert_eq!(openpyxl_number(45000.0), 45000.0);
        assert!(openpyxl_number(f64::NAN).is_nan());
    }

    #[test]
    fn hora_vira_texto_como_str_do_python() {
        assert_eq!(
            python_time_str(&NaiveTime::from_hms_opt(14, 30, 0).unwrap()),
            "14:30:00"
        );
        assert_eq!(
            python_time_str(&NaiveTime::from_hms_micro_opt(8, 5, 9, 500_000).unwrap()),
            "08:05:09.500000"
        );
    }

    #[test]
    fn regras_de_celula_object() {
        assert_eq!(
            object_cell(&Value::Str("=SOMA(A1)".into())),
            OutCell::Formula("=SOMA(A1)".into())
        );
        assert_eq!(
            object_cell(&Value::Str("=".into())),
            OutCell::Text("=".into())
        );
        assert_eq!(object_cell(&Value::Str(String::new())), OutCell::Blank);
        assert_eq!(
            object_cell(&Value::Float(f64::INFINITY)),
            OutCell::Text("inf".into())
        );
        assert_eq!(object_cell(&Value::Float(f64::NAN)), OutCell::Blank);
    }

    #[test]
    fn caractere_de_controle_e_recusado() {
        assert!(matches!(
            check_string("a\u{0001}b", 1, 0),
            Err(ConvertError::IllegalCharacter { row: 2, col: 0 })
        ));
        assert!(check_string("tab\tok\nquebra", 0, 0).is_ok());
    }
}
