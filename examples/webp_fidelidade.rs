//! Compara o `/webp` com o `cwebp` do legado nas mesmas imagens.
//!
//! - `args <img>...`: imprime `<img>\t<W> <H>`, o `-resize` que o legado
//!   passaria ao `cwebp` (`TTPictureHelper.SIZE(400)`).
//! - `comparar <dir_cwebp> <img>...`: converte cada imagem pela biblioteca e
//!   compara com `<dir_cwebp>/<nome>.webp` (dimensões, tamanho e PSNR entre as
//!   duas saídas).

use std::path::Path;

use xlsconvert::webp::{resize_dims, to_webp, WebpParams};

fn psnr(a: &image::RgbaImage, b: &image::RgbaImage) -> f64 {
    let mut se = 0f64;
    let mut n = 0f64;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for c in 0..3 {
            let d = f64::from(pa[c]) - f64::from(pb[c]);
            se += d * d;
            n += 1.0;
        }
    }
    let mse = se / n;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("args") => {
            for f in &args[1..] {
                let (w, h) = image::image_dimensions(f).expect(f);
                let (rw, rh) = resize_dims(w, h, 400);
                println!("{f}\t{rw} {rh}");
            }
        }
        Some("comparar") => {
            let dir = Path::new(&args[1]);
            println!("arquivo\torig\tcwebp\tnovo\tbytes_cwebp\tbytes_novo\tpsnr_db");
            for f in &args[2..] {
                let p = Path::new(f);
                let nome = p.file_stem().unwrap().to_string_lossy();
                let orig = image::image_dimensions(p).unwrap();
                let ref_bytes = std::fs::read(dir.join(format!("{nome}.webp"))).unwrap();
                let novo =
                    to_webp(&std::fs::read(p).unwrap(), &WebpParams::default(), 1 << 32).unwrap();
                let a = image::load_from_memory(&ref_bytes).unwrap().to_rgba8();
                let b = image::load_from_memory(&novo).unwrap().to_rgba8();
                let ps = if a.dimensions() == b.dimensions() {
                    format!("{:.1}", psnr(&a, &b))
                } else {
                    "dim!".into()
                };
                println!(
                    "{nome}\t{}x{}\t{}x{}\t{}x{}\t{}\t{}\t{ps}",
                    orig.0,
                    orig.1,
                    a.width(),
                    a.height(),
                    b.width(),
                    b.height(),
                    ref_bytes.len(),
                    novo.len()
                );
            }
        }
        _ => eprintln!("uso: webp_fidelidade args <img>... | comparar <dir_cwebp> <img>..."),
    }
}
