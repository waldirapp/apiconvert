//! Conversão de planilha com a semântica exata de um conversor em pandas.
//!
//! O conversor de referência executa, para cada arquivo recebido:
//!
//! ```python
//! df = pd.read_excel(r'<tmp>.xls')
//! df.to_excel(r'<tmp>.xlsx', index=False)
//! ```
//!
//! com pandas 2.2.3, xlrd 2.0.1 e openpyxl 3.1.5 (versões instaladas no
//! ambiente de referência). Os sistemas consomem o resultado, então o
//! contrato é o comportamento observável dessa linha, inclusive as
//! peculiaridades: só a primeira aba, a primeira linha virando nomes de
//! coluna (`Unnamed: N` para vazio, sufixo `.N` para repetido), textos
//! numéricos convertidos em número e horas gravadas como texto. Cada etapa
//! cita a fonte do pandas que reproduz; ver `read.rs`, `header.rs`,
//! `infer.rs` e `write.rs`.

mod header;
mod infer;
mod read;
mod write;

use chrono::{NaiveDateTime, NaiveTime};
use thiserror::Error;

pub use read::InputFormat;

/// Valor de uma célula depois da leitura no estilo do `xlrd` + pandas
/// (`pandas/io/excel/_xlrd.py::_parse_cell`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    /// Célula vazia: o `xlrd` devolve `''`, que o pandas trata como nulo.
    Empty,
    /// Número não inteiro, NaN (célula de erro) ou data fora do intervalo do
    /// Python, que o pandas mantém como o float original.
    Float(f64),
    /// Número inteiro (`int(x) == x`). `i128` cobre todo float integral
    /// abaixo de 2^127; acima disso vale `BigInt`.
    Int(i128),
    /// Float integral grande demais para `i128`: no Python vira um `int`
    /// enorme, que o pandas trata como fora do intervalo de `uint64`.
    BigInt(f64),
    Bool(bool),
    Str(String),
    DateTime(NaiveDateTime),
    /// Data no dia-época, que o pandas converte em `datetime.time`.
    Time(NaiveTime),
}

/// Limites de entrada, aplicados antes de montar a saída.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_rows: u32,
    pub max_cols: u32,
    pub max_cells: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rows: 1_048_576,
            max_cols: 16_384,
            max_cells: 5_000_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConvertError {
    #[error("arquivo vazio")]
    EmptyInput,
    #[error("formato não suportado: esperado .xls (BIFF/CFB) ou .xlsx (ZIP)")]
    UnsupportedFormat,
    #[error("planilha inválida ou corrompida")]
    InvalidWorkbook,
    #[error("planilha sem abas")]
    NoSheet,
    #[error("planilha acima do limite ({0})")]
    TooLarge(&'static str),
    #[error("texto com caractere de controle não aceito no .xlsx (linha {row}, coluna {col})")]
    IllegalCharacter { row: u32, col: u16 },
    #[error("texto acima de 32767 caracteres (linha {row}, coluna {col})")]
    StringTooLong { row: u32, col: u16 },
    #[error("falha ao gerar o .xlsx")]
    Write,
}

/// Converte o conteúdo de um `.xls` ou `.xlsx` no `.xlsx` que o conversor de referência
/// legada produziria para o mesmo arquivo.
pub fn convert(input: &[u8], limits: Limits) -> Result<Vec<u8>, ConvertError> {
    if input.is_empty() {
        return Err(ConvertError::EmptyInput);
    }
    let format = read::detect(input).ok_or(ConvertError::UnsupportedFormat)?;
    let grid = read::read_first_sheet(input, format, limits)?;
    let labels = grid
        .rows
        .first()
        .map(|row| header::column_labels(row))
        .unwrap_or_default();
    let columns = infer::infer_columns(&grid);
    write::write_xlsx(&labels, &columns, grid.data_rows())
}
