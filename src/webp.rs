//! Conversão de imagem em WebP com o resultado do `cwebp` usado pelo x-duall.
//!
//! O legado (`Duall.Features.WebpImage.ConvertImgToWebp` +
//! `Duall.Conv.CWebP.TWebPConverter`) chama o `cwebp` assim:
//!
//! ```text
//! cwebp -quiet -q 50 -m 6 -resize W H -jpeg_like -mt <entrada> -o <saida>
//! ```
//!
//! com `W H` dados por `TTPictureHelper.SIZE(400)`: o lado maior vira 400 e o
//! outro fica 0, que o `cwebp` interpreta como "manter a proporção" (imagem
//! quadrada vai a 400×400). A imagem menor que 400 também é ampliada.
//!
//! Aqui a mesma libwebp faz o trabalho, sem processo externo: a imagem é
//! decodificada pelo crate `image` (JPEG, PNG, GIF, BMP, TIFF, WebP e ICO,
//! detectados pelo conteúdo), importada em ARGB como o `cwebp` faz quando há
//! `-resize`, redimensionada por `WebPPictureRescale` e codificada com a mesma
//! configuração. Os bytes podem diferir do `cwebp` 1.2.1 do servidor por causa
//! da versão da libwebp; o resultado visual é o mesmo.
//!
//! Divergência intencional: imagem com transparência usa `-m 4` em vez de
//! `-m 6` (ver `method_for`).

use std::io::Cursor;

use image::{DynamicImage, ImageError, ImageReader};
use libwebp_sys as sys;

/// Parâmetros da conversão; o padrão é o do legado.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WebpParams {
    /// Tamanho do lado maior; 0 mantém o tamanho original.
    pub max_side: u32,
    /// Qualidade `-q` (0 a 100).
    pub quality: f32,
}

impl Default for WebpParams {
    fn default() -> Self {
        Self {
            max_side: 400,
            quality: 50.0,
        }
    }
}

/// Teto do lado pedido em `max_side`, para não ampliar a imagem sem limite.
pub const MAX_SIDE_LIMIT: u32 = 4096;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WebpError {
    #[error("imagem vazia")]
    Empty,
    #[error("formato de imagem não suportado")]
    Unsupported,
    #[error("imagem inválida ou corrompida")]
    Invalid,
    #[error("imagem grande demais")]
    TooLarge,
    #[error("parâmetro inválido: {0}")]
    Param(&'static str),
    #[error("falha ao gerar o WebP")]
    Encode,
}

/// Converte `input` em WebP. `max_pixels` limita largura × altura da imagem
/// decodificada (proteção contra imagem pequena em bytes e enorme em pixels).
pub fn to_webp(input: &[u8], params: &WebpParams, max_pixels: u64) -> Result<Vec<u8>, WebpError> {
    if input.is_empty() {
        return Err(WebpError::Empty);
    }
    if params.max_side > MAX_SIDE_LIMIT {
        return Err(WebpError::Param("max"));
    }
    if !(0.0..=100.0).contains(&params.quality) {
        return Err(WebpError::Param("q"));
    }
    let img = decode(input, max_pixels)?;
    let (w, h) = (img.width(), img.height());
    let (rw, rh) = resize_dims(w, h, params.max_side);
    encode(img, rw, rh, params.quality, method_for)
}

/// Largura e altura pelo cabeçalho, sem decodificar a imagem.
pub fn dimensions(input: &[u8]) -> Option<(u32, u32)> {
    reader(input).ok()?.into_dimensions().ok()
}

fn reader(input: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, WebpError> {
    let r = ImageReader::new(Cursor::new(input))
        .with_guessed_format()
        .map_err(|_| WebpError::Invalid)?;
    if r.format().is_none() {
        return Err(WebpError::Unsupported);
    }
    Ok(r)
}

fn decode(input: &[u8], max_pixels: u64) -> Result<DynamicImage, WebpError> {
    // Primeiro só o cabeçalho, para recusar pelo tamanho antes de alocar.
    let (w, h) = reader(input)?.into_dimensions().map_err(map_image_error)?;
    if w == 0 || h == 0 {
        return Err(WebpError::Invalid);
    }
    if u64::from(w) * u64::from(h) > max_pixels {
        return Err(WebpError::TooLarge);
    }
    let mut r = reader(input)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(w);
    limits.max_image_height = Some(h);
    // RGBA de 16 bits por canal é o maior formato intermediário.
    limits.max_alloc = Some(max_pixels.saturating_mul(8));
    r.limits(limits);
    r.decode().map_err(map_image_error)
}

fn map_image_error(e: ImageError) -> WebpError {
    match e {
        ImageError::Limits(_) => WebpError::TooLarge,
        ImageError::Unsupported(_) => WebpError::Unsupported,
        _ => WebpError::Invalid,
    }
}

/// `TTPictureHelper.SIZE`: o lado maior recebe `max_side` e o outro 0
/// (proporção mantida pelo `WebPPictureRescale`); quadrada vai a
/// `max_side × max_side`. `max_side == 0` mantém o original.
pub fn resize_dims(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    if max_side == 0 {
        return (0, 0);
    }
    if h > w {
        (0, max_side)
    } else if w > h {
        (max_side, 0)
    } else {
        (max_side, max_side)
    }
}

/// `-m` do legado (6), exceto em imagem com transparência: ali o método 6
/// faz a compressão sem perdas do canal alfa levar segundos (de 1,4 a 6 s
/// por imagem de 400 px no servidor, contra 0,04 s no método 4), com ganho de
/// cerca de 5% no tamanho. Divergência intencional em relação ao `cwebp`.
pub fn method_for(transparente: bool) -> i32 {
    if transparente {
        4
    } else {
        6
    }
}

/// Libera a imagem e o buffer da libwebp em qualquer saída.
struct Encoding {
    pic: sys::WebPPicture,
    writer: sys::WebPMemoryWriter,
}

impl Drop for Encoding {
    fn drop(&mut self) {
        // SAFETY: `pic` e `writer` foram inicializados pela libwebp em
        // `encode`; as duas funções aceitam estruturas vazias.
        unsafe {
            sys::WebPPictureFree(&mut self.pic);
            sys::WebPMemoryWriterClear(&mut self.writer);
        }
    }
}

fn encode(
    img: DynamicImage,
    rw: u32,
    rh: u32,
    quality: f32,
    method: fn(bool) -> i32,
) -> Result<Vec<u8>, WebpError> {
    let mut config = sys::WebPConfig::new().map_err(|_| WebpError::Encode)?;
    config.quality = quality;
    config.emulate_jpeg_size = 1; // -jpeg_like
    config.thread_level = 1; // -mt

    let width = i32::try_from(img.width()).map_err(|_| WebpError::TooLarge)?;
    let height = i32::try_from(img.height()).map_err(|_| WebpError::TooLarge)?;
    let mut enc = Encoding {
        pic: sys::WebPPicture::new().map_err(|_| WebpError::Encode)?,
        // SAFETY: estrutura de dados simples; `WebPMemoryWriterInit` a preenche.
        writer: unsafe { std::mem::zeroed() },
    };
    // SAFETY: `writer` é uma estrutura válida, inicializada aqui.
    unsafe { sys::WebPMemoryWriterInit(&mut enc.writer) };

    // O cwebp importa em ARGB quando há -resize.
    enc.pic.use_argb = i32::from(rw > 0 || rh > 0);
    enc.pic.width = width;
    enc.pic.height = height;
    let ok = if img.color().has_alpha() {
        let rgba = img.into_rgba8();
        let stride = width.checked_mul(4).ok_or(WebpError::TooLarge)?;
        // SAFETY: `rgba` tem `width * height * 4` bytes, com a largura de
        // linha `stride`, e vive até o fim da chamada (a libwebp copia).
        unsafe { sys::WebPPictureImportRGBA(&mut enc.pic, rgba.as_ptr(), stride) }
    } else {
        let rgb = img.into_rgb8();
        let stride = width.checked_mul(3).ok_or(WebpError::TooLarge)?;
        // SAFETY: idem, com 3 bytes por pixel.
        unsafe { sys::WebPPictureImportRGB(&mut enc.pic, rgb.as_ptr(), stride) }
    };
    if ok == 0 {
        return Err(WebpError::Encode);
    }

    if rw > 0 || rh > 0 {
        let rw = i32::try_from(rw).map_err(|_| WebpError::Param("max"))?;
        let rh = i32::try_from(rh).map_err(|_| WebpError::Param("max"))?;
        // SAFETY: `pic` contém a imagem importada acima.
        if unsafe { sys::WebPPictureRescale(&mut enc.pic, rw, rh) } == 0 {
            return Err(WebpError::Encode);
        }
    }

    // SAFETY: `pic` contém a imagem final (importada e redimensionada).
    let transparente = unsafe { sys::WebPPictureHasTransparency(&enc.pic) } != 0;
    config.method = method(transparente);
    // SAFETY: `config` foi inicializada por `WebPConfig::new`.
    if unsafe { sys::WebPValidateConfig(&config) } == 0 {
        return Err(WebpError::Encode);
    }

    enc.pic.writer = Some(sys::WebPMemoryWrite);
    enc.pic.custom_ptr = std::ptr::addr_of_mut!(enc.writer).cast();
    // SAFETY: `config` validada; `pic` com imagem e escritor em memória que
    // aponta para `enc.writer`, que não se move durante a chamada.
    if unsafe { sys::WebPEncode(&config, &mut enc.pic) } == 0 {
        return Err(WebpError::Encode);
    }
    if enc.writer.mem.is_null() || enc.writer.size == 0 {
        return Err(WebpError::Encode);
    }
    // SAFETY: a libwebp escreveu `size` bytes em `mem`.
    let out = unsafe { std::slice::from_raw_parts(enc.writer.mem, enc.writer.size) }.to_vec();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

    fn png(img: DynamicImage) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn webp_dims(bytes: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory_with_format(bytes, ImageFormat::WebP).unwrap();
        (img.width(), img.height())
    }

    fn gradiente(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }))
    }

    #[test]
    fn regra_de_tamanho_do_legado() {
        assert_eq!(resize_dims(800, 600, 400), (400, 0));
        assert_eq!(resize_dims(600, 800, 400), (0, 400));
        assert_eq!(resize_dims(500, 500, 400), (400, 400));
        assert_eq!(resize_dims(500, 300, 0), (0, 0));
    }

    #[test]
    fn lado_maior_vai_a_400_com_proporcao() {
        let out = to_webp(&png(gradiente(800, 600)), &WebpParams::default(), 1 << 30).unwrap();
        assert_eq!(&out[..4], b"RIFF");
        assert_eq!(&out[8..12], b"WEBP");
        assert_eq!(webp_dims(&out), (400, 300));

        let out = to_webp(&png(gradiente(300, 900)), &WebpParams::default(), 1 << 30).unwrap();
        // arredondamento do WebPPictureRescale: 300 * 400 / 900 = 133,3 -> 134
        assert_eq!(webp_dims(&out), (134, 400));
    }

    #[test]
    fn imagem_pequena_e_ampliada_como_no_cwebp() {
        let out = to_webp(&png(gradiente(100, 50)), &WebpParams::default(), 1 << 30).unwrap();
        assert_eq!(webp_dims(&out), (400, 200));
    }

    #[test]
    fn max_zero_mantem_o_tamanho() {
        let p = WebpParams {
            max_side: 0,
            ..WebpParams::default()
        };
        let out = to_webp(&png(gradiente(123, 45)), &p, 1 << 30).unwrap();
        assert_eq!(webp_dims(&out), (123, 45));
    }

    #[test]
    fn metodo_4_so_com_transparencia() {
        assert_eq!(method_for(false), 6);
        assert_eq!(method_for(true), 4);
    }

    #[test]
    fn transparente_usa_metodo_4_e_opaco_metodo_6() {
        let com_alfa = DynamicImage::ImageRgba8(RgbaImage::from_fn(48, 48, |x, y| {
            let d = (x as i32 - 24).pow(2) + (y as i32 - 24).pow(2);
            Rgba([
                (x * 5) as u8,
                (y * 5) as u8,
                200,
                if d < 400 { 255 } else { 0 },
            ])
        }));
        let gerar = |img: &DynamicImage, m: fn(bool) -> i32| {
            encode(img.clone(), 400, 400, 50.0, m).unwrap()
        };
        let saida = to_webp(&png(com_alfa.clone()), &WebpParams::default(), 1 << 30).unwrap();
        assert_eq!(saida, gerar(&com_alfa, |_| 4));
        assert_ne!(saida, gerar(&com_alfa, |_| 6));

        // RGBA com alfa todo 255 não tem transparência: segue o método 6.
        let opaco = DynamicImage::ImageRgba8(RgbaImage::from_fn(64, 64, |x, y| {
            let h = x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503);
            Rgba([(h >> 24) as u8, (h >> 16) as u8, (x * 4) as u8, 255])
        }));
        let saida = to_webp(&png(opaco.clone()), &WebpParams::default(), 1 << 30).unwrap();
        assert_eq!(saida, gerar(&opaco, |_| 6));
        assert_ne!(saida, gerar(&opaco, |_| 4));
    }

    #[test]
    fn transparencia_e_preservada() {
        let img = DynamicImage::ImageRgba8(RgbaImage::from_fn(64, 64, |x, _| {
            Rgba([200, 10, 10, if x < 32 { 0 } else { 255 }])
        }));
        let out = to_webp(&png(img), &WebpParams::default(), 1 << 30).unwrap();
        let back = image::load_from_memory_with_format(&out, ImageFormat::WebP).unwrap();
        assert!(back.color().has_alpha());
        assert_eq!(back.to_rgba8().get_pixel(10, 200)[3], 0);
        assert_eq!(back.to_rgba8().get_pixel(390, 200)[3], 255);
    }

    #[test]
    fn jpeg_e_webp_tambem_entram() {
        let mut jpg = Cursor::new(Vec::new());
        gradiente(320, 240)
            .write_to(&mut jpg, ImageFormat::Jpeg)
            .unwrap();
        let out = to_webp(&jpg.into_inner(), &WebpParams::default(), 1 << 30).unwrap();
        assert_eq!(webp_dims(&out), (400, 300));
        // WebP de entrada (o próprio resultado) volta a ser aceito.
        assert_eq!(
            webp_dims(&to_webp(&out, &WebpParams::default(), 1 << 30).unwrap()),
            (400, 300)
        );
    }

    #[test]
    fn recusas() {
        let p = WebpParams::default();
        assert_eq!(to_webp(b"", &p, 1 << 30), Err(WebpError::Empty));
        assert_eq!(
            to_webp(b"isto nao e imagem", &p, 1 << 30),
            Err(WebpError::Unsupported)
        );
        let mut corrompido = png(gradiente(50, 50));
        corrompido.truncate(60);
        assert!(matches!(
            to_webp(&corrompido, &p, 1 << 30),
            Err(WebpError::Invalid)
        ));
        assert_eq!(
            to_webp(&png(gradiente(100, 100)), &p, 9_999),
            Err(WebpError::TooLarge)
        );
        let fora = WebpParams {
            max_side: MAX_SIDE_LIMIT + 1,
            ..p
        };
        assert_eq!(
            to_webp(&png(gradiente(10, 10)), &fora, 1 << 30),
            Err(WebpError::Param("max"))
        );
        let q = WebpParams {
            quality: 101.0,
            ..p
        };
        assert_eq!(
            to_webp(&png(gradiente(10, 10)), &q, 1 << 30),
            Err(WebpError::Param("q"))
        );
    }
}
