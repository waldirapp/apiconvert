//! Testes ponta a ponta da conversão com fixtures sintéticas geradas no
//! próprio teste (nenhum arquivo de cliente no repositório).
//!
//! A entrada é montada com o rust_xlsxwriter; a saída é lida com o calamine.

use std::io::Cursor;

use calamine::{Data, Reader, Xlsx};
use rust_xlsxwriter::{ExcelDateTime, Format, Workbook};
use xlsconvert::convert::{convert, ConvertError, Limits};

fn read_output(xlsx: &[u8]) -> (String, Vec<Vec<Data>>) {
    let mut wb: Xlsx<_> = Xlsx::new(Cursor::new(xlsx.to_vec())).expect("saída é xlsx válido");
    let name = wb.sheet_names()[0].clone();
    let range = wb
        .worksheet_range_at(0)
        .expect("uma aba")
        .expect("aba legível");
    let (h, w) = match range.start() {
        Some((r, c)) => {
            let (hh, ww) = range.get_size();
            (r as usize + hh, c as usize + ww)
        }
        None => (0, 0),
    };
    let mut grid = vec![vec![Data::Empty; w]; h];
    if let Some((r0, c0)) = range.start() {
        for (r, c, cell) in range.used_cells() {
            grid[r0 as usize + r][c0 as usize + c] = cell.clone();
        }
    }
    (name, grid)
}

fn as_f64(d: &Data) -> Option<f64> {
    match d {
        Data::Float(f) => Some(*f),
        Data::Int(i) => Some(*i as f64),
        Data::DateTime(dt) => Some(dt.as_f64()),
        _ => None,
    }
}

#[test]
fn primeira_linha_descartada_so_primeira_aba_e_nome_sheet1() {
    let mut wb = Workbook::new();
    let s = wb.add_worksheet();
    s.set_name("Dados").unwrap();
    s.write_string(0, 0, "CABECALHO").unwrap();
    s.write_string(0, 1, "OUTRO").unwrap();
    s.write_string(1, 0, "linha 1").unwrap();
    s.write_string(1, 1, "x").unwrap();
    s.write_string(2, 0, "linha 2").unwrap();
    s.write_string(2, 1, "y").unwrap();
    let outra = wb.add_worksheet();
    outra.write_string(0, 0, "ignorada").unwrap();
    let input = wb.save_to_buffer().unwrap();

    let out = convert(&input, Limits::default()).unwrap();
    let (name, grid) = read_output(&out);
    assert_eq!(name, "Sheet1");
    assert_eq!(grid.len(), 2);
    assert_eq!(grid[0][0], Data::String("linha 1".into()));
    assert_eq!(grid[1][1], Data::String("y".into()));
}

#[test]
fn texto_numerico_vira_numero_e_coluna_mista_mantem_texto() {
    let mut wb = Workbook::new();
    let s = wb.add_worksheet();
    s.write_string(0, 0, "COD").unwrap();
    s.write_string(0, 1, "MISTA").unwrap();
    s.write_string(1, 0, "00123").unwrap();
    s.write_string(1, 1, "00123").unwrap();
    s.write_string(2, 0, "456").unwrap();
    s.write_string(2, 1, "ABC").unwrap();
    let input = wb.save_to_buffer().unwrap();

    let (_, grid) = read_output(&convert(&input, Limits::default()).unwrap());
    assert_eq!(as_f64(&grid[0][0]), Some(123.0));
    assert_eq!(as_f64(&grid[1][0]), Some(456.0));
    assert_eq!(grid[0][1], Data::String("00123".into()));
    assert_eq!(grid[1][1], Data::String("ABC".into()));
}

#[test]
fn nulos_viram_vazio_e_linha_em_branco_no_meio_e_mantida() {
    let mut wb = Workbook::new();
    let s = wb.add_worksheet();
    s.write_string(0, 0, "A").unwrap();
    s.write_string(1, 0, "texto").unwrap();
    // linha 2 em branco
    s.write_string(3, 0, "NA").unwrap();
    s.write_string(4, 0, "fim").unwrap();
    let input = wb.save_to_buffer().unwrap();

    let (_, grid) = read_output(&convert(&input, Limits::default()).unwrap());
    assert_eq!(grid[0][0], Data::String("texto".into()));
    assert_eq!(grid[1][0], Data::Empty);
    assert_eq!(grid[2][0], Data::Empty);
    assert_eq!(grid[3][0], Data::String("fim".into()));
}

#[test]
fn datas_com_formato_e_hora_vira_texto() {
    let mut wb = Workbook::new();
    let s = wb.add_worksheet();
    let fmt_data = Format::new().set_num_format("dd/mm/yyyy");
    let fmt_hora = Format::new().set_num_format("hh:mm:ss");
    s.write_string(0, 0, "DATA").unwrap();
    s.write_string(0, 1, "HORA").unwrap();
    let data = ExcelDateTime::from_ymd(2026, 10, 10).unwrap();
    s.write_datetime_with_format(1, 0, &data, &fmt_data)
        .unwrap();
    s.write_number_with_format(1, 1, 0.5, &fmt_hora).unwrap();
    let input = wb.save_to_buffer().unwrap();

    let (_, grid) = read_output(&convert(&input, Limits::default()).unwrap());
    // 2026-10-10 = serial 46305
    assert_eq!(as_f64(&grid[0][0]), Some(46305.0));
    assert_eq!(grid[0][1], Data::String("12:00:00".into()));
}

#[test]
fn inteiros_com_vazio_ficam_float_e_booleanos_preservados() {
    let mut wb = Workbook::new();
    let s = wb.add_worksheet();
    s.write_string(0, 0, "N").unwrap();
    s.write_string(0, 1, "B").unwrap();
    s.write_number(1, 0, 10.0).unwrap();
    s.write_boolean(1, 1, true).unwrap();
    s.write_number(3, 0, 30.0).unwrap();
    s.write_boolean(2, 1, false).unwrap();
    s.write_boolean(3, 1, true).unwrap();
    let input = wb.save_to_buffer().unwrap();

    let (_, grid) = read_output(&convert(&input, Limits::default()).unwrap());
    assert_eq!(as_f64(&grid[0][0]), Some(10.0));
    assert_eq!(grid[1][0], Data::Empty);
    assert_eq!(as_f64(&grid[2][0]), Some(30.0));
    assert_eq!(grid[0][1], Data::Bool(true));
    assert_eq!(grid[1][1], Data::Bool(false));
}

#[test]
fn colunas_vazias_a_esquerda_mantem_a_posicao() {
    let mut wb = Workbook::new();
    let s = wb.add_worksheet();
    s.write_string(0, 2, "C").unwrap();
    s.write_string(1, 2, "valor").unwrap();
    let input = wb.save_to_buffer().unwrap();

    let (_, grid) = read_output(&convert(&input, Limits::default()).unwrap());
    assert_eq!(grid[0][2], Data::String("valor".into()));
}

#[test]
fn so_cabecalho_gera_aba_vazia() {
    let mut wb = Workbook::new();
    wb.add_worksheet()
        .write_string(0, 0, "SO CABECALHO")
        .unwrap();
    let input = wb.save_to_buffer().unwrap();
    let (name, grid) = read_output(&convert(&input, Limits::default()).unwrap());
    assert_eq!(name, "Sheet1");
    assert!(grid.is_empty());
}

#[test]
fn entradas_invalidas() {
    assert!(matches!(
        convert(b"", Limits::default()),
        Err(ConvertError::EmptyInput)
    ));
    assert!(matches!(
        convert(
            b"<html><table><tr><td>1</td></tr></table></html>",
            Limits::default()
        ),
        Err(ConvertError::UnsupportedFormat)
    ));
    assert!(matches!(
        convert(b"PK\x03\x04lixo-que-nao-e-zip", Limits::default()),
        Err(ConvertError::InvalidWorkbook)
    ));
}

#[test]
fn limite_de_celulas() {
    let mut wb = Workbook::new();
    let s = wb.add_worksheet();
    s.write_string(0, 0, "A").unwrap();
    s.write_string(9, 9, "fim").unwrap();
    let input = wb.save_to_buffer().unwrap();
    let limits = Limits {
        max_cells: 50,
        ..Limits::default()
    };
    assert!(matches!(
        convert(&input, limits),
        Err(ConvertError::TooLarge(_))
    ));
}
