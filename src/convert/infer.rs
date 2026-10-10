//! Inferência de tipo por coluna, como o `TextParser` do pandas 2.2.3 faz no
//! `read_excel` (motor Python, `skip_blank_lines=False`).
//!
//! Fontes reproduzidas:
//! - `pandas/io/parsers/base_parser.py::_infer_types`: tenta conversão
//!   numérica; se falhar, troca os nulos por NaN e tenta booleano.
//! - `pandas/_libs/lib.pyx::maybe_convert_numeric` (com `convert_empty=False`)
//!   e a classe `Seen` (`saw_null`, `saw_int`, `check_uint64_conflict`).
//! - `pandas/_libs/ops.pyx::maybe_convert_bool`.
//! - `pandas/_libs/parsers.pyx::STR_NA_VALUES` e `sanitize_objects`.
//! - `pandas/_libs/src/parser/tokenizer.c::floatify`/`to_double` (texto
//!   numérico com espaços nas bordas, sinal, decimal com ponto e expoente).

use super::read::Grid;
use super::{ConvertError, Limits, Value};

/// `pandas._libs.parsers.STR_NA_VALUES` (pandas 2.2.3).
const STR_NA_VALUES: [&str; 19] = [
    "-1.#IND", "1.#QNAN", "1.#IND", "-1.#QNAN", "#N/A N/A", "#N/A", "N/A", "n/a", "NA", "<NA>",
    "#NA", "NULL", "null", "NaN", "-NaN", "nan", "-nan", "", "None",
];

const TRUE_VALUES: [&str; 3] = ["True", "TRUE", "true"];
const FALSE_VALUES: [&str; 3] = ["False", "FALSE", "false"];

const INT64_MIN: i128 = i64::MIN as i128;
const INT64_MAX: i128 = i64::MAX as i128;
const UINT64_MAX: i128 = u64::MAX as i128;

/// Valor de uma coluna `object`, depois da troca dos nulos por NaN.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ObjValue {
    Nan,
    Value(Value),
}

/// Coluna já com o dtype que o pandas atribuiria.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Column {
    /// `float64`: NaN representa nulo.
    Float(Vec<f64>),
    /// `int64` ou `uint64`.
    Int(Vec<i128>),
    Bool(Vec<bool>),
    Object(Vec<ObjValue>),
}

fn is_na_str(s: &str) -> bool {
    STR_NA_VALUES.contains(&s)
}

/// Infere as colunas das linhas de dados (linha 0 é o cabeçalho, descartado).
pub(crate) fn infer_columns(grid: &Grid, limits: &Limits) -> Result<Vec<Column>, ConvertError> {
    if grid.height <= 1 {
        return Ok(Vec::new());
    }
    (0..grid.width as usize)
        .map(|c| {
            limits.check_deadline()?;
            let values: Vec<&Value> = grid.rows[1..].iter().map(|row| &row[c]).collect();
            Ok(infer_column(&values))
        })
        .collect()
}

/// `_infer_types` para uma coluna `object` (o `TextParser` monta as colunas
/// como `object` a partir das linhas).
pub(crate) fn infer_column(values: &[&Value]) -> Column {
    match maybe_convert_numeric(values) {
        Numeric::Converted(col) => col,
        Numeric::Uint64Conflict => {
            // `check_uint64_conflict`: o pandas devolve o array original, sem
            // trocar os nulos, e o `maybe_convert_bool` recebe esse array.
            let objs: Vec<ObjValue> = values
                .iter()
                .map(|v| ObjValue::Value((*v).clone()))
                .collect();
            maybe_convert_bool(objs)
        }
        Numeric::Failed => {
            let objs = sanitize_objects(values);
            maybe_convert_bool(objs)
        }
    }
}

enum Numeric {
    Converted(Column),
    Failed,
    Uint64Conflict,
}

#[derive(Default)]
struct Seen {
    null: bool,
    float: bool,
    int: bool,
    uint: bool,
    sint: bool,
    boolean: bool,
}

impl Seen {
    fn saw_null(&mut self) {
        self.null = true;
        self.float = true;
    }

    fn saw_int(&mut self, val: i128) {
        self.int = true;
        self.sint = self.sint || (INT64_MIN..0).contains(&val);
        self.uint = self.uint || (INT64_MAX < val && val <= UINT64_MAX);
    }
}

/// Número de uma célula durante a conversão numérica.
#[derive(Clone, Copy)]
enum Num {
    Float(f64),
    Int(i128),
    Bool(bool),
}

fn maybe_convert_numeric(values: &[&Value]) -> Numeric {
    let mut seen = Seen::default();
    let mut nums: Vec<Num> = Vec::with_capacity(values.len());

    for v in values {
        match v {
            Value::Empty => {
                // '' está em STR_NA_VALUES.
                seen.saw_null();
                nums.push(Num::Float(f64::NAN));
            }
            Value::Str(s) if is_na_str(s) => {
                seen.saw_null();
                nums.push(Num::Float(f64::NAN));
            }
            Value::Float(f) => {
                if f.is_nan() {
                    seen.null = true;
                }
                seen.float = true;
                nums.push(Num::Float(*f));
            }
            Value::Int(i) => {
                let val = *i;
                seen.saw_int(val);
                if val > UINT64_MAX {
                    seen.float = true;
                }
                if val < INT64_MIN || (seen.sint && seen.uint) {
                    seen.float = true;
                }
                nums.push(Num::Int(val));
            }
            Value::BigInt(f) => {
                // int do Python fora de [INT64_MIN, UINT64_MAX]: só marca int_
                // e float_ (o valor vale como float).
                seen.int = true;
                seen.float = true;
                nums.push(Num::Float(*f));
            }
            Value::Bool(b) => {
                seen.boolean = true;
                nums.push(Num::Bool(*b));
            }
            Value::DateTime(_) | Value::Time(_) => return Numeric::Failed,
            Value::Str(s) => match floatify(s) {
                None => return Numeric::Failed,
                Some(Floatified::Float(f)) => {
                    if f.is_nan() {
                        seen.null = true;
                    }
                    seen.float = true;
                    nums.push(Num::Float(f));
                }
                Some(Floatified::Int(as_int)) => {
                    let Some(as_int) = as_int else {
                        // "Integer out of range." (ValueError) sem coerce.
                        return Numeric::Failed;
                    };
                    if !(INT64_MIN..=UINT64_MAX).contains(&as_int) {
                        return Numeric::Failed;
                    }
                    seen.saw_int(as_int);
                    seen.float = seen.float || (seen.uint && seen.sint);
                    nums.push(Num::Int(as_int));
                }
            },
        }
    }

    // `check_uint64_conflict` (sem coerce_numeric).
    if seen.uint && (seen.null || seen.sint) {
        return Numeric::Uint64Conflict;
    }

    let col = if seen.float {
        Column::Float(
            nums.iter()
                .map(|n| match *n {
                    Num::Float(f) => f,
                    Num::Int(i) => i as f64,
                    Num::Bool(b) => f64::from(u8::from(b)),
                })
                .collect(),
        )
    } else if seen.int {
        Column::Int(
            nums.iter()
                .map(|n| match *n {
                    Num::Int(i) => i,
                    Num::Bool(b) => i128::from(b),
                    Num::Float(f) => f as i128,
                })
                .collect(),
        )
    } else if seen.boolean {
        Column::Bool(
            nums.iter()
                .map(|n| match *n {
                    Num::Bool(b) => b,
                    Num::Int(i) => i != 0,
                    Num::Float(f) => f != 0.0,
                })
                .collect(),
        )
    } else {
        // Coluna sem valores: o pandas devolve `ints` vazio.
        Column::Int(Vec::new())
    };
    Numeric::Converted(col)
}

/// `sanitize_objects`: valores em `na_values` viram NaN.
fn sanitize_objects(values: &[&Value]) -> Vec<ObjValue> {
    values
        .iter()
        .map(|v| match v {
            Value::Empty => ObjValue::Nan,
            Value::Str(s) if is_na_str(s) => ObjValue::Nan,
            other => ObjValue::Value((*other).clone()),
        })
        .collect()
}

/// `maybe_convert_bool` com os conjuntos padrão de verdadeiro e falso.
fn maybe_convert_bool(values: Vec<ObjValue>) -> Column {
    let mut out = Vec::with_capacity(values.len());
    let mut has_na = false;
    for v in &values {
        match v {
            ObjValue::Value(Value::Bool(b)) => out.push(Some(*b)),
            ObjValue::Value(Value::Str(s)) if TRUE_VALUES.contains(&s.as_str()) => {
                out.push(Some(true))
            }
            ObjValue::Value(Value::Str(s)) if FALSE_VALUES.contains(&s.as_str()) => {
                out.push(Some(false))
            }
            ObjValue::Nan => {
                has_na = true;
                out.push(None);
            }
            ObjValue::Value(Value::Float(f)) if f.is_nan() => {
                has_na = true;
                out.push(None);
            }
            _ => return Column::Object(values),
        }
    }
    if has_na {
        Column::Object(
            out.into_iter()
                .map(|b| match b {
                    Some(b) => ObjValue::Value(Value::Bool(b)),
                    None => ObjValue::Nan,
                })
                .collect(),
        )
    } else {
        Column::Bool(out.into_iter().map(|b| b.unwrap_or(false)).collect())
    }
}

/// Resultado do `floatify` do pandas para texto.
#[derive(Debug, PartialEq)]
enum Floatified {
    Float(f64),
    /// Texto inteiro (`maybe_int`); `None` quando não cabe em `i128`, o que
    /// já está fora de `uint64` e falha no pandas.
    Int(Option<i128>),
}

/// `floatify` → `to_double` → `precise_xstrtod(decimal='.', sci='E',
/// tsep='\0', skip_trailing=1)`, com o tratamento de `inf`/`infinity`.
fn floatify(s: &str) -> Option<Floatified> {
    if let Some(r) = to_double(s) {
        return Some(r);
    }
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => Some(Floatified::Float(f64::INFINITY)),
        "-inf" | "-infinity" => Some(Floatified::Float(f64::NEG_INFINITY)),
        _ => None,
    }
}

fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

fn to_double(s: &str) -> Option<Floatified> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && is_c_space(bytes[i]) {
        i += 1;
    }
    let num_start = i;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut digits = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    let mut maybe_int = true;
    if i < bytes.len() && bytes[i] == b'.' {
        maybe_int = false;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let save = i;
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            // Expoente sem dígitos: o 'e' não é consumido e sobra no fim.
            i = save;
        } else {
            maybe_int = false;
        }
    }
    let num_end = i;
    while i < bytes.len() && is_c_space(bytes[i]) {
        i += 1;
    }
    if i != bytes.len() {
        return None;
    }
    let numeric = &s[num_start..num_end];
    let f: f64 = numeric.parse().ok()?;
    if f.is_infinite() {
        // Estouro (ERANGE) no precise_xstrtod.
        return None;
    }
    if maybe_int {
        // int(val) do Python sobre o texto original (aceita espaço e sinal).
        Some(Floatified::Int(numeric.parse::<i128>().ok()))
    } else {
        Some(Floatified::Float(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn s(v: &str) -> Value {
        Value::Str(v.to_string())
    }

    fn infer(vals: &[Value]) -> Column {
        let refs: Vec<&Value> = vals.iter().collect();
        infer_column(&refs)
    }

    #[test]
    fn so_inteiros_vira_int() {
        assert_eq!(
            infer(&[Value::Int(1), Value::Int(2)]),
            Column::Int(vec![1, 2])
        );
    }

    #[test]
    fn inteiros_com_vazio_viram_float() {
        let c = infer(&[Value::Int(1), Value::Empty, Value::Int(3)]);
        match c {
            Column::Float(v) => {
                assert_eq!(v[0], 1.0);
                assert!(v[1].is_nan());
                assert_eq!(v[2], 3.0);
            }
            other => panic!("esperado Float, veio {other:?}"),
        }
    }

    #[test]
    fn texto_numerico_vira_numero_e_perde_zero_a_esquerda() {
        assert_eq!(
            infer(&[s("00123"), s(" 45 "), s("+6")]),
            Column::Int(vec![123, 45, 6])
        );
        assert_eq!(infer(&[s("1.5"), s("2")]), Column::Float(vec![1.5, 2.0]));
        assert_eq!(infer(&[s("1e3")]), Column::Float(vec![1000.0]));
    }

    #[test]
    fn texto_nao_numerico_mantem_object_e_troca_nulos() {
        let c = infer(&[s("abc"), s("NA"), Value::Empty, Value::Int(7)]);
        assert_eq!(
            c,
            Column::Object(vec![
                ObjValue::Value(s("abc")),
                ObjValue::Nan,
                ObjValue::Nan,
                ObjValue::Value(Value::Int(7)),
            ])
        );
    }

    #[test]
    fn virgula_decimal_nao_e_numero() {
        assert!(matches!(infer(&[s("1,5")]), Column::Object(_)));
    }

    #[test]
    fn chave_nfe_de_44_digitos_continua_texto() {
        let chave = "35201234567890123456550010000000011000000010";
        assert_eq!(chave.len(), 44);
        assert_eq!(
            infer(&[s(chave)]),
            Column::Object(vec![ObjValue::Value(s(chave))])
        );
    }

    #[test]
    fn inteiro_acima_de_int64_vira_uint64() {
        let big = "18446744073709551615"; // u64::MAX
        assert_eq!(infer(&[s(big)]), Column::Int(vec![UINT64_MAX]));
    }

    #[test]
    fn uint64_com_nulo_devolve_original_sem_trocar_nulos() {
        let big = "18446744073709551615";
        assert_eq!(
            infer(&[s(big), s("NA")]),
            Column::Object(vec![ObjValue::Value(s(big)), ObjValue::Value(s("NA"))])
        );
    }

    #[test]
    fn booleanos_e_texto_booleano() {
        assert_eq!(
            infer(&[Value::Bool(true), Value::Bool(false)]),
            Column::Bool(vec![true, false])
        );
        assert_eq!(
            infer(&[s("True"), s("false"), s("TRUE")]),
            Column::Bool(vec![true, false, true])
        );
        assert_eq!(
            infer(&[s("True"), Value::Empty]),
            Column::Object(vec![ObjValue::Value(Value::Bool(true)), ObjValue::Nan])
        );
    }

    #[test]
    fn coluna_toda_nula_vira_float_nan() {
        match infer(&[Value::Empty, s("NULL")]) {
            Column::Float(v) => assert!(v.iter().all(|f| f.is_nan())),
            other => panic!("esperado Float, veio {other:?}"),
        }
    }

    #[test]
    fn datas_ficam_object() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 10)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(
            infer(&[Value::DateTime(d), Value::Empty]),
            Column::Object(vec![ObjValue::Value(Value::DateTime(d)), ObjValue::Nan])
        );
    }

    #[test]
    fn inf_em_texto() {
        assert_eq!(
            infer(&[s("inf"), s("-Infinity")]),
            Column::Float(vec![f64::INFINITY, f64::NEG_INFINITY])
        );
    }

    #[test]
    fn floatify_casos_de_borda() {
        assert_eq!(floatify("12"), Some(Floatified::Int(Some(12))));
        assert_eq!(floatify("  -0 "), Some(Floatified::Int(Some(0))));
        assert_eq!(floatify(".5"), Some(Floatified::Float(0.5)));
        assert_eq!(floatify("5."), Some(Floatified::Float(5.0)));
        assert_eq!(floatify("1e"), None);
        assert_eq!(floatify("abc"), None);
        assert_eq!(floatify("1 2"), None);
        assert_eq!(floatify("-"), None);
    }
}
