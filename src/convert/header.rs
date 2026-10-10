//! Nomes das colunas a partir da 1ª linha, como o `read_excel` do pandas
//! 2.2.3 (motor Python, `header=0`), e que o `to_excel(index=False)` grava na
//! 1ª linha da saída.
//!
//! Fonte reproduzida: `pandas/io/parsers/python_parser.py::_infer_columns`
//! (ramo sem MultiIndex):
//!
//! ```python
//! for i, c in enumerate(line):
//!     if c == "":
//!         col_name = f"Unnamed: {i}"
//!         this_columns.append(col_name); this_unnamed_cols.append(i)
//!     else:
//!         this_columns.append(c)
//! counts = defaultdict(int)
//! col_loop_order = [i for i in range(len(this_columns))
//!                   if i not in this_unnamed_cols] + this_unnamed_cols
//! for i in col_loop_order:
//!     col = this_columns[i]; old_col = col; cur_count = counts[col]
//!     if cur_count > 0:
//!         while cur_count > 0:
//!             counts[old_col] = cur_count + 1
//!             col = f"{old_col}.{cur_count}"
//!             if col in this_columns: cur_count += 1
//!             else: cur_count = counts[col]
//!     this_columns[i] = col
//!     counts[col] = cur_count + 1
//! ```
//!
//! O dicionário `counts` usa a igualdade do Python: `True == 1 == 1.0`, e
//! texto nunca é igual a número.

use std::collections::HashMap;

use chrono::{NaiveDateTime, Timelike};

use super::Value;

/// Rótulo de coluna do pandas.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Label {
    Str(String),
    Int(i128),
    /// Float não inteiro (ou fora de `i128`), NaN incluído.
    Float(f64),
    Bool(bool),
    DateTime(NaiveDateTime),
    Time(chrono::NaiveTime),
}

impl Label {
    fn from_value(v: &Value) -> Option<Self> {
        Some(match v {
            Value::Empty => return None,
            Value::Str(s) if s.is_empty() => return None,
            Value::Str(s) => Label::Str(s.clone()),
            Value::Int(i) => Label::Int(*i),
            Value::Float(f) | Value::BigInt(f) => Label::Float(*f),
            Value::Bool(b) => Label::Bool(*b),
            Value::DateTime(d) => Label::DateTime(*d),
            Value::Time(t) => Label::Time(*t),
        })
    }

    /// Chave com a igualdade do Python (`hash`/`==` de `dict`).
    fn key(&self) -> String {
        match self {
            Label::Str(s) => format!("s:{s}"),
            Label::Int(i) => format!("n:{i}"),
            Label::Bool(b) => format!("n:{}", u8::from(*b)),
            Label::Float(f) => {
                if f.is_finite() && f.trunc() == *f && f.abs() < 1.7e38 {
                    format!("n:{}", *f as i128)
                } else if f.is_nan() {
                    "n:nan".to_string()
                } else {
                    format!("f:{}", f.to_bits())
                }
            }
            Label::DateTime(d) => format!("d:{d}"),
            Label::Time(t) => format!("t:{t}"),
        }
    }

    /// `str(label)` do Python.
    fn py_str(&self) -> String {
        match self {
            Label::Str(s) => s.clone(),
            Label::Int(i) => i.to_string(),
            Label::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            Label::Float(f) => py_float_repr(*f),
            Label::DateTime(d) => py_datetime_str(d),
            Label::Time(t) => super::write::python_time_str(t),
        }
    }
}

/// `repr(float)` do Python: dígitos mínimos de ida e volta; notação
/// científica quando o expoente decimal é menor que -4 ou maior ou igual a 16.
pub(crate) fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    // `{:e}` do Rust já dá os dígitos mínimos de ida e volta.
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("formato {:e}");
    let exp: i32 = exp.parse().expect("expoente");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    if !(-4..16).contains(&exp) {
        let (first, rest) = digits.split_at(1);
        let frac = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let esign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{first}{frac}e{esign}{:02}", exp.abs());
    }
    let n = digits.len() as i32;
    if exp >= 0 {
        let int_len = exp + 1;
        if n <= int_len {
            let zeros = "0".repeat((int_len - n) as usize);
            format!("{sign}{digits}{zeros}.0")
        } else {
            let (a, b) = digits.split_at(int_len as usize);
            format!("{sign}{a}.{b}")
        }
    } else {
        let zeros = "0".repeat((-exp - 1) as usize);
        format!("{sign}0.{zeros}{digits}")
    }
}

/// `str(datetime)` do Python: `AAAA-MM-DD HH:MM:SS[.ffffff]`.
fn py_datetime_str(d: &NaiveDateTime) -> String {
    let micros = d.nanosecond() / 1_000;
    let base = d.format("%Y-%m-%d %H:%M:%S").to_string();
    if micros == 0 {
        base
    } else {
        format!("{base}.{micros:06}")
    }
}

/// Nomes das colunas a partir da linha de cabeçalho.
pub(crate) fn column_labels(header_row: &[Value]) -> Vec<Label> {
    let mut labels: Vec<Label> = Vec::with_capacity(header_row.len());
    let mut unnamed: Vec<usize> = Vec::new();
    for (i, v) in header_row.iter().enumerate() {
        match Label::from_value(v) {
            Some(l) => labels.push(l),
            None => {
                labels.push(Label::Str(format!("Unnamed: {i}")));
                unnamed.push(i);
            }
        }
    }

    let order: Vec<usize> = (0..labels.len())
        .filter(|i| !unnamed.contains(i))
        .chain(unnamed.iter().copied())
        .collect();

    let mut counts: HashMap<String, usize> = HashMap::new();
    for i in order {
        let old = labels[i].clone();
        let old_key = old.key();
        let mut cur = counts.get(&old_key).copied().unwrap_or(0);
        let mut col = old.clone();
        if cur > 0 {
            let old_str = old.py_str();
            while cur > 0 {
                counts.insert(old_key.clone(), cur + 1);
                let candidate = format!("{old_str}.{cur}");
                col = Label::Str(candidate.clone());
                // `col in this_columns`: texto só é igual a texto.
                let exists = labels
                    .iter()
                    .any(|l| matches!(l, Label::Str(s) if *s == candidate));
                if exists {
                    cur += 1;
                } else {
                    cur = counts.get(&col.key()).copied().unwrap_or(0);
                }
            }
        }
        counts.insert(col.key(), cur + 1);
        labels[i] = col;
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn s(v: &str) -> Value {
        Value::Str(v.into())
    }

    #[test]
    fn nomes_como_o_pandas_medido() {
        // Mesmo cabeçalho do teste feito com pandas 2.2.3 no servidor.
        let d = NaiveDate::from_ymd_opt(2026, 10, 10)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let row = vec![
            Value::Empty,
            s("A"),
            s("A"),
            s("A.1"),
            Value::Int(2024),
            Value::Float(1.5),
            Value::Bool(true),
            Value::DateTime(d),
            Value::Empty,
            s("A"),
            Value::Int(1),
            Value::Int(1), // 1.0 lido como int
            Value::Int(10_000_000_000_000_000),
            Value::Float(0.1),
            s("NA"),
        ];
        let got = column_labels(&row);
        let want = vec![
            Label::Str("Unnamed: 0".into()),
            Label::Str("A".into()),
            Label::Str("A.2".into()),
            Label::Str("A.1".into()),
            Label::Int(2024),
            Label::Float(1.5),
            Label::Bool(true),
            Label::DateTime(d),
            Label::Str("Unnamed: 8".into()),
            Label::Str("A.3".into()),
            Label::Str("1.1".into()),
            Label::Str("1.2".into()),
            Label::Int(10_000_000_000_000_000),
            Label::Float(0.1),
            Label::Str("NA".into()),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn repetidos_simples() {
        let got = column_labels(&[s("X"), s("X"), s("X")]);
        assert_eq!(
            got,
            vec![
                Label::Str("X".into()),
                Label::Str("X.1".into()),
                Label::Str("X.2".into())
            ]
        );
    }

    #[test]
    fn repr_de_float_como_python() {
        assert_eq!(py_float_repr(1.5), "1.5");
        assert_eq!(py_float_repr(0.1), "0.1");
        assert_eq!(py_float_repr(1e16), "1e+16");
        assert_eq!(py_float_repr(123456789012345.6), "123456789012345.6");
        assert_eq!(py_float_repr(0.0001), "0.0001");
        assert_eq!(py_float_repr(0.00001), "1e-05");
        assert_eq!(py_float_repr(2.0), "2.0");
        assert_eq!(py_float_repr(-2.5e-7), "-2.5e-07");
    }
}
