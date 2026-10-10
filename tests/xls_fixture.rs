//! Conversão de um `.xls` BIFF8 real (caminho do xlrd no pandas).
//!
//! `tests/fixtures/tipos.xls` é gerado a partir de `tests/fixtures/tipos.fods`
//! (planilha-fonte em texto, versionada) com:
//!
//! ```text
//! libreoffice --headless --convert-to 'xls:MS Excel 97' tipos.fods
//! ```

use std::io::Cursor;

use calamine::{Data, Reader, Xlsx};
use xlsconvert::convert::{convert, Limits};

const FIXTURE: &[u8] = include_bytes!("fixtures/tipos.xls");

fn output_grid() -> (String, Vec<Vec<Data>>) {
    let out = convert(FIXTURE, Limits::default()).expect("conversão do .xls");
    let mut wb: Xlsx<_> = Xlsx::new(Cursor::new(out)).expect("saída xlsx");
    let name = wb.sheet_names()[0].clone();
    let range = wb.worksheet_range_at(0).unwrap().unwrap();
    let (r0, c0) = range.start().unwrap();
    let (h, w) = range.get_size();
    let mut grid = vec![vec![Data::Empty; c0 as usize + w]; r0 as usize + h];
    for (r, c, cell) in range.used_cells() {
        grid[r0 as usize + r][c0 as usize + c] = cell.clone();
    }
    (name, grid)
}

fn num(d: &Data) -> f64 {
    match d {
        Data::Float(f) => *f,
        Data::Int(i) => *i as f64,
        Data::DateTime(dt) => dt.as_f64(),
        other => panic!("esperado número, veio {other:?}"),
    }
}

#[test]
fn xls_biff_com_tipos_variados() {
    let (name, g) = output_grid();
    assert_eq!(name, "Sheet1");
    assert_eq!(g.len(), 3, "cabeçalho + 2 linhas de dados");

    // Cabeçalho: nomes das colunas na 1ª linha.
    let cab: Vec<Data> = [
        "CODIGO", "DATA", "HORA", "VALOR", "INTEIRO", "BOOL", "TEXTO",
    ]
    .iter()
    .map(|s| Data::String((*s).into()))
    .collect();
    assert_eq!(g[0], cab);

    // CODIGO: texto numérico vira número (perde o zero à esquerda).
    assert_eq!(num(&g[1][0]), 123.0);
    assert_eq!(num(&g[2][0]), 456.0);

    // DATA: serial com formato de data-hora.
    assert_eq!(num(&g[1][1]), 46305.0); // 2026-10-10
    assert_eq!(num(&g[2][1]), 45351.0); // 2024-02-29
    assert!(
        matches!(g[1][1], Data::DateTime(_)),
        "célula de data com formato de data"
    );

    // HORA: vira texto.
    assert_eq!(g[1][2], Data::String("14:30:00".into()));
    assert_eq!(g[2][2], Data::String("08:05:09".into()));

    // VALOR: float com vazio.
    assert_eq!(num(&g[1][3]), 10.5);
    assert_eq!(g[2][3], Data::Empty);

    // INTEIRO
    assert_eq!(num(&g[1][4]), 1.0);
    assert_eq!(num(&g[2][4]), 2.0);

    // BOOL: o LibreOffice grava booleano no .xls como número com formato
    // VERDADEIRO/FALSO (o calamine lê Int(1)/Int(0)); o xlrd também lê número,
    // então o pandas grava 1 e 0. O caminho booleano real está coberto pelos
    // testes com .xlsx em convert_semantics.rs.
    assert_eq!(num(&g[1][5]), 1.0);
    assert_eq!(num(&g[2][5]), 0.0);

    // TEXTO: acento preservado; "NA" vira vazio.
    assert_eq!(g[1][6], Data::String("ação".into()));
    assert_eq!(g[2][6], Data::Empty);
}
