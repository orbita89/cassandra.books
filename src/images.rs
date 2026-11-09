//! Собственная оптимизация картинок внутри EPUB.
//!
//! Что делает:
//!   * достаёт из EPUB **все** растровые картинки (jpeg/png/gif/bmp/tiff/webp/avif);
//!   * уменьшает каждую — сначала до вписывания в `max_w`x`max_h`, а затем ещё
//!     на `shrink_percent` процентов (по умолчанию 45 %) — то есть даже картинки,
//!     которые и так меньше лимита, всё равно уменьшаются;
//!   * перебирает несколько вариантов и берёт самый маленький по байтам:
//!     WebP lossy (quality 35, а для мелких картинок — 50), WebP near-lossless
//!     (для png/gif/bmp/tiff, `near_lossless=60`) и AVIF lossy (через `ravif`,
//!     чистый Rust, без внешних процессов);
//!   * картинки в градациях серого кодирует одним каналом, а не тремя;
//!   * обложку всегда кодирует в AVIF, quality 50;
//!   * переименовывает файлы и правит ссылки в OPF/XHTML/CSS/NCX, а также
//!     `href` и `media-type` в `content.opf` (точечно, по `<item>`);
//!   * минифицирует XHTML/CSS и чистит метаданные Calibre в OPF;
//!   * ужимает встроенные `.ttf`/`.otf` до символов книги (HarfBuzz);
//!   * выбрасывает `toc.ncx` вместе с `<item>` в OPF и `spine toc="…"`;
//!   * собирает EPUB заново, соблюдая требование OCF: `mimetype` первым и без
//!     сжатия, а остальные записи — Stored или Deflate (`pack_epub`).
//!
//! Последние три шага и замер их эффекта лежат здесь же: `prepare_entries`
//! (шаги 1–3), `pack_epub` (сборка) и `benchmark_epub` (сколько байт даёт
//! каждая стадия — это режим `--bench` в `main.rs`).
//!
//! Вариант «в исходном размере» для lossless тоже пробуется: Lanczos3 сглаживает
//! штриховую графику, и lossless после ресайза раздувается, так что исходный
//! вариант нередко выигрывает.

use anyhow::{anyhow, bail, Context, Result};
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use libwebp_sys::*;
use std::collections::HashSet;
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// Диагностика: сколько раз реально вызывался AVIF-энкодер и сколько это заняло.
///
/// Нужна, чтобы видеть работу «лестницы»: для графики кандидат кодируется,
/// замеряется штрих, и при неудаче берётся следующая ступень — то есть на одну
/// картинку может уйти несколько полных кодирований.
pub static AVIF_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Наносекунды внутри AVIF-энкодера (только сам `ravif`, без декодирования).
pub static AVIF_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Наносекунды на декодирование исходной картинки.
pub static DECODE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Наносекунды на анализ «графика или фотография».
pub static FEATURES_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Наносекунды на уменьшение (Lanczos3) и подчёркивание краёв.
pub static RESIZE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Наносекунды на измерение штриха уже закодированного кадра.
pub static STROKE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Наносекунды внутри WebP-энкодера.
pub static WEBP_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Сколько раз вызывался WebP-энкодер.
pub static WEBP_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Прибавить к счётчику прошедшее с `t` время.
fn tally_ns(slot: &std::sync::atomic::AtomicU64, t: std::time::Instant) {
    slot.fetch_add(
        t.elapsed().as_nanos() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Растровые форматы, которые мы обязаны перевести в WebP или AVIF.
pub const RASTER_EXTS: [&str; 9] = [
    "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "avif",
];

/// Форматы, для которых имеет смысл пробовать lossless WebP
/// (для JPEG он практически всегда проигрывает).
const LOSSLESS_SOURCE_EXTS: [&str; 6] = ["png", "gif", "bmp", "tif", "tiff", "webp"];

/// Текстовые ресурсы EPUB, в которых могут быть ссылки на картинки.
const TEXT_EXTS: [&str; 9] = [
    "xhtml", "html", "htm", "opf", "ncx", "css", "xml", "smil", "svg",
];

/// MIME-типы растровых картинок, которые надо заменить на современный формат.
const RASTER_MIME_TYPES: [&str; 10] = [
    "image/jpeg",
    "image/jpg",
    "image/pjpeg",
    "image/png",
    "image/x-png",
    "image/gif",
    "image/bmp",
    "image/tiff",
    "image/x-tiff",
    "image/avif",
];

#[derive(Debug, Clone)]
pub struct ImageOptConfig {
    /// Жёсткий предел по ширине.
    pub max_w: u32,
    /// Жёсткий предел по высоте.
    pub max_h: u32,
    /// Дополнительное уменьшение в процентах, применяется ко **всем** картинкам.
    pub shrink_percent: f64,
    /// Качество WebP (lossy), 0..100.
    pub quality: f32,
    /// `method` libwebp: 0 (быстро) .. 6 (максимальное сжатие).
    pub method: i32,
    /// Не опускать длинную сторону ниже этого значения (защита иконок).
    pub min_side: u32,
    /// Пробовать lossless WebP для png/gif/bmp/tiff, если он меньше lossy.
    pub try_lossless: bool,
    /// Пробовать и вариант «в исходном размере» при уменьшении.
    /// Выигрыш ~0.1 %, но стоит ещё одного кодирования на картинку.
    pub try_original_size: bool,
    // --- «умное» сжатие ---------------------------------------------------
    /// Картинка считается мелкой, если её ширина или высота меньше этого.
    pub small_px: u32,
    /// Качество для мелких картинок: их не так жалко, но артефакты видны сильнее.
    pub quality_small: f32,
    /// Кодировать ли AVIF (кодировщик `ravif`, чистый Rust, без внешних процессов).
    pub avif: bool,
    /// Скорость rav1e для фотографий, 1..10: 1 — медленно и мелко,
    /// 10 — быстро и крупнее. Именно она определяет время импорта: на
    /// реальном корпусе кодирование картинок — это ~99 % работы.
    pub avif_speed: u8,
    /// Скорость rav1e для графики. Ниже, чем у фотографий: на штрихах и
    /// буквах быстрое кодирование видно сразу.
    pub avif_speed_graphic: u8,
    /// Скорость rav1e для обложек. Обложка одна на книгу, но кодируется
    /// всегда в AVIF — на скорости 2 она стоила больше, чем весь остальной
    /// импорт (замер: 4.1 с на книгу).
    pub avif_speed_cover: u8,
    /// `sns_strength` libwebp: 0..100, по умолчанию 50.
    pub sns_strength: i32,
    /// `filter_strength` libwebp: 0..100, по умолчанию 60.
    pub filter_strength: i32,
    /// Сколько проходов анализа энтропии делает libwebp (1..10).
    /// Больше — мельче файл, но дольше; при `1` этот резерв не используется.
    pub webp_pass: i32,
    /// Разрешить libwebp самому подобрать силу фильтра (`autofilter`).
    pub webp_autofilter: bool,
    /// Пробовать near-lossless WebP для png/gif/bmp/tiff.
    pub near_lossless: bool,
    /// `near_lossless` libwebp: 0 — выключено, 100 — максимум.
    pub near_lossless_quality: i32,
    /// Качество lossless-ветки (для `lossless=1`).
    pub lossless_quality: f32,
    /// Отличать «графику» (текст, схемы, чертежи) от фотографий и сжимать её
    /// бережнее: у графики после уменьшения слипаются штрихи.
    pub text_detect: bool,
    /// Какая толщина штриха должна остаться после уменьшения, в пикселях.
    /// 2.0 — штрих уверенно различим, 1.5 — на грани, 1.0 — уже серая каша.
    pub text_min_stroke: f64,
    /// Дополнительное уменьшение графики в процентах (обычно мягче общего).
    pub text_shrink_percent: f64,
    /// Качество графики: выше общего, чтобы штрихи не «звенели» артефактами.
    pub text_quality: f32,
    /// Качество обложки (только AVIF).
    pub cover_quality: f32,
    /// Длинная сторона обложки и внутри EPUB, и в `data/covers/`.
    // --- шаги 1-3: текст, шрифты, ZIP-контейнер ----------------------------
    /// Минифицировать XHTML/CSS: убрать комментарии и лишние пробелы.
    pub minify: bool,
    /// Удалять из OPF метаданные Calibre (`calibre:timestamp`, `calibre:series`…).
    pub strip_calibre_meta: bool,
    /// Удалять устаревший `toc.ncx` (EPUB 3) и ссылки на него в OPF.
    pub drop_ncx: bool,
    /// Ужимать встроенные шрифты до символов, которые реально есть в книге.
    pub subset_fonts: bool,
    /// Уровень Deflate для записей EPUB (1..9).
    pub zip_level: u8,
    /// Не сжимать записи EPUB: тогда батч целиком сожмёт `.arx` (zstd),
    /// и это на ~20 % меньше, чем Deflate внутри каждого EPUB по отдельности.
    pub epub_stored: bool,
}

impl ImageOptConfig {
    /// Настройки из переменных окружения — для подбора режима без пересборки.
    ///
    /// Читаются только те переменные, что заданы; остальное берётся из
    /// `Default`. Нужны, чтобы один и тот же бинарник можно было прогнать по
    /// реальному архиву в нескольких режимах и сравнить время и размер.
    ///
    /// * `LIBRARY_SHRINK` — дополнительное уменьшение, процентов;
    /// * `LIBRARY_MAX_W`, `LIBRARY_MAX_H` — предел вписывания;
    /// * `LIBRARY_QUALITY`, `LIBRARY_TEXT_QUALITY`, `LIBRARY_COVER_QUALITY`;
    /// * `LIBRARY_AVIF_SPEED`, `LIBRARY_AVIF_SPEED_GRAPHIC` — скорость rav1e;
    /// * `LIBRARY_WEBP=1` — отказаться от AVIF в пользу WebP;
    /// * `LIBRARY_WEBP_METHOD` — метод libwebp (0..6).
    pub fn from_env() -> Self {
        let mut c = Self::default();
        let num = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<f64>().ok());
        let int = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok());
        if let Some(v) = num("LIBRARY_SHRINK") {
            c.shrink_percent = v;
        }
        if let Some(v) = int("LIBRARY_MAX_W") {
            c.max_w = v;
        }
        if let Some(v) = int("LIBRARY_MAX_H") {
            c.max_h = v;
        }
        if let Some(v) = num("LIBRARY_QUALITY") {
            c.quality = v as f32;
        }
        if let Some(v) = num("LIBRARY_TEXT_QUALITY") {
            c.text_quality = v as f32;
        }
        if let Some(v) = num("LIBRARY_COVER_QUALITY") {
            c.cover_quality = v as f32;
        }
        if let Some(v) = int("LIBRARY_AVIF_SPEED") {
            c.avif_speed = v.min(10.0 as u32) as u8;
        }
        if let Some(v) = int("LIBRARY_AVIF_SPEED_GRAPHIC") {
            c.avif_speed_graphic = v.min(10) as u8;
        }
        if let Some(v) = int("LIBRARY_AVIF_SPEED_COVER") {
            c.avif_speed_cover = v.min(10) as u8;
        }
        if std::env::var_os("LIBRARY_WEBP").is_some() {
            c.avif = false;
        }
        if std::env::var_os("LIBRARY_AVIF").is_some() {
            c.avif = true;
        }
        if let Some(v) = int("LIBRARY_WEBP_METHOD") {
            c.method = v as i32;
        }
        if let Some(v) = int("LIBRARY_WEBP_PASS") {
            c.webp_pass = v.clamp(1, 10) as i32;
        }
        if std::env::var_os("LIBRARY_WEBP_AUTOFILTER").is_some() {
            c.webp_autofilter = true;
        }
        if std::env::var_os("LIBRARY_NO_LOSSLESS").is_some() {
            c.try_lossless = false;
        }
        c
    }
}

impl Default for ImageOptConfig {
    fn default() -> Self {
        Self {
            // Формат по умолчанию — AVIF: место важнее времени, и импорт
            // делается один раз. Замер на реальном корпусе (92 книги, 157 МБ):
            //   WebP m4 q40, 480x720  →  18 с, архив 38.10 МБ
            //   AVIF q35,   480x720  → 246 с, архив 34.58 МБ  (−9.2 %)
            // Скорость rav1e почти ничего не даёт: speed 1 против speed 2 —
            // это −0.4 % размера за 3.8x времени, поэтому стоит speed 2.
            max_w: 480,
            max_h: 720,
            shrink_percent: 55.0,
            quality: 35.0,
            method: 4,
            min_side: 240,
            try_lossless: true,
            try_original_size: false,
            small_px: 300,
            quality_small: 48.0,
            avif: true,
            avif_speed: 2,
            avif_speed_graphic: 2,
            // Обложка кодируется тем же форматом и с той же скоростью, что и
            // остальные картинки: раньше она всегда шла в AVIF на скорости 2
            // и съедала 46 % всей работы над картинками.
            avif_speed_cover: 2,
            sns_strength: 80,
            filter_strength: 70,
            webp_pass: 1,
            webp_autofilter: false,
            near_lossless: true,
            near_lossless_quality: 60,
            lossless_quality: 75.0,
            text_detect: true,
            text_min_stroke: 2.0,
            text_shrink_percent: 10.0,
            text_quality: 50.0,
            cover_quality: 55.0,
            minify: true,
            strip_calibre_meta: true,
            drop_ncx: true,
            subset_fonts: true,
            zip_level: 9,
            epub_stored: true,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ImageOptStats {
    /// Сколько растровых картинок было в EPUB.
    pub images: usize,
    /// Сколько картинок удалось уменьшить.
    pub resized: usize,
    /// Сколько картинок выбрало lossless-режим WebP. Сюда же входит
    /// near-lossless: он тоже кодируется с `lossless = 1`, только с потерями
    /// в цвете (`near_lossless = 60`), и именно он выигрывает на графике.
    pub lossless: usize,
    /// Сколько картинок выбрало near-lossless WebP.
    pub near_lossless: usize,
    /// Сколько картинок осталось в WebP.
    pub webp: usize,
    /// Сколько картинок стало AVIF.
    pub avif: usize,
    /// Сколько картинок не удалось декодировать (остаются как есть).
    pub failed: usize,
    /// Суммарный размер текстовых файлов (XHTML/CSS/OPF/NCX) до минификации.
    pub text_before: u64,
    /// Он же после минификации.
    pub text_after: u64,
    /// Суммарный размер встроенных шрифтов до и после subsetting.
    pub fonts_before: u64,
    pub fonts_after: u64,
    /// Сколько шрифтов удалось уменьшить.
    pub fonts_subset: usize,
    /// Сколько записей удалено из контейнера (`toc.ncx` и т.п.).
    pub entries_dropped: usize,
    /// Суммарный размер картинок до оптимизации.
    pub bytes_before: u64,
    /// Суммарный размер картинок после оптимизации.
    pub bytes_after: u64,
    /// Максимальная ширина/высота до и после.
    pub max_before: (u32, u32),
    pub max_after: (u32, u32),
}

// ---------------------------------------------------------------------------
// Кодирование в WebP через libwebp
// ---------------------------------------------------------------------------

/// Закодировать RGBA-буфер в WebP.
/// Параметры кодирования в WebP.
///
/// `quality` и `method` — то же, что у libwebp; `near_lossless` действует только
/// при `lossless = 1`; `sns_strength` / `filter_strength` — сглаживание
/// и деблокинг-фильтр, по умолчанию 50 и 60.
#[derive(Clone, Copy, Debug)]
pub struct WebpParams {
    pub quality: f32,
    pub method: i32,
    pub lossless: bool,
    pub near_lossless: i32,
    pub sns_strength: i32,
    pub filter_strength: i32,
    /// Проходов анализа энтропии: больше — мельче и дольше.
    pub pass: i32,
    /// Подбирать силу фильтра автоматически.
    pub autofilter: bool,
}

impl WebpParams {
    /// Обычный lossy WebP.
    pub fn lossy(
        quality: f32,
        method: i32,
        sns_strength: i32,
        filter_strength: i32,
        pass: i32,
        autofilter: bool,
    ) -> Self {
        Self {
            quality,
            method,
            lossless: false,
            near_lossless: 0,
            sns_strength,
            filter_strength,
            pass,
            autofilter,
        }
    }

    /// Lossless с near-lossless-предобработкой (для png/gif/bmp/tiff).
    pub fn near_lossless(quality: f32, method: i32, near: i32) -> Self {
        Self {
            quality,
            method,
            lossless: true,
            near_lossless: near,
            sns_strength: 50,
            filter_strength: 60,
            pass: 1,
            autofilter: false,
        }
    }
}

/// Закодировать буфер в WebP.
///
/// `rgb = true` — буфер трёхканальный (RGB, шаг `w * 3`), `false` — четырёхканальный
/// (RGBA, шаг `w * 4`). Одноканальный (градации серого) libwebp не принимает,
/// поэтому серые картинки раскладываются в три одинаковых канала.
unsafe fn webp_encode(
    pixels: &[u8],
    w: u32,
    h: u32,
    p: &WebpParams,
    rgb: bool,
) -> Result<Vec<u8>> {
    let mut config: WebPConfig = std::mem::zeroed();
    if WebPConfigInitInternal(
        &mut config,
        WebPPreset::WEBP_PRESET_DEFAULT,
        p.quality,
        WEBP_ENCODER_ABI_VERSION as i32,
    ) == 0
    {
        bail!("WebPConfigInitInternal завершился с ошибкой");
    }
    config.lossless = i32::from(p.lossless);
    config.method = p.method;
    config.quality = p.quality;
    config.near_lossless = p.near_lossless;
    config.sns_strength = p.sns_strength;
    config.filter_strength = p.filter_strength;
    config.pass = p.pass;
    config.autofilter = i32::from(p.autofilter);
    if WebPValidateConfig(&config) == 0 {
        bail!(
            "libwebp отверг конфигурацию (quality={}, method={}, lossless={}, near_lossless={})",
            p.quality,
            p.method,
            p.lossless,
            p.near_lossless
        );
    }

    let mut pic: WebPPicture = std::mem::zeroed();
    if WebPPictureInitInternal(&mut pic, WEBP_ENCODER_ABI_VERSION as i32) == 0 {
        bail!("WebPPictureInitInternal завершился с ошибкой");
    }
    pic.use_argb = 1;
    pic.width = w as i32;
    pic.height = h as i32;
    let imported = if rgb {
        WebPPictureImportRGB(&mut pic, pixels.as_ptr(), (w as i32) * 3)
    } else {
        WebPPictureImportRGBA(&mut pic, pixels.as_ptr(), (w as i32) * 4)
    };
    if imported == 0 {
        let code = pic.error_code;
        WebPPictureFree(&mut pic);
        bail!("WebPPictureImport{} завершился с ошибкой ({code:?})", if rgb { "RGB" } else { "RGBA" });
    }

    let mut writer: WebPMemoryWriter = std::mem::zeroed();
    WebPMemoryWriterInit(&mut writer);
    pic.writer = Some(WebPMemoryWrite);
    pic.custom_ptr = &mut writer as *mut _ as *mut c_void;

    let ok = WebPEncode(&config, &mut pic);
    let out = if ok != 0 && !writer.mem.is_null() && writer.size > 0 {
        std::slice::from_raw_parts(writer.mem, writer.size).to_vec()
    } else {
        Vec::new()
    };

    WebPPictureFree(&mut pic);
    WebPMemoryWriterClear(&mut writer);

    if out.is_empty() {
        bail!("WebPEncode не вернул данных");
    }
    Ok(out)
}

/// Закодировать RGBA-буфер в WebP.
///
/// # Safety
/// `rgba` должен содержать ровно `w * h * 4` байт.
pub fn encode_webp(rgba: &[u8], w: u32, h: u32, p: &WebpParams) -> Result<Vec<u8>> {
    WEBP_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: буфер соответствует w*h*4 байт, libwebp копирует данные в WebPPicture.
    unsafe { webp_encode(rgba, w, h, p, false) }
}

/// Закодировать RGB-буфер (3 канала) в WebP.
///
/// # Safety
/// `rgb` должен содержать ровно `w * h * 3` байт.
pub fn encode_webp_rgb(rgb: &[u8], w: u32, h: u32, p: &WebpParams) -> Result<Vec<u8>> {
    WEBP_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: буфер соответствует w*h*3 байт, libwebp копирует данные в WebPPicture.
    unsafe { webp_encode(rgb, w, h, p, true) }
}

// ---------------------------------------------------------------------------
// Кодирование в AVIF (крейт `ravif`, чистый Rust, без внешних процессов)
// ---------------------------------------------------------------------------

/// Закодировать буфер в AVIF.
///
/// `color` — настоящий тип данных: `L8` (градации серого), `Rgb8` (без альфы)
/// или `Rgba8`. `quality` — 1..100, `speed` — 1..10 (8 = разумный компромисс).
///
/// Кодирование однопоточное: внешний `rayon` уже разложил картинки по ядрам,
/// поэтому вложенный пул rav1e только мешал бы.
pub fn encode_avif(
    data: &[u8],
    w: u32,
    h: u32,
    color: ExtendedColorType,
    quality: f32,
    speed: u8,
) -> Result<Vec<u8>> {
    let _t = std::time::Instant::now();
    AVIF_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut buf = Vec::new();
    let q = quality.clamp(1.0, 100.0).round() as u8;
    let enc = image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut buf, speed, q)
        .with_num_threads(Some(1));
    let outcome = enc
        .write_image(data, w, h, color)
        .map_err(|e| anyhow!("AVIF-кодирование не удалось: {e}"));
    AVIF_NS.fetch_add(
        _t.elapsed().as_nanos() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    outcome?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Графика или фотография
// ---------------------------------------------------------------------------

/// «Графика» — почти серое изображение со светлым фоном и множеством тонких
/// штрихов. Пороги подобраны по корпусу: 34 % картинок, медианный штрих 2 px.
pub fn is_graphic_features(per_1k: f64, paper: f64, sat: f64) -> bool {
    sat < 30.0 && per_1k > 15.0 && paper > 0.3
}

/// Признаки «графики»: доля светлого фона, плотность тонких штрихов и
/// медианная толщина штриха. Прозрачность раскладывается на белый фон —
/// иначе RGB под альфой равен нулю и картинка выглядит чёрным квадратом.
///
/// Возвращает `(штрихов на 1000 пикселей, медианная толщина штриха, доля фона, насыщенность)`.
pub fn graphic_features(img: &RgbaImage) -> (f64, f64, f64, f64) {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let step = ((w.max(h) / 512).max(1)) as usize;
    let mut paper = 0u32;
    let mut sat = 0u32;
    let mut tot = 0u32;
    let mut runs: Vec<u32> = Vec::new();
    for y in (0..h).step_by(step) {
        let mut run = 0u32;
        for x in (0..w).step_by(step) {
            let p = img.get_pixel(x, y);
            let a = p[3] as u32;
            let mix = |c: u8| -> u32 { (c as u32 * a + 255 * (255 - a)) / 255 };
            let l = (mix(p[0]) * 299 + mix(p[1]) * 587 + mix(p[2]) * 114) / 1000;
            if l > 200 {
                paper += 1;
            }
            sat += p[0].max(p[1]).max(p[2]) as u32 - p[0].min(p[1]).min(p[2]) as u32;
            if l < 128 {
                run += 1;
            } else if run > 0 {
                if run <= 32 {
                    runs.push(run);
                }
                run = 0;
            }
            tot += 1;
        }
        if run > 0 && run <= 32 {
            runs.push(run);
        }
    }
    runs.sort_unstable();
    let t = tot.max(1) as f64;
    let median = if runs.is_empty() {
        0.0
    } else {
        runs[runs.len() / 2] as f64
    };
    (
        runs.len() as f64 / t * 1000.0,
        median,
        paper as f64 / t,
        sat as f64 / t,
    )
}

// ---------------------------------------------------------------------------
// Расчёт целевых размеров
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Вспомогательные функции для имён и текста
// ---------------------------------------------------------------------------

fn ext_lower(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

fn strip_ext(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

fn is_raster(name: &str) -> bool {
    RASTER_EXTS.contains(&ext_lower(name).as_str())
}

/// Растровая картинка по «магическим» байтам.
///
/// Нужна для файлов **без расширения**: Calibre умеет положить обложку
/// в EPUB записью с именем просто `cover` (внутри обычный JPEG), и по имени
/// её не узнать. Именно из-за этого у книг «Лесной дозор 4», «Новый каменный
/// век» не находилась обложка, а требование «100 % WebP» для них молча
/// нарушалось — JPEG так и оставался в EPUB.
fn looks_like_raster(data: &[u8]) -> bool {
    sniff_image(data).is_some()
}

/// Настоящий формат картинки по содержимому: `(расширение, MIME-тип)`.
///
/// Формат берётся из байтов, а не из `content-type` и не из имени файла:
/// в реальных FB2 `<binary content-type="image/png">` сплошь и рядом лежит
/// JPEG, и на слово верить нельзя. Из-за этого epubcheck ругался
/// `OPF-029: declared as image/png but its actual format is image/jpeg`.
pub fn sniff_image(data: &[u8]) -> Option<(&'static str, &'static str)> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(("jpg", "image/jpeg"));
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("png", "image/png"));
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some(("gif", "image/gif"));
    }
    if data.starts_with(b"BM") {
        return Some(("bmp", "image/bmp"));
    }
    if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        return Some(("tif", "image/tiff"));
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some(("webp", "image/webp"));
    }
    // AVIF — ISO-BMFF: `ftyp` в байтах 4..8, бренд `avif`/`avis` (или `mif1`
    // с совместимым брендом) сразу за ним.
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        let head = &data[..data.len().min(64)];
        if head.windows(4).any(|w| w == b"avif") {
            return Some(("avif", "image/avif"));
        }
    }
    // SVG — текст, поэтому ищем корневой элемент в начале файла.
    if let Ok(head) = std::str::from_utf8(&data[..data.len().min(1024)]) {
        let head = head.trim_start();
        if head.starts_with("<svg") || (head.starts_with("<?xml") && head.contains("<svg")) {
            return Some(("svg", "image/svg+xml"));
        }
    }
    None
}

/// Картинка ли это: сначала по расширению, потом по содержимому.
fn is_raster_entry(name: &str, data: &[u8]) -> bool {
    is_raster(name) || looks_like_raster(data)
}

/// Все ли пиксели серые (`R == G == B`).
///
/// Проверка по пикселям, а не по `ColorType`: у JPEG-исходника тип `Rgb8`
/// даже тогда, когда картинка на самом деле чёрно-белая, и `has_color()`
/// в одиночку такой случай не поймает.
///
/// В горячем пути её заменил `image_kind`: он отдаёт и серость, и
/// прозрачность за один проход. Здесь она остаётся для тестов.
#[cfg(test)]
fn is_grayscale(img: &RgbaImage) -> bool {
    img.pixels().all(|p| p[0] == p[1] && p[1] == p[2])
}

/// Один проход по кадру: `(серый, непрозрачный)`.
///
/// `is_grayscale` + `is_opaque` сканировали пиксели дважды. Для лестницы AVIF
/// это было до 8 проходов на кадр (2 проверки × до 4 кандидатов).
fn image_kind(img: &RgbaImage) -> (bool, bool) {
    let mut gray = true;
    let mut opaque = true;
    for p in img.pixels() {
        if p[0] != p[1] || p[1] != p[2] {
            gray = false;
        }
        if p[3] != 255 {
            opaque = false;
        }
        if !gray && !opaque {
            return (false, false);
        }
    }
    (gray, opaque)
}

/// Полностью ли непрозрачная картинка (альфа 255 везде).
fn is_opaque(img: &RgbaImage) -> bool {
    img.pixels().all(|p| p[3] == 255)
}

/// Серые пиксели в одноканальный буфер (для AVIF `L8`).
fn gray_to_luma(img: &RgbaImage) -> Vec<u8> {
    img.pixels().map(|p| p[0]).collect()
}

/// RGBA-пиксели в трёхканальный RGB-буфер (альфа отбрасывается).
fn rgba_to_rgb(img: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.width() as usize * img.height() as usize * 3);
    for p in img.pixels() {
        out.extend_from_slice(&[p[0], p[1], p[2]]);
    }
    out
}

/// Ссылка ли это на ресурс в этом месте текста.
///
/// Замена имени картинки «в лоб» ломает OPF: у обложки без расширения
/// basename — просто `cover`, и он встречается ещё в `name="cover"` и
/// `type="cover"`. Поэтому меняем `old` только внутри значений `href=`,
/// `src=`, `poster=` и `url(...)`.
fn is_ref_position(text: &str, at: usize) -> bool {
    let bytes = text.as_bytes();
    let mut i = at;
    let stop = at.saturating_sub(1024);
    while i > stop {
        match bytes[i - 1] {
            b'"' | b'\'' | b'(' => {
                let head = text[..i - 1].trim_end();
                return head.ends_with("href=")
                    || head.ends_with("src=")
                    || head.ends_with("poster=")
                    || head.ends_with("url(")
                    || head.ends_with("href =")
                    || head.ends_with("src =")
                    || head.ends_with("poster =")
                    || head.ends_with("url (");
            }
            b'<' | b'>' => return false,
            _ => i -= 1,
        }
    }
    false
}

/// Переписать ссылки на `old` → `new`, не задевая посторонние вхождения.
fn replace_refs(text: &str, old: &str, new: &str) -> (String, bool) {
    if old.is_empty() || old == new {
        return (text.to_string(), false);
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    let mut changed = false;
    while let Some(p) = text[pos..].find(old) {
        let at = pos + p;
        out.push_str(&text[pos..at]);
        if is_ref_position(text, at) {
            out.push_str(new);
            changed = true;
        } else {
            out.push_str(old);
        }
        pos = at + old.len();
    }
    out.push_str(&text[pos..]);
    (out, changed)
}

// ---------------------------------------------------------------------------
// Шаг 1. Минификация XHTML/CSS и чистка OPF
// ---------------------------------------------------------------------------

/// Теги, между которыми пробел значим: перед ними и после них оставляем
/// один пробел, иначе «слово</em> <em>слово» слипнется.
const INLINE_TAGS: [&str; 22] = [
    "a", "abbr", "b", "big", "br", "cite", "code", "dfn", "em", "i", "img", "kbd", "q", "s", "samp",
    "small", "span", "strong", "sub", "sup", "u", "var",
];

fn is_inline_tag(name: &str) -> bool {
    INLINE_TAGS.contains(&name)
}

/// Имя тега из строки вида `<p ...>`, `</p>`, `<br/>`.
fn tag_name(tag: &str) -> &str {
    let t = tag
        .strip_prefix("</")
        .or_else(|| tag.strip_prefix('<'))
        .unwrap_or(tag);
    let end = t
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(t.len());
    &t[..end]
}

/// Индекс сразу за `>` тега, начиная с `<` в позиции `start`.
///
/// Кавычки внутри атрибутов учитываются: `alt="a > b"` не закрывает тег.
fn find_tag_end(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start + 1;
    let mut quote = 0u8;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' if quote == 0 => quote = bytes[i],
            b'"' | b'\'' if quote == bytes[i] => quote = 0,
            b'>' if quote == 0 => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

fn next_tag_name(s: &str, at: usize) -> Option<String> {
    let rest = s.get(at..)?;
    let end = find_tag_end(rest, 0)?;
    Some(tag_name(&rest[..end]).to_ascii_lowercase())
}

/// Минифицировать XHTML: убрать комментарии и лишние пробелы.
///
/// Правила консервативные, чтобы файл остался well-formed XML (иначе
/// `epubcheck` ругается `RSC-005`):
///   * комментарии `<!-- ... -->` удаляются;
///   * пробельные последовательности внутри текста сжимаются в один пробел;
///   * пробел между тегами выбрасывается, если хотя бы один из соседей —
///     блочный тег; между двумя строчными он сохраняется;
///   * содержимое `<pre>` не трогается вообще.
pub fn minify_xhtml(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    let mut pre_depth = 0usize;
    let mut prev_tag: Option<String> = None;

    while i < src.len() {
        if src.as_bytes()[i] == b'<' {
            // комментарии
            if src[i..].starts_with("<!--") {
                match src[i + 4..].find("-->") {
                    Some(end) => {
                        i += 4 + end + 3;
                        continue;
                    }
                    None => break,
                }
            }
            // CDATA и processing instructions копируются как есть
            if src[i..].starts_with("<![CDATA[") {
                match src[i + 9..].find("]]>") {
                    Some(end) => {
                        let stop = i + 9 + end + 3;
                        out.push_str(&src[i..stop]);
                        i = stop;
                        continue;
                    }
                    None => break,
                }
            }
            let Some(end) = find_tag_end(src, i) else {
                out.push_str(&src[i..]);
                break;
            };
            let tag = &src[i..end];
            out.push_str(tag);

            let name = tag_name(tag).to_ascii_lowercase();
            let closing = tag.starts_with("</");
            let self_closing = tag.ends_with("/>");
            if name == "pre" {
                if closing {
                    pre_depth = pre_depth.saturating_sub(1);
                } else if !self_closing {
                    pre_depth += 1;
                }
            }
            if !self_closing {
                prev_tag = Some(name);
            }
            i = end;
            continue;
        }

        let end = src[i..].find('<').map(|p| i + p).unwrap_or(src.len());
        let text = &src[i..end];
        if pre_depth > 0 {
            out.push_str(text);
        } else if text.trim().is_empty() {
            // Пробел между двумя строчными тегами значим, между блочными — нет.
            let next = next_tag_name(src, end);
            let keep = match (prev_tag.as_deref(), next.as_deref()) {
                (Some(a), Some(b)) => is_inline_tag(a) && is_inline_tag(b),
                _ => false,
            };
            if keep {
                out.push(' ');
            }
        } else {
            let mut prev_space = false;
            for ch in text.chars() {
                if ch.is_whitespace() {
                    if !prev_space {
                        out.push(' ');
                        prev_space = true;
                    }
                } else {
                    out.push(ch);
                    prev_space = false;
                }
            }
        }
        i = end;
    }
    out
}

/// Минифицировать CSS: убрать комментарии, сжать пробелы, снять `;` перед `}`.
///
/// Строки (`content: "…"`) и `url(…)` не трогаются: внутри них пробелы значимы.
pub fn minify_css(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut quote: Option<char> = None;
    let mut pending_space = false;
    // После `{`, `;`, `:`, `,` и т.п. пробел не нужен вообще: он бы вернулся
    // из `pending_space` на следующем символе.
    let mut suppress_space = false;

    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            if pending_space && !suppress_space && !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            pending_space = false;
            quote = Some(c);
            out.push(c);
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(d) = chars.next() {
                if d == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            pending_space = true;
            continue;
        }
        if c.is_whitespace() {
            if !suppress_space {
                pending_space = true;
            }
            continue;
        }
        if matches!(c, '{' | '}' | ':' | ';' | ',' | '>' | '+' | '~' | '(' | ')') {
            while out.ends_with(' ') {
                out.pop();
            }
            out.push(c);
            pending_space = false;
            suppress_space = true;
            continue;
        }
        if pending_space && !suppress_space && !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        pending_space = false;
        suppress_space = false;
        out.push(c);
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out.replace(";}", "}")
}

/// Удалить из OPF метаданные Calibre: они занимают место и читалкам не нужны.
pub fn strip_calibre_metadata(opf: &str) -> String {
    let mut out = String::with_capacity(opf.len());
    let mut rest = opf;
    while let Some(p) = rest.find("<meta") {
        let (before, from) = rest.split_at(p);
        out.push_str(before);
        let Some(end) = from.find('>') else {
            out.push_str(from);
            return out;
        };
        let tag = &from[..=end];
        let lower = tag.to_ascii_lowercase();
        let calibre = lower.contains("calibre")
            || (lower.contains("name=\"generator\"") && lower.contains("calibre"));
        if calibre {
            // `<meta ...>текст</meta>` — если тег не самозакрывающийся, съесть и тело
            if !tag.ends_with("/>") {
                if let Some(close) = from[end + 1..].find("</meta>") {
                    rest = &from[end + 1 + close + 7..];
                    continue;
                }
            }
            rest = &from[end + 1..];
            continue;
        }
        out.push_str(tag);
        rest = &from[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Удалить `<item>` с указанным basename из OPF.
fn remove_opf_item(text: &str, base: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find("<item") else {
            out.push_str(rest);
            return out;
        };
        let (before, from_item) = rest.split_at(start);
        out.push_str(before);
        let after = &from_item[5..];
        if !after.starts_with([' ', '\n', '\r', '\t']) {
            out.push_str("<item");
            rest = after;
            continue;
        }
        let Some(end) = from_item.find('>') else {
            out.push_str(from_item);
            return out;
        };
        let (item, tail) = from_item.split_at(end + 1);
        let hit = item_attr(item, "href")
            .map(|h| basename(&h) == base)
            .unwrap_or(false);
        if !hit {
            out.push_str(item);
        }
        rest = tail;
    }
}

/// Убрать `toc="…"` из `<spine>` (ссылка на удалённый `toc.ncx`).
fn remove_spine_toc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find("<spine") {
        let (before, from) = rest.split_at(p);
        out.push_str(before);
        let Some(end) = from.find('>') else {
            out.push_str(from);
            return out;
        };
        let tag = &from[..=end];
        if let Some(t) = tag.find(" toc=\"") {
            let tail = &tag[t + 6..];
            if let Some(q) = tail.find('"') {
                out.push_str(&tag[..t]);
                out.push_str(&tail[q + 1..]);
                rest = &from[end + 1..];
                continue;
            }
        }
        out.push_str(tag);
        rest = &from[end + 1..];
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Шаг 2. Subsetting шрифтов
// ---------------------------------------------------------------------------

#[cfg(feature = "fonts")]
fn subset_one_font(data: &[u8], chars: &str) -> Result<Vec<u8>> {
    hb_subset::subset(data, chars.chars())
        .map_err(|e| anyhow!("HarfBuzz не смог ужать шрифт: {e}"))
}

#[cfg(not(feature = "fonts"))]
fn subset_one_font(_data: &[u8], _chars: &str) -> Result<Vec<u8>> {
    bail!("сборка без фичи `fonts`: subsetting шрифтов недоступен")
}

/// Раскодировать HTML-сущности в строку.
fn decode_entities_into(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let tail = &rest[p..];
        let Some(end) = tail.find(';').filter(|e| *e <= 12) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => {
                if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = ent.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
}

/// Все символы, которые встречаются в тексте книги.
fn collect_book_chars(entries: &[(String, Vec<u8>)]) -> String {
    let mut set = std::collections::BTreeSet::new();
    for (name, data) in entries {
        let lower = name.to_ascii_lowercase();
        if !(lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm")) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(data) else {
            continue;
        };
        let mut i = 0usize;
        while i < text.len() {
            if text.as_bytes()[i] == b'<' {
                match find_tag_end(text, i) {
                    Some(end) => i = end,
                    None => break,
                }
            } else {
                let end = text[i..].find('<').map(|p| i + p).unwrap_or(text.len());
                let mut plain = String::new();
                decode_entities_into(&mut plain, &text[i..end]);
                set.extend(plain.chars().filter(|c| !c.is_whitespace()));
                i = end;
            }
        }
    }
    // Служебные символы, которые типографика может подставить уже после подсчёта.
    for c in [' ', '\u{a0}', '-', '\u{2011}', '\u{2013}', '\u{2014}', '.', ',', '«', '»'] {
        set.insert(c);
    }
    set.into_iter().collect()
}

/// Ужать встроенные шрифты до символов книги.
fn subset_fonts_in(entries: &mut [(String, Vec<u8>)], stats: &mut ImageOptStats) -> Result<()> {
    let chars = collect_book_chars(entries);
    if chars.is_empty() {
        return Ok(());
    }
    for (name, data) in entries.iter_mut() {
        let ext = ext_lower(name);
        if !matches!(ext.as_str(), "ttf" | "otf") {
            continue;
        }
        stats.fonts_before += data.len() as u64;
        match subset_one_font(data, &chars) {
            Ok(sub) if sub.len() < data.len() => {
                stats.fonts_subset += 1;
                *data = sub;
            }
            Ok(_) => {}
            Err(err) => eprintln!("    [!] шрифт `{name}` не удалось ужать: {err}"),
        }
        stats.fonts_after += data.len() as u64;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Шаг 3. Чистка контейнера
// ---------------------------------------------------------------------------

/// Удалить `toc.ncx` (в EPUB 3 его заменяет `nav.xhtml`) и ссылки на него.
fn drop_ncx_entries(
    entries: &mut Vec<(String, Vec<u8>)>,
    opf: Option<&str>,
    stats: &mut ImageOptStats,
) {
    let bases: Vec<String> = entries
        .iter()
        .filter(|(n, _)| ext_lower(n) == "ncx")
        .map(|(n, _)| basename(n).to_string())
        .collect();
    if bases.is_empty() {
        return;
    }
    let before = entries.len();
    entries.retain(|(n, _)| ext_lower(n) != "ncx");
    stats.entries_dropped += before - entries.len();

    if let Some(opf_name) = opf {
        if let Some((_, data)) = entries.iter_mut().find(|(n, _)| n == opf_name) {
            if let Ok(text) = String::from_utf8(data.clone()) {
                let mut t = text;
                for base in &bases {
                    t = remove_opf_item(&t, base);
                }
                t = remove_spine_toc(&t);
                *data = t.into_bytes();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Главная функция
// ---------------------------------------------------------------------------

/// Формат, в который перекодируется картинка.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    WebP,
    Avif,
}

impl Format {
    /// Расширение файла.
    pub fn ext(self) -> &'static str {
        match self {
            Format::WebP => "webp",
            Format::Avif => "avif",
        }
    }

    /// MIME-тип для `content.opf`.
    pub fn mime(self) -> &'static str {
        match self {
            Format::WebP => "image/webp",
            Format::Avif => "image/avif",
        }
    }
}

/// Результат перекодирования одной картинки.
struct Encoded {
    /// Самый маленький из перебранных вариантов.
    data: Vec<u8>,
    /// В каком формате он оказался.
    format: Format,
    /// Итоговые размеры.
    w: u32,
    h: u32,
    /// Выиграл ли lossless-вариант.
    lossless: bool,
    /// Выиграл ли near-lossless-вариант.
    near_lossless: bool,
    /// Уменьшалась ли картинка.
    resized: bool,
    /// Исходные размеры (нужны для статистики).
    before_w: u32,
    before_h: u32,
}

/// Один вариант кодирования.
struct Candidate {
    data: Vec<u8>,
    format: Format,
    w: u32,
    h: u32,
    lossless: bool,
    near_lossless: bool,
}

/// Качество кодирования: обложка → `cover_quality`, мелкая → `quality_small`.
///
/// Порог — строго «меньше `small_px`»: картинка 300x900 уже считается обычной.
fn pick_quality(w: u32, h: u32, cfg: &ImageOptConfig, is_cover: bool) -> f32 {
    if is_cover {
        cfg.cover_quality
    } else if w < cfg.small_px || h < cfg.small_px {
        cfg.quality_small
    } else {
        cfg.quality
    }
}

/// Скорость AVIF: меньше — лучше сжатие по кривой «размер↔качество», но
/// дольше кодирование.
///
/// Графика и обложки кодируются аккуратно (speed 2): для текста качество важнее
/// секунд. Фотографии — по размеру кадра, потому что крупный снимок на speed 2
/// считается минутами, а выигрыш там небольшая величина.
pub fn pick_avif_speed(
    w: u32,
    h: u32,
    graphic: bool,
    is_cover: bool,
    cfg: &ImageOptConfig,
) -> u8 {
    if is_cover {
        return cfg.avif_speed_cover;
    }
    if graphic {
        return cfg.avif_speed_graphic;
    }
    // Очень крупным фотографиям разрешаем ещё на шаг быстрее: rav1e на них
    // особенно медленный, а на глаз разница уже не видна.
    let extra = u8::from((w as u64) * (h as u64) > 1_000_000);
    (cfg.avif_speed + extra).min(10)
}

/// Размер после уменьшения в `scale` раз. `scale >= 1.0` — исходный размер.
fn dims_at_scale(w: u32, h: u32, scale: f64) -> (u32, u32) {
    if scale >= 1.0 {
        return (w, h);
    }
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

/// Толщина штриха, измеренная уже на **закодированной** картинке: сжатие тоже
/// размывает края, поэтому проверяем результат, а не исходник.
fn decoded_stroke(encoded: &[u8]) -> f64 {
    image::load_from_memory(encoded)
        .ok()
        .map(|d| graphic_features(&d.to_rgba8()).1)
        .unwrap_or(0.0)
}

/// Готовая картинка после нашей обработки — то же, что попадает в EPUB.
pub struct OptimizedImage {
    /// Байты закодированного файла.
    pub data: Vec<u8>,
    /// Расширение и MIME-тип выигравшего формата.
    pub ext: &'static str,
    pub mime: &'static str,
    /// Итоговые размеры.
    pub w: u32,
    pub h: u32,
    /// Размеры исходника.
    pub before_w: u32,
    pub before_h: u32,
    /// Уменьшалась ли картинка.
    pub resized: bool,
}

/// Перекодировать **одну** картинку ровно теми же настройками, что и импорт.
///
/// Нужна для ручной проверки и для команды `library_gui --image`: результат
/// бит в бит совпадает с тем, что попадает внутрь книги. `None` означает, что
/// картинка не декодировалась.
pub fn optimize_single(
    name: &str,
    data: &[u8],
    cfg: &ImageOptConfig,
) -> Result<Option<OptimizedImage>> {
    Ok(encode_image(name, data, cfg, false)?.map(|e| OptimizedImage {
        data: e.data,
        ext: e.format.ext(),
        mime: e.format.mime(),
        w: e.w,
        h: e.h,
        before_w: e.before_w,
        before_h: e.before_h,
        resized: e.resized,
    }))
}

/// Закодировать кадр в AVIF. Серые и непрозрачные идут одним/тремя каналами.
fn avif_candidate(
    img: &RgbaImage,
    w: u32,
    h: u32,
    quality: f32,
    speed: u8,
    gray: bool,
    opaque: bool,
) -> Result<Candidate> {
    let (color, buf) = if opaque {
        if gray {
            (ExtendedColorType::L8, gray_to_luma(img))
        } else {
            (ExtendedColorType::Rgb8, rgba_to_rgb(img))
        }
    } else {
        (ExtendedColorType::Rgba8, img.as_raw().to_vec())
    };
    Ok(Candidate {
        data: encode_avif(&buf, w, h, color, quality, speed)?,
        format: Format::Avif,
        w,
        h,
        lossless: false,
        near_lossless: false,
    })
}

/// Перекодировать одну картинку.
///
/// Основной формат — AVIF; WebP остаётся только запасным на случай, если AVIF
/// не закодировался (или явно выключен в настройках).
///
/// Уменьшение выбирается лестницей: начинаем с самого сильного допустимого
/// масштаба и, если проверка качества не прошла, отступаем на шаг к исходному
/// размеру. Для графики проверка настоящая — штрих измеряется на уже
/// закодированном кадре; для фотографий граница задана масштабом.
///
/// `Ok(None)` означает «картинка не декодировалась»: её оставляют как есть,
/// но считают в статистике как сбой — молча пропустить нельзя.
///
/// Функция чистая: она ничего не знает про имена и порядок записей, поэтому
/// её можно звать из нескольких потоков сразу.
fn encode_image(
    name: &str,
    data: &[u8],
    cfg: &ImageOptConfig,
    is_cover: bool,
) -> Result<Option<Encoded>> {
    let ext = ext_lower(name);
    let t_decode = std::time::Instant::now();
    let decoded = match image::load_from_memory(data) {
        Ok(img) => img,
        Err(err) => {
            // Битый ресурс: оставляем как есть и честно сообщаем об этом.
            eprintln!("    [!] не удалось декодировать `{name}`: {err} — оставляю без изменений");
            return Ok(None);
        }
    };

    let (w, h) = (decoded.width(), decoded.height());
    if w == 0 || h == 0 {
        return Ok(None);
    }
    let rgba = decoded.to_rgba8();

    // Графика (текст, схемы) и фотографии сжимаются по-разному: у графики при
    // уменьшении слипаются штрихи, и текст перестаёт читаться.
    tally_ns(&DECODE_NS, t_decode);
    let t_features = std::time::Instant::now();
    let (per_1k, stroke, paper, sat) = graphic_features(&rgba);
    let graphic = !is_cover && cfg.text_detect && is_graphic_features(per_1k, paper, sat);
    tally_ns(&FEATURES_NS, t_features);

    let fit = (cfg.max_w as f64 / w as f64)
        .min(cfg.max_h as f64 / h as f64)
        .min(1.0);

    // Сколько уменьшать, с каким качеством и ниже какого масштаба нельзя.
    let (shrink, quality, floor, min_scale) = if is_cover {
        // Обложка только вписывается в лимит: дополнительно не уменьшаем.
        (0.0, cfg.cover_quality, 1.0, 0.0)
    } else if graphic {
        // Текст: почти без уменьшения, и только пока штрих остаётся различимым.
        // `text_min_stroke` задан в пикселях, а `stroke` измерен на исходнике,
        // поэтому допустимый масштаб — их отношение.
        let ms = if stroke > 0.0 {
            cfg.text_min_stroke / stroke
        } else {
            1.0
        };
        (cfg.text_shrink_percent, cfg.text_quality, 0.85, ms)
    } else {
        // Пол — это и есть предел уменьшения: `shrink_percent = 45` означает
        // масштаб 0.55. Прежний жёсткий пол 0.65 молча съедал всю надбавку:
        // любой `shrink` больше 35 % упирался в него и не давал ничего.
        // Мелким фотографиям уменьшение вредит сильнее, поэтому им пол мягче.
        let small = w < cfg.small_px || h < cfg.small_px;
        let target = 1.0 - cfg.shrink_percent / 100.0;
        let floor = if small { target.max(0.80) } else { target.max(0.5) };
        (cfg.shrink_percent, pick_quality(w, h, cfg, is_cover), floor, 0.0)
    };

    // Самый сильный допустимый масштаб: вписаться в лимит, уменьшить на
    // `shrink` процентов, но не ниже границы штриха и не мельче `min_side`.
    let long = w.max(h) as f64;
    let mut base = (fit * (1.0 - shrink / 100.0))
        .max(min_scale)
        .max(floor)
        .min(fit)
        .min(1.0);
    if !is_cover && long * base < cfg.min_side as f64 {
        base = (cfg.min_side as f64 / long).min(1.0);
    }

    // Лестница от сильного уменьшения к исходному размеру. Последняя ступень
    // всегда 1.0, чтобы было куда отступить.
    //
    // Для графики лестница не нужна: `base` уже посчитан как max(min_scale,
    // floor, fit*shrink), и штрих после ресайза гарантированно не тоньше
    // text_min_stroke. Отступать на исходный размер — это ещё одно полное
    // кодирование AVIF плюс декодирование ради проверки штриха, а разница
    // «2.0 px → 1.9 px» на глаз не видна. Для фотографий — как раньше.
    let ladder: Vec<f64> = if graphic {
        vec![base]
    } else {
        let mut l = vec![base];
        let mut step = base;
        while step < 0.999 && l.len() < 4 {
            step = (step + 0.1).min(1.0);
            l.push(step);
        }
        if l.last().is_some_and(|s| *s < 0.999) {
            l.push(1.0);
        }
        l
    };
    // Ступень одна — отступать некуда, поэтому проверку штриха можно не
    // делать: её результат всё равно не может изменить решение, а измерение
    // стоит полного декодирования закодированного кадра. Без этого признака
    // графика, не прошедшая проверку, не приняла бы единственного кандидата
    // (условие `ok || last` при `last == false`) и молча ушла в WebP.
    let single = ladder.len() == 1;

    let speed = pick_avif_speed(w, h, graphic, is_cover, cfg);

    // Серость и прозрачность не меняются при ресайзе. Считаем один раз
    // на исходном кадре, а не на каждом кандидате лестницы.
    let (gray_src, opaque_src) = image_kind(&rgba);

    // --- основной путь: AVIF -------------------------------------------------
    //
    // Обложка больше не форсируется в AVIF: на скорости 2 она стоила 4 с, на
    // скорости 8 — 1.7 с, и 85 обложек съедали 46 % всей работы над картинками.
    // Теперь обложка кодируется тем же форматом, что и всё остальное.
    if cfg.avif {
        let mut avif: Option<Candidate> = None;
        for &scale in &ladder {
            let (nw, nh) = dims_at_scale(w, h, scale);
            let t_resize = std::time::Instant::now();
            let resized = if (nw, nh) == (w, h) {
                rgba.clone()
            } else {
                image::imageops::resize(&rgba, nw, nh, FilterType::Lanczos3)
            };
            // Графика после уменьшения теряет резкость краёв — подчёркиваем их.
            let prepped = if graphic && (nw, nh) != (w, h) {
                image::imageops::unsharpen(&resized, 0.7, 2)
            } else {
                resized
            };
            tally_ns(&RESIZE_NS, t_resize);
            let cand = match avif_candidate(&prepped, nw, nh, quality, speed, gray_src, opaque_src)
            {
                Ok(c) => c,
                Err(err) => {
                    eprintln!("    [!] AVIF не удался для `{name}`: {err} — беру WebP");
                    break;
                }
            };
            // Штрих измеряем только там, где картинка реально уменьшалась.
            let shrunk = (nw, nh) != (w, h);
            let t_stroke = std::time::Instant::now();
            let ok = single
                || !graphic
                || !shrunk
                || decoded_stroke(&cand.data) >= cfg.text_min_stroke * 0.75;
            tally_ns(&STROKE_NS, t_stroke);
            let last = scale >= 0.999;
            if ok || last {
                avif = Some(cand);
                break;
            }
        }
        if let Some(cand) = avif {
            return Ok(Some(Encoded {
                resized: cand.w != w || cand.h != h,
                data: cand.data,
                format: cand.format,
                w: cand.w,
                h: cand.h,
                lossless: cand.lossless,
                near_lossless: cand.near_lossless,
                before_w: w,
                before_h: h,
            }));
        }
    }

    // --- запасной путь: WebP -------------------------------------------------
    // Обложке lossless-ветка не нужна: это фотография, и лишние кодирования
    // только удлиняют импорт.
    let lossless_src =
        !is_cover && cfg.try_lossless && LOSSLESS_SOURCE_EXTS.contains(&ext.as_str());
    let (nw, nh) = dims_at_scale(w, h, ladder[0]);
    let t_resize = std::time::Instant::now();
    let resized = if (nw, nh) == (w, h) {
        rgba.clone()
    } else {
        image::imageops::resize(&rgba, nw, nh, FilterType::Lanczos3)
    };
    tally_ns(&RESIZE_NS, t_resize);
    let mut candidates: Vec<Candidate> = Vec::new();
    add_webp(&mut candidates, &resized, nw, nh, cfg, quality, lossless_src)?;
    // Lossless в исходном размере: Lanczos3 сглаживает штриховую графику,
    // и lossless после ресайза раздувается, поэтому исходный вариант
    // нередко выигрывает.
    if lossless_src && (nw, nh) != (w, h) {
        add_webp_near_lossless(&mut candidates, &rgba, w, h, cfg)?;
    }
    let best = candidates.into_iter().min_by_key(|c| c.data.len());
    let Some(best) = best else {
        return Ok(None);
    };
    Ok(Some(Encoded {
        data: best.data,
        format: best.format,
        w: best.w,
        h: best.h,
        lossless: best.lossless,
        near_lossless: best.near_lossless,
        resized: best.w != w || best.h != h,
        before_w: w,
        before_h: h,
    }))
}

/// Варианты WebP: lossy и, если уместно, near-lossless.
fn add_webp(
    out: &mut Vec<Candidate>,
    img: &RgbaImage,
    w: u32,
    h: u32,
    cfg: &ImageOptConfig,
    quality: f32,
    near: bool,
) -> Result<()> {
    let _t = std::time::Instant::now();
    let opaque = is_opaque(img);
    let p = WebpParams::lossy(
        quality,
        cfg.method,
        cfg.sns_strength,
        cfg.filter_strength,
        cfg.webp_pass,
        cfg.webp_autofilter,
    );
    let data = if opaque {
        encode_webp_rgb(&rgba_to_rgb(img), w, h, &p)?
    } else {
        encode_webp(img.as_raw(), w, h, &p)?
    };
    out.push(Candidate {
        data,
        format: Format::WebP,
        w,
        h,
        lossless: false,
        near_lossless: false,
    });

    if near && cfg.near_lossless {
        add_webp_near_lossless(out, img, w, h, cfg)?;
    }
    tally_ns(&WEBP_NS, _t);
    Ok(())
}

/// Вариант near-lossless WebP (только для png/gif/bmp/tiff).
fn add_webp_near_lossless(
    out: &mut Vec<Candidate>,
    img: &RgbaImage,
    w: u32,
    h: u32,
    cfg: &ImageOptConfig,
) -> Result<()> {
    if !cfg.near_lossless {
        return Ok(());
    }
    let _t = std::time::Instant::now();
    let opaque = is_opaque(img);
    let p = WebpParams::near_lossless(cfg.lossless_quality, cfg.method, cfg.near_lossless_quality);
    let data = if opaque {
        encode_webp_rgb(&rgba_to_rgb(img), w, h, &p)?
    } else {
        encode_webp(img.as_raw(), w, h, &p)?
    };
    out.push(Candidate {
        data,
        format: Format::WebP,
        w,
        h,
        lossless: true,
        near_lossless: cfg.near_lossless_quality > 0,
    });
    tally_ns(&WEBP_NS, _t);
    Ok(())
}

/// Прочитать все записи EPUB в память.
fn read_entries(input: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let file = File::open(input)
        .with_context(|| format!("не удалось открыть EPUB: {}", input.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("EPUB не читается как ZIP: {}", input.display()))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue; // каталоги в ZIP не нужны
        }
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut data)
            .with_context(|| format!("не удалось прочитать `{name}` из EPUB"))?;
        entries.push((name, data));
    }
    if entries.is_empty() {
        bail!("EPUB пуст: {}", input.display());
    }
    Ok(entries)
}

/// Записи после шага «картинки» + статистика + путь к OPF.
struct ImageStage {
    entries: Vec<(String, Vec<u8>)>,
    stats: ImageOptStats,
    opf_name: Option<String>,
}

/// Задание для пула кодирования.
type EncodeJob = Box<dyn FnOnce() + Send + 'static>;

/// Общий на процесс пул потоков для AVIF-кодирования.
///
/// Книги обрабатываются параллельно (поток на книгу), но картинка — единица
/// работы очень разного размера: в корпусе есть книги с одной картинкой и
/// книги с двумя сотнями. Если кодировать картинки книги строго по очереди,
/// книга-гигант занимает одно ядро на минуты, а остальные потоки простаивают.
/// Общий пул раздаёт работу по картинкам, поэтому ядра занимает и большая
/// книга, а мелкие не плодят по потоку на каждую.
///
/// Пул именно свой, а не `rayon`: `ravif`/`rav1e` внутри зовут rayon, и
/// заблокированный на них воркер «крадёт» следующую задачу из того же
/// реестра — вложенность растёт до числа картинок и рвёт стек (см. `worker.rs`).
fn encode_pool() -> &'static crossbeam_channel::Sender<EncodeJob> {
    static POOL: OnceLock<crossbeam_channel::Sender<EncodeJob>> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::env::var("LIBRARY_ENCODE_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                // 1.5x ядер: decode / resize / encode дают короткие CPU-bound
                // отрезки, и на переходах между ними честные N потоков
                // недогружают ядра.
                std::thread::available_parallelism()
                    .map(|n| (n.get() * 3 / 2).max(2))
                    .unwrap_or(6)
            });
        let (tx, rx) = crossbeam_channel::unbounded::<EncodeJob>();
        for i in 0..threads {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("avif-enc-{i}"))
                // Столько же, сколько у потоков импорта: `rav1e` рекурсивен.
                .stack_size(16 * 1024 * 1024)
                .spawn(move || {
                    for job in rx {
                        job();
                    }
                })
                .expect("не удалось создать поток кодирования картинок");
        }
        tx
    })
}

/// Закодировать все растровые картинки книги через общий пул.
///
/// Порядок результатов совпадает с порядком `raster_idx` — на него опирается
/// раздача имён ниже. Байты картинок не копируются: задания держат `Arc` на
/// тот же список записей.
fn encode_all(
    entries: &Arc<Vec<(String, Vec<u8>)>>,
    raster_idx: &[usize],
    cover_name: Option<&str>,
    cfg: &ImageOptConfig,
) -> Vec<Result<Option<Encoded>>> {
    if raster_idx.is_empty() {
        return Vec::new();
    }
    let mut slots: Vec<Option<Result<Option<Encoded>>>> =
        (0..raster_idx.len()).map(|_| None).collect();

    let (done_tx, done_rx) = crossbeam_channel::bounded(raster_idx.len());
    for (slot, &entry_idx) in raster_idx.iter().enumerate() {
        let entries = Arc::clone(entries);
        let cfg = cfg.clone();
        let cover = cover_name.map(str::to_string);
        let done_tx = done_tx.clone();
        encode_pool()
            .send(Box::new(move || {
                let (name, data) = &entries[entry_idx];
                let is_cover = cover.as_deref().is_some_and(|c| basename(name) == c);
                let outcome = encode_image(name, data, &cfg, is_cover);
                let _ = done_tx.send((slot, outcome));
            }))
            .expect("пул кодирования картинок недоступен");
    }
    drop(done_tx);
    for (slot, outcome) in done_rx {
        slots[slot] = Some(outcome);
    }
    slots
        .into_iter()
        .map(|s| s.expect("задание кодирования не вернуло результат"))
        .collect()
}

/// Шаг «картинки»: перекодировать, переименовать, поправить ссылки и OPF.
///
/// Это самая тяжёлая часть (libwebp method 6 + AVIF), но картинки одной книги
/// кодируются **последовательно**, а параллелизм даёт обработка книг.
/// Вложенный `par_iter` здесь был бы ловушкой: `ravif`/`rav1e` внутри сами
/// зовут rayon, и заблокированный на них поток «крадёт» следующую картинку из
/// нашего же реестра — вложенность растёт до числа картинок в книге и рвёт
/// стек (см. `worker.rs`).
fn process_images(input: &Path, cfg: &ImageOptConfig) -> Result<ImageStage> {
    // `Arc`, а не голый `Vec`: задания кодирования живут дольше этой функции,
    // и копировать байты картинок на каждое задание незачем.
    let entries = Arc::new(read_entries(input)?);

    // Обложку опознаём по `properties="cover-image"` в OPF: её кодируем
    // только в AVIF и с качеством 50.
    let cover_name = find_cover_image(&entries);
    let opf_name = find_opf_name(&entries);

    // Один проход: is_raster_entry делает ext_lower (аллокация String) и
    // sniff_image (сравнение сигнатур), три вызова подряд — 2/3 работы зря.
    let is_raster: Vec<bool> = entries
        .iter()
        .map(|(n, d)| is_raster_entry(n, d))
        .collect();
    let raster_idx: Vec<usize> = is_raster
        .iter()
        .enumerate()
        .filter(|(_, r)| **r)
        .map(|(i, _)| i)
        .collect();
    let encoded = encode_all(&entries, &raster_idx, cover_name.as_deref(), cfg);

    // `encode_all` уже дождался всех заданий: канал `done_rx` закрывается
    // только вместе с последним замыканием, а оно держит `Arc`, — значит,
    // клонов больше нет и `Arc` разворачивается. Дальше байты перемещаются,
    // а не копируются: у книги с 5 МБ текста это лишние 5 МБ `memcpy`.
    let entries = match Arc::try_unwrap(entries) {
        Ok(v) => v,
        Err(arc) => (*arc).clone(),
    };

    let mut stats = ImageOptStats::default();
    let mut used_names: HashSet<String> = entries.iter().map(|(n, _)| n.clone()).collect();
    // (старое basename, новое basename, MIME) — по basename, как их видят ссылки.
    let mut renames: Vec<(String, String, String)> = Vec::new();
    let mut out_entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(entries.len());

    // Имена выдаются последовательно: результат не должен зависеть от того,
    // в каком порядке потоки закончили кодирование.
    let mut enc_iter = encoded.into_iter();

    for (i, (name, data)) in entries.into_iter().enumerate() {
        if !is_raster[i] {
            out_entries.push((name, data));
            continue;
        }

        let Some(result) = enc_iter.next() else {
            bail!("внутренняя ошибка: нет результата кодирования для `{name}`");
        };

        stats.images += 1;
        stats.bytes_before += data.len() as u64;

        let Some(enc) = result? else {
            // Картинка не декодировалась — оставляем байты как есть.
            stats.failed += 1;
            stats.bytes_after += data.len() as u64;
            out_entries.push((name, data));
            continue;
        };

        stats.max_before = (
            stats.max_before.0.max(enc.before_w),
            stats.max_before.1.max(enc.before_h),
        );
        if enc.resized && (enc.w != enc.before_w || enc.h != enc.before_h) {
            stats.resized += 1;
        }
        if enc.lossless {
            stats.lossless += 1;
        }
        if enc.near_lossless {
            stats.near_lossless += 1;
        }
        match enc.format {
            Format::WebP => stats.webp += 1,
            Format::Avif => stats.avif += 1,
        }
        stats.max_after = (stats.max_after.0.max(enc.w), stats.max_after.1.max(enc.h));
        stats.bytes_after += enc.data.len() as u64;

        // --- новое имя -------------------------------------------------------
        let ext = enc.format.ext();
        used_names.remove(&name);
        let candidate_name = if ext_lower(&name) == ext {
            name.clone()
        } else {
            format!("{}.{}", strip_ext(&name), ext)
        };
        let mut final_name = candidate_name.clone();
        let mut n = 1;
        while used_names.contains(&final_name) {
            final_name = format!("{}.{}.{}", strip_ext(&name), n, ext);
            n += 1;
        }
        used_names.insert(final_name.clone());

        if final_name != name {
            renames.push((
                basename(&name).to_string(),
                basename(&final_name).to_string(),
                enc.format.mime().to_string(),
            ));
        }
        out_entries.push((final_name, enc.data));
    }

    // --- ссылки на переименованные картинки ---------------------------------
    // Длинные имена первыми: чтобы `i_001.jpg.jpg` не пострадал от замены `i_001.jpg`.
    renames.sort_by_key(|r| std::cmp::Reverse(r.0.len()));
    for (name, data) in out_entries.iter_mut() {
        if name == "mimetype" || !TEXT_EXTS.contains(&ext_lower(name).as_str()) {
            continue;
        }
        let Ok(mut text) = String::from_utf8(data.clone()) else {
            continue;
        };
        let mut changed = false;
        for (old, new, _) in &renames {
            let (next, did) = replace_refs(&text, old, new);
            text = next;
            changed |= did;
        }
        // В OPF, кроме href, надо поправить `media-type` — точечно, по `<item>`.
        if Some(name.as_str()) == opf_name.as_deref() {
            let fixed = rewrite_opf_items(&text, &renames);
            if fixed != text {
                text = fixed;
                changed = true;
            }
        }
        if changed {
            *data = text.into_bytes();
        }
    }

    Ok(ImageStage {
        entries: out_entries,
        stats,
        opf_name,
    })
}

/// Шаг 1: минификация XHTML/CSS/OPF + чистка метаданных Calibre.
fn minify_step(
    entries: &mut [(String, Vec<u8>)],
    opf: Option<&str>,
    cfg: &ImageOptConfig,
    stats: &mut ImageOptStats,
) {
    for (name, data) in entries.iter_mut() {
        let ext = ext_lower(name);
        let is_opf = Some(name.as_str()) == opf;
        let is_text = matches!(
            ext.as_str(),
            "xhtml" | "html" | "htm" | "css" | "opf" | "ncx"
        );
        if !is_text {
            continue;
        }
        let Ok(text) = String::from_utf8(data.clone()) else {
            continue;
        };
        stats.text_before += text.len() as u64;
        let mut new = if ext == "css" {
            minify_css(&text)
        } else {
            minify_xhtml(&text)
        };
        if is_opf && cfg.strip_calibre_meta {
            new = strip_calibre_metadata(&new);
        }
        let chosen = if new.len() < text.len() { new } else { text };
        stats.text_after += chosen.len() as u64;
        if chosen.len() != data.len() || chosen.as_bytes() != data.as_slice() {
            *data = chosen.into_bytes();
        }
    }
}

/// Записи EPUB в памяти: путь внутри архива → байты.
pub type EpubEntries = Vec<(String, Vec<u8>)>;

/// Все шаги 1-3: минификация, subsetting шрифтов, чистка контейнера.
pub fn prepare_entries(
    input: &Path,
    cfg: &ImageOptConfig,
) -> Result<(EpubEntries, ImageOptStats)> {
    let ImageStage {
        mut entries,
        mut stats,
        opf_name,
    } = process_images(input, cfg)?;

    if cfg.minify {
        minify_step(&mut entries, opf_name.as_deref(), cfg, &mut stats);
    }
    if cfg.subset_fonts {
        subset_fonts_in(&mut entries, &mut stats)?;
    }
    if cfg.drop_ncx {
        drop_ncx_entries(&mut entries, opf_name.as_deref(), &mut stats);
    }
    Ok((entries, stats))
}

/// Записать EPUB: `mimetype` первым и всегда Stored, остальное — по флагу.
///
/// `stored = true` оставляет текст несжатым: тогда `.arx` жмёт батч целиком
/// (zstd) и получается заметно меньше, чем Deflate внутри каждой книги.
pub fn pack_epub(
    entries: &[(String, Vec<u8>)],
    output: &Path,
    stored: bool,
    level: u8,
) -> Result<()> {
    let mime_pos = entries
        .iter()
        .position(|(n, _)| n == "mimetype")
        .ok_or_else(|| anyhow!("в EPUB нет файла `mimetype`"))?;

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out_file = File::create(output)
        .with_context(|| format!("не удалось создать {}", output.display()))?;
    let mut writer = ZipWriter::new(out_file);

    // Время изменения — фиксированное (1980-01-01, эпоха ZIP). Иначе каждый
    // прогон даёт разные байты в local header, и `.arx` поверх них отличается
    // на ±0.2 %: сборка перестаёт быть воспроизводимой, а zstd хуже находит
    // общие заголовки между книгами (на 14 книгах — 9 366 Б разницы).
    let stamp = zip::DateTime::default();
    writer.start_file(
        entries[mime_pos].0.clone(),
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .last_modified_time(stamp),
    )?;
    writer.write_all(&entries[mime_pos].1)?;

    let opts = if stored {
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .last_modified_time(stamp)
    } else {
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(i64::from(level.clamp(1, 9))))
            .last_modified_time(stamp)
    };
    for (i, (name, data)) in entries.iter().enumerate() {
        if i == mime_pos {
            continue;
        }
        writer.start_file(name.clone(), opts)?;
        writer.write_all(data)?;
    }
    writer.finish()?;
    Ok(())
}

/// Оптимизировать EPUB полностью и записать результат в `output`.
pub fn optimize_epub(input: &Path, output: &Path, cfg: &ImageOptConfig) -> Result<ImageOptStats> {
    let (entries, stats) = prepare_entries(input, cfg)?;
    pack_epub(&entries, output, cfg.epub_stored, cfg.zip_level)?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Бенчмарк по стадиям
// ---------------------------------------------------------------------------

/// Размеры EPUB на каждой стадии оптимизации одной книги.
#[derive(Debug, Clone, Copy, Default)]
pub struct BenchReport {
    /// После картинок, без шагов 1-3.
    pub base: u64,
    /// + минификация XHTML/CSS/OPF.
    pub minified: u64,
    /// + subsetting шрифтов.
    pub fonts: u64,
    /// + чистка контейнера (`toc.ncx`).
    pub clean: u64,
    /// Итог, но с записями без Deflate (для упаковки в `.arx`).
    pub stored: u64,
    pub images: usize,
    pub text_before: u64,
    pub text_after: u64,
    pub fonts_before: u64,
    pub fonts_after: u64,
    pub fonts_subset: usize,
    pub entries_dropped: usize,
}

impl BenchReport {
    /// Сложить два отчёта (для прогона по многим книгам).
    pub fn merge(&mut self, other: &BenchReport) {
        self.base += other.base;
        self.minified += other.minified;
        self.fonts += other.fonts;
        self.clean += other.clean;
        self.stored += other.stored;
        self.images += other.images;
        self.text_before += other.text_before;
        self.text_after += other.text_after;
        self.fonts_before += other.fonts_before;
        self.fonts_after += other.fonts_after;
        self.fonts_subset += other.fonts_subset;
        self.entries_dropped += other.entries_dropped;
    }
}

fn packed_size(entries: &[(String, Vec<u8>)], path: &Path, stored: bool, level: u8) -> Result<u64> {
    pack_epub(entries, path, stored, level)?;
    Ok(std::fs::metadata(path)?.len())
}

/// Прогнать одну книгу по стадиям и записать итоговые EPUB в двух вариантах:
/// с Deflate (`out_deflate`) и без внутреннего сжатия (`out_stored`).
pub fn benchmark_epub(
    input: &Path,
    out_deflate: &Path,
    out_stored: &Path,
    work: &Path,
    cfg: &ImageOptConfig,
) -> Result<BenchReport> {
    let ImageStage {
        mut entries,
        mut stats,
        opf_name,
    } = process_images(input, cfg)?;

    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book".to_string());
    let tmp = work.join(format!("{stem}.tmp.epub"));
    let mut report = BenchReport {
        base: packed_size(&entries, &tmp, false, cfg.zip_level)?,
        ..Default::default()
    };

    minify_step(&mut entries, opf_name.as_deref(), cfg, &mut stats);
    report.minified = packed_size(&entries, &tmp, false, cfg.zip_level)?;

    subset_fonts_in(&mut entries, &mut stats)?;
    report.fonts = packed_size(&entries, &tmp, false, cfg.zip_level)?;

    drop_ncx_entries(&mut entries, opf_name.as_deref(), &mut stats);
    report.clean = packed_size(&entries, &tmp, false, cfg.zip_level)?;
    report.stored = packed_size(&entries, &tmp, true, cfg.zip_level)?;

    std::fs::create_dir_all(out_deflate)?;
    std::fs::create_dir_all(out_stored)?;
    pack_epub(
        &entries,
        &out_deflate.join(format!("{stem}.epub")),
        false,
        cfg.zip_level,
    )?;
    pack_epub(
        &entries,
        &out_stored.join(format!("{stem}.epub")),
        true,
        cfg.zip_level,
    )?;
    let _ = std::fs::remove_file(&tmp);

    report.images = stats.images;
    report.text_before = stats.text_before;
    report.text_after = stats.text_after;
    report.fonts_before = stats.fonts_before;
    report.fonts_after = stats.fonts_after;
    report.fonts_subset = stats.fonts_subset;
    report.entries_dropped = stats.entries_dropped;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Правка OPF: href и media-type у переименованных картинок
// ---------------------------------------------------------------------------

/// Путь к OPF внутри EPUB: из `META-INF/container.xml`, иначе первый `*.opf`.
fn find_opf_name(entries: &[(String, Vec<u8>)]) -> Option<String> {
    if let Some((_, data)) = entries.iter().find(|(n, _)| n == "META-INF/container.xml") {
        let text = String::from_utf8_lossy(data);
        if let Some(p) = text.find("full-path=\"") {
            let s = p + "full-path=\"".len();
            if let Some(e) = text[s..].find('"') {
                let path = &text[s..s + e];
                if entries.iter().any(|(n, _)| n == path) {
                    return Some(path.to_string());
                }
            }
        }
    }
    entries
        .iter()
        .find(|(n, _)| n.to_lowercase().ends_with(".opf"))
        .map(|(n, _)| n.clone())
}

/// Значение атрибута `name="..."` внутри одного тега `<item ...>`.
fn item_attr(item: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let p = item.find(&key)? + key.len();
    let e = item[p..].find('"')? + p;
    Some(item[p..e].to_string())
}

/// Имя файла обложки (basename) по `properties="cover-image"` в OPF.
fn find_cover_image(entries: &[(String, Vec<u8>)]) -> Option<String> {
    let opf_name = find_opf_name(entries)?;
    let (_, data) = entries.iter().find(|(n, _)| n == &opf_name)?;
    let text = String::from_utf8_lossy(data).into_owned();
    let mut from = 0usize;
    while let Some(p) = text[from..].find("<item") {
        let start = from + p;
        let end = text[start..].find('>')? + start;
        let item = &text[start..=end];
        if item.contains("cover-image") {
            if let Some(href) = item_attr(item, "href") {
                return Some(basename(&href).to_string());
            }
        }
        from = end + 1;
    }
    None
}

/// Готовые байты обложки из оптимизированного EPUB.
///
/// Раньше обложка кодировалась **второй раз**: `optimize_epub` уже сделала
/// `images/cover.avif`, а `extract_cover` заново декодировал исходник,
/// уменьшал его и снова гонял AVIF-энкодер — это около 12 % всей
/// процессорной работы импорта на ровном месте. Здесь берём готовое.
pub fn cover_bytes(epub: &Path) -> Result<Option<(String, Vec<u8>)>> {
    let entries = read_entries(epub)?;
    let Some(wanted) = find_cover_image(&entries) else {
        return Ok(None);
    };
    Ok(entries
        .into_iter()
        .find(|(n, _)| basename(n) == wanted)
        .map(|(_, data)| (wanted, data)))
}

/// Переписать `<item>` в OPF: `href` и `media-type` у переименованных картинок.
///
/// Заодно чинит `application/octet-stream` у картинок, которые уже `.webp`/`.avif`
/// (например, у обложки без расширения в исходнике).
fn rewrite_opf_items(text: &str, renames: &[(String, String, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find("<item") else {
            out.push_str(rest);
            return out;
        };
        let (before, from_item) = rest.split_at(start);
        out.push_str(before);
        // `<itemref ...>` — не наш тег, не трогаем.
        let after = &from_item[5..];
        if !after.starts_with([' ', '\n', '\r', '\t']) {
            out.push_str("<item");
            rest = after;
            continue;
        }
        let Some(end) = from_item.find('>') else {
            out.push_str(from_item);
            return out;
        };
        let (item, tail) = from_item.split_at(end + 1);
        out.push_str(&rewrite_item(item, renames));
        rest = tail;
    }
}

/// Переписать один тег `<item .../>`.
fn rewrite_item(item: &str, renames: &[(String, String, String)]) -> String {
    let Some(href) = item_attr(item, "href") else {
        return item.to_string();
    };
    let base = basename(&href);
    let mut out = item.to_string();
    let mut new_href: Option<String> = None;
    let mut new_mime: Option<String> = None;

    if let Some((_, new, mime)) = renames.iter().find(|(old, _, _)| old == base) {
        new_href = Some(href.replace(base, new));
        new_mime = Some(mime.clone());
    } else {
        // Уже в современном формате, но тип мог остаться `application/octet-stream`.
        let mime = match ext_lower(&href).as_str() {
            "webp" => Some("image/webp"),
            "avif" => Some("image/avif"),
            _ => None,
        };
        if let Some(mime) = mime {
            if let Some(mt) = item_attr(item, "media-type") {
                if mt == "application/octet-stream" || RASTER_MIME_TYPES.contains(&mt.as_str()) {
                    new_mime = Some(mime.to_string());
                }
            }
        }
    }

    if let Some(h) = new_href {
        out = out.replace(&format!("href=\"{href}\""), &format!("href=\"{h}\""));
    }
    if let Some(m) = new_mime {
        if let Some(mt) = item_attr(&out, "media-type") {
            if mt != m {
                out = out.replace(
                    &format!("media-type=\"{mt}\""),
                    &format!("media-type=\"{m}\""),
                );
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Поиск «висячих» ссылок (битых ресурсов) внутри EPUB
// ---------------------------------------------------------------------------

/// Достать все ссылки на ресурсы из текста (`src=`, `href=`, CSS `url(...)`).
fn collect_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["src=\"", "href=\""] {
        let mut from = 0usize;
        while let Some(p) = text.get(from..).and_then(|s| s.find(key)) {
            let start = from + p + key.len();
            match text.get(start..).and_then(|s| s.find('"')) {
                Some(e) => {
                    out.push(text[start..start + e].to_string());
                    from = start + e + 1;
                }
                None => break,
            }
        }
    }
    let mut from = 0usize;
    while let Some(p) = text.get(from..).and_then(|s| s.find("url(")) {
        let start = from + p + 4;
        match text.get(start..).and_then(|s| s.find(')')) {
            Some(e) => {
                let raw = text[start..start + e].trim().trim_matches(['"', '\'']);
                out.push(raw.to_string());
                from = start + e + 1;
            }
            None => break,
        }
    }
    out
}

/// Нормализовать путь `target` относительно каталога `base_dir` (убрать `.` и `..`).
fn normalize_join(base_dir: &str, target: &str) -> String {
    let joined = if base_dir.is_empty() {
        target.to_string()
    } else {
        format!("{base_dir}/{target}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Найти ссылки на локальные ресурсы, которых **нет** внутри EPUB.
///
/// Возвращает список `(файл, из которого ссылка, отсутствующий ресурс)`.
/// Это позволяет отличить дефекты исходной книги от ошибок собственной правки ссылок.
pub fn find_dangling_refs(path: &Path) -> Result<Vec<(String, String)>> {
    let file =
        File::open(path).with_context(|| format!("не удалось открыть {}", path.display()))?;
    let mut archive = ZipArchive::new(file)?;

    let mut names: HashSet<String> = HashSet::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        if !entry.is_dir() {
            names.insert(entry.name().to_string());
        }
    }

    let mut dangling = Vec::new();
    for name in names.iter() {
        if !TEXT_EXTS.contains(&ext_lower(name).as_str()) {
            continue;
        }
        let mut data = Vec::new();
        archive
            .by_name(name)
            .with_context(|| format!("не удалось прочитать `{name}`"))?
            .read_to_end(&mut data)?;
        let text = String::from_utf8_lossy(&data).into_owned();

        let base_dir = match name.rfind('/') {
            Some(i) => &name[..i],
            None => "",
        };
        for target in collect_refs(&text) {
            let t = target.split(['#', '?']).next().unwrap_or("");
            if t.is_empty()
                || t.starts_with("http:")
                || t.starts_with("https:")
                || t.starts_with("data:")
                || t.starts_with("mailto:")
                || t.starts_with("//")
                || t.starts_with('#')
            {

                continue;
            }
            let full = normalize_join(base_dir, t);
            if names.contains(&full) || names.contains(t) {
                continue;
            }
            dangling.push((name.clone(), target.clone()));
        }
    }
    Ok(dangling)
}

// ---------------------------------------------------------------------------
// Проверка «100 % WebP»
// ---------------------------------------------------------------------------

/// Сколько растровых картинок в EPUB и сколько из них WebP.
pub fn count_images(path: &Path) -> Result<(usize, usize, usize)> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut total = 0;
    let mut webp = 0;
    let mut avif = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_ascii_lowercase();
        if name.ends_with(".webp") {
            total += 1;
            webp += 1;
            continue;
        }
        if name.ends_with(".avif") {
            total += 1;
            avif += 1;
            continue;
        }
        if RASTER_EXTS.iter().any(|e| name.ends_with(&format!(".{e}"))) {
            total += 1;
            continue;
        }
        // Файл без «картиночного» расширения может всё же быть картинкой:
        // Calibre кладёт обложку записью `cover`. Считаем её по содержимому,
        // иначе проверка «100 % WebP/AVIF» её не увидит и пропустит JPEG.
        let mut head = [0u8; 16];
        let mut filled = 0usize;
        while filled < head.len() {
            match entry.read(&mut head[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    return Err(e).context("не удалось прочитать начало записи EPUB")
                }
            }
        }
        if filled > 0 && looks_like_raster(&head[..filled]) {
            total += 1;
        }
    }
    Ok((total, webp, avif))
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Минимальное «оглавление» AVIF: `ftyp` + бренд `avif`.
    fn avif_header() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&24u32.to_be_bytes());
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(b"avif");
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(b"mif1avif");
        v
    }

    #[test]
    fn sniff_avif_by_brand() {
        assert_eq!(sniff_image(&avif_header()), Some(("avif", "image/avif")));
    }

    #[test]
    fn sniff_webp_unchanged() {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(b"WEBP");
        assert_eq!(sniff_image(&v), Some(("webp", "image/webp")));
    }

    #[test]
    fn grayscale_and_opacity_detection() {
        let gray = RgbaImage::from_pixel(8, 8, image::Rgba([17, 17, 17, 255]));
        assert!(is_grayscale(&gray));
        assert!(is_opaque(&gray));

        let colored = RgbaImage::from_pixel(8, 8, image::Rgba([17, 30, 17, 255]));
        assert!(!is_grayscale(&colored));

        let translucent = RgbaImage::from_pixel(8, 8, image::Rgba([17, 17, 17, 0]));
        assert!(!is_opaque(&translucent));
    }

    #[test]
    fn avif_round_trip() {
        let img = RgbaImage::from_pixel(32, 24, image::Rgba([200, 30, 30, 255]));
        let avif = encode_avif(
            img.as_raw(),
            32,
            24,
            ExtendedColorType::Rgba8,
            50.0,
            10,
        )
        .expect("AVIF должен кодироваться");
        assert_eq!(sniff_image(&avif), Some(("avif", "image/avif")));

        let back = image::load_from_memory(&avif).expect("AVIF должен декодироваться");
        assert_eq!((back.width(), back.height()), (32, 24));
    }

    #[test]
    fn opf_rewrite_updates_href_and_media_type() {
        let renames = vec![(
            "cover.jpg".to_string(),
            "cover.avif".to_string(),
            "image/avif".to_string(),
        )];
        let opf = r##"<manifest><item id="cover" href="images/cover.jpg" media-type="image/jpeg"/><item id="css" href="style.css" media-type="text/css"/></manifest><spine><itemref idref="cover"/></spine>"##;
        let out = rewrite_opf_items(opf, &renames);
        assert!(out.contains(r#"href="images/cover.avif""#), "{out}");
        assert!(out.contains(r#"media-type="image/avif""#), "{out}");
        assert!(out.contains(r#"href="style.css" media-type="text/css""#), "{out}");
        // `<itemref>` — не `<item>`, его трогать нельзя.
        assert!(out.contains(r#"<itemref idref="cover"/>"#), "{out}");
    }

    #[test]
    fn opf_octet_stream_becomes_webp() {
        let opf = r#"<item id="x" href="i_1.webp" media-type="application/octet-stream"/>"#;
        let out = rewrite_opf_items(opf, &[]);
        assert!(out.contains(r#"media-type="image/webp""#), "{out}");
    }

    #[test]
    fn opf_leaves_css_media_type_alone() {
        let opf = r#"<item id="css" href="style.css" media-type="text/css"/>"#;
        assert_eq!(rewrite_opf_items(opf, &[]), opf);
    }

    #[test]
    fn cover_image_found_by_properties() {
        let entries = vec![
            (
                "META-INF/container.xml".to_string(),
                br#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#.to_vec(),
            ),
            (
                "OEBPS/content.opf".to_string(),
                br#"<package><manifest><item id="i0" href="images/cover.jpg" media-type="image/jpeg" properties="cover-image"/></manifest></package>"#.to_vec(),
            ),
        ];
        assert_eq!(find_cover_image(&entries), Some("cover.jpg".to_string()));
        assert_eq!(find_opf_name(&entries).as_deref(), Some("OEBPS/content.opf"));
    }

    #[test]
    fn avif_grayscale_luma_round_trip() {
        let img = RgbaImage::from_pixel(16, 16, image::Rgba([120, 120, 120, 255]));
        let buf = gray_to_luma(&img);
        let avif = encode_avif(&buf, 16, 16, ExtendedColorType::L8, 50.0, 10)
            .expect("AVIF L8 должен кодироваться");
        let back = image::load_from_memory(&avif)
            .expect("AVIF L8 должен декодироваться")
            .to_rgba8();
        let p = back.get_pixel(0, 0);
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[2]);
        assert!(
            (i32::from(p[0]) - 120).abs() < 12,
            "яркость {} должна быть около 120",
            p[0]
        );
    }

    #[test]
    fn quality_is_lower_for_small_images_and_covers() {
        let cfg = ImageOptConfig {
            quality: 35.0,
            quality_small: 50.0,
            cover_quality: 50.0,
            ..Default::default()
        };
        assert_eq!(pick_quality(299, 900, &cfg, false), 50.0);
        assert_eq!(pick_quality(900, 299, &cfg, false), 50.0);
        assert_eq!(pick_quality(300, 900, &cfg, false), 35.0);
        assert_eq!(pick_quality(900, 300, &cfg, false), 35.0);
        assert_eq!(pick_quality(100, 100, &cfg, true), 50.0);
    }

    // --- шаги 1-3: минификация, шрифты, контейнер ---------------------------

    /// Собрать минимальный EPUB в памяти: `mimetype` + OPF + XHTML + NCX.
    fn sample_epub(path: &Path) {
        let opf = r##"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="book-id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Тест</dc:title>
    <!-- комментарий, который не нужен -->
    <meta name="calibre:timestamp" content="2020-01-01T00:00:00+00:00"/>
    <meta name="generator" content="calibre 5.0"/>
    <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="ch1"/>
  </spine>
</package>
"##;
        let xhtml = r##"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="ru">
  <head><title>Глава</title></head>
  <body>
    <!-- лишний комментарий -->
    <p>Слово <em>важное</em> <em>ещё</em> и &laquo;цитата&raquo;.</p>
    <pre>  отступ   значим  </pre>
  </body>
</html>
"##;
        let entries: [(&str, &str); 6] = [
            ("mimetype", "application/epub+zip"),
            (
                "META-INF/container.xml",
                r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            ),
            ("OEBPS/content.opf", opf),
            ("OEBPS/ch1.xhtml", xhtml),
            ("OEBPS/nav.xhtml", xhtml),
            ("OEBPS/toc.ncx", r#"<?xml version="1.0"?><ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1"/>"#),
        ];
        let file = File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, data) in entries {
            zip.start_file(name, opts).unwrap();
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn minify_keeps_space_between_inline_tags() {
        let src = "<p>раз <em>два</em> <em>три</em>.</p>";
        assert_eq!(minify_xhtml(src), src);
    }

    #[test]
    fn minify_drops_space_between_block_tags_and_comments() {
        let src = "<div>\n  <p>а</p>\n  <!-- нет -->\n  <p>б</p>\n</div>";
        assert_eq!(minify_xhtml(src), "<div><p>а</p><p>б</p></div>");
    }

    #[test]
    fn minify_keeps_pre_content() {
        let src = "<pre>  отступ   значим  </pre>";
        assert_eq!(minify_xhtml(src), src);
    }

    #[test]
    fn minify_collapses_text_whitespace() {
        assert_eq!(minify_xhtml("<p>а   б\n в</p>"), "<p>а б в</p>");
    }

    #[test]
    fn minify_css_removes_comments_and_keeps_strings() {
        let src = "/* шапка */\nbody {\n  margin: 0;\n  content: \"a ; b  c\";\n}\np > a { color: red; }";
        let out = minify_css(src);
        assert_eq!(out, "body{margin:0;content:\"a ; b  c\"}p>a{color:red}");
    }

    #[test]
    fn strip_calibre_meta_removes_only_calibre() {
        let opf = r#"<metadata><meta name="calibre:series" content="X"/><meta name="cover" content="c"/><meta property="dcterms:modified">2020</meta></metadata>"#;
        let out = strip_calibre_metadata(opf);
        assert!(!out.contains("calibre"));
        assert!(out.contains(r#"name="cover""#));
        assert!(out.contains("dcterms:modified"));
    }

    #[test]
    fn collect_chars_decodes_entities_and_skips_tags() {
        let entries = vec![(
            "ch.xhtml".to_string(),
            "<p title=\"не считается\">Привет &laquo;мир&raquo; &#1055;</p>"
                .as_bytes()
                .to_vec(),
        )];
        let chars = collect_book_chars(&entries);
        assert!(chars.contains('П'));
        assert!(chars.contains('«'));
        assert!(chars.contains('м'));
        assert!(!chars.contains('t'));
    }

    /// Тест зависит от системного шрифта: если его нет — просто пропускаем.
    #[cfg(feature = "fonts")]
    #[test]
    fn subset_fonts_shrinks_embedded_ttf() {
        let candidates = [
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        ];
        let Some(font) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
            eprintln!("системный TTF не найден — тест subsetting пропущен");
            return;
        };
        let mut entries = vec![
            ("OEBPS/fonts/DejaVuSans.ttf".to_string(), font),
            (
                "OEBPS/ch1.xhtml".to_string(),
                "<p>Привет, мир! 42</p>".as_bytes().to_vec(),
            ),
        ];
        let mut stats = ImageOptStats::default();
        subset_fonts_in(&mut entries, &mut stats).unwrap();

        assert_eq!(stats.fonts_subset, 1, "шрифт не был ужат");
        assert!(stats.fonts_after < stats.fonts_before);
        assert!(
            entries[0].1.len() < stats.fonts_before as usize / 10,
            "ужатие подозрительно слабое: {} из {}",
            entries[0].1.len(),
            stats.fonts_before
        );
        // Ужатый шрифт обязан остаться валидным OpenType с таблицей cmap.
        assert_eq!(&entries[0].1[..4], b"\x00\x01\x00\x00", "это уже не TTF");
        assert!(
            entries[0].1.windows(4).any(|w| w == b"cmap"),
            "в ужатом шрифте нет таблицы cmap — читалка не отобразит текст"
        );
    }

    #[test]
    fn prepare_drops_ncx_minifies_and_cleans_opf() {
        let dir = std::env::temp_dir().join(format!("lib_gui_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.epub");
        sample_epub(&src);

        let (entries, stats) = prepare_entries(&src, &ImageOptConfig::default()).unwrap();
        assert_eq!(stats.entries_dropped, 1, "toc.ncx должен быть удалён");
        assert!(
            !entries.iter().any(|(n, _)| n.ends_with(".ncx")),
            "запись toc.ncx осталась"
        );
        let opf = entries
            .iter()
            .find(|(n, _)| n == "OEBPS/content.opf")
            .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
            .unwrap();
        assert!(!opf.contains("toc.ncx"), "ссылка на toc.ncx осталась: {opf}");
        assert!(!opf.contains("toc=\"ncx\""), "атрибут toc остался: {opf}");
        assert!(!opf.contains("calibre"), "метаданные Calibre остались");
        assert!(opf.contains("<spine>"), "spine испорчен: {opf}");
        assert!(stats.text_after < stats.text_before);

        let xhtml = entries
            .iter()
            .find(|(n, _)| n == "OEBPS/ch1.xhtml")
            .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
            .unwrap();
        assert!(!xhtml.contains("<!--"), "комментарий остался");
        assert!(xhtml.contains("  отступ   значим  "), "pre испорчен");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pack_epub_puts_mimetype_first_and_stored() {
        let dir = std::env::temp_dir().join(format!("lib_gui_pack_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Данные должны быть сжимаемыми: на трёх байтах Deflate только
        // добавляет служебные байты и «экономия» получается отрицательной.
        let text = "<p>повторяющийся текст</p>".repeat(400);
        let entries = vec![
            ("OEBPS/ch1.xhtml".to_string(), text.into_bytes()),
            ("mimetype".to_string(), b"application/epub+zip".to_vec()),
        ];
        let out = dir.join("out.epub");
        pack_epub(&entries, &out, false, 9).unwrap();

        let file = File::open(&out).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();
        assert_eq!(zip.by_index(0).unwrap().name(), "mimetype");
        assert_eq!(
            zip.by_index(0).unwrap().compression(),
            CompressionMethod::Stored
        );
        assert_eq!(
            zip.by_index(1).unwrap().compression(),
            CompressionMethod::Deflated
        );

        let stored = dir.join("stored.epub");
        pack_epub(&entries, &stored, true, 9).unwrap();
        let file = File::open(&stored).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();
        assert_eq!(
            zip.by_index(1).unwrap().compression(),
            CompressionMethod::Stored
        );
        assert!(
            std::fs::metadata(&stored).unwrap().len() > std::fs::metadata(&out).unwrap().len(),
            "Stored-вариант должен быть больше: его сжимает уже сам .arx"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Замер: сколько стека реально нужно кодировщику WebP/AVIF в тех же
    /// условиях, что и при импорте (вложенный `par_iter` внутри пула).
    #[test]
    #[ignore]
    fn repro_full_pipeline_small_stack() {
        let stack = std::env::var("REPRO_STACK")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(2 * 1024 * 1024);
        let name = std::env::var("REPRO_BOOK").unwrap_or_else(|_| "821890".to_string());
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .stack_size(stack)
            .build()
            .unwrap();
        pool.install(|| {
            let src = format!("/tmp/conv/{name}.epub");
            let out = format!("/tmp/repro_{name}.epub");
            let s = optimize_epub(
                Path::new(&src),
                Path::new(&out),
                &ImageOptConfig::default(),
            )
            .unwrap();
            println!(
                "стек {} КиБ, книга {name}: картинок {}, {} → {} Б",
                stack / 1024,
                s.images,
                s.bytes_before,
                s.bytes_after
            );
        });
    }

    /// Проверка главного обещания: читается ли текст после сжатия. Для каждой
    /// картинки измеряем толщину штриха **до** и **после** кодирования — на
    /// уже декодированном результате.
    ///
    ///     cargo test --release -- --ignored img_stroke --nocapture
    #[test]
    #[ignore]
    fn img_stroke() {
        let mut all: Vec<(String, Vec<u8>)> = Vec::new();
        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir("/tmp/conv")
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        dirs.sort();
        for p in dirs {
            if p.extension().is_some_and(|e| e == "epub") {
                if let Ok(entries) = read_entries(&p) {
                    for (n, d) in entries {
                        if is_raster_entry(&n, &d) {
                            all.push((n, d));
                        }
                    }
                }
            }
        }
        if all.is_empty() {
            eprintln!("нет /tmp/conv/*.epub — тест пропущен");
            return;
        }
        let cfg = ImageOptConfig::default();
        // (штрих до, штрих после, доля масштаба, имя)
        let mut graphics: Vec<(f64, f64, f64, String)> = Vec::new();
        let mut photos = 0usize;
        for (n, d) in &all {
            let Ok(img) = image::load_from_memory(d) else {
                continue;
            };
            let rgba = img.to_rgba8();
            let before = graphic_features(&rgba).1;
            let Some(enc) = encode_image(n, d, &cfg, false).ok().flatten() else {
                continue;
            };
            if !is_graphic_features(
                graphic_features(&rgba).0,
                graphic_features(&rgba).2,
                graphic_features(&rgba).3,
            ) {
                photos += 1;
                continue;
            }
            let after = decoded_stroke(&enc.data);
            let scale = enc.w as f64 / rgba.width() as f64;
            graphics.push((before, after, scale, n.clone()));
        }
        graphics.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let bad = graphics.iter().filter(|g| g.1 < 1.5).count();
        let warn = graphics.iter().filter(|g| g.1 >= 1.5 && g.1 < 2.0).count();
        let good = graphics.iter().filter(|g| g.1 >= 2.0).count();
        println!(
            "\nграфических {} из {} (фото {photos})",
            graphics.len(),
            graphics.len() + photos
        );
        println!("  после сжатия штрих >= 2.0 px: {good}");
        println!("  1.5..2.0 (на грани):          {warn}");
        println!("  < 1.5 (плохо):                {bad}");
        println!("\nхудшие 12:");
        for (b, a, sc, n) in graphics.iter().take(12) {
            println!("  штрих {b:>4.1} → {a:>4.1} px, масштаб {sc:.2}  {n}");
        }
    }

    /// Статистика картинок: где текст, а где фото. Ключевая величина —
    /// **медианная толщина штриха**: текст читается, пока штрих после
    /// уменьшения остаётся хотя бы ~2 пикселя.
    ///
    /// Прозрачные PNG раскладываются на белый фон, иначе RGB под альфой = 0
    /// и картинка выглядит чёрным квадратом.
    ///
    ///     cargo test --release -- --ignored img_stats --nocapture
    #[test]
    #[ignore]
    fn img_stats() {
        fn stats(img: &image::DynamicImage) -> (f64, f64, f64, f64, f64, f64, (u32, u32)) {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let step = ((w.max(h) / 512).max(1)) as usize;
            let (mut paper, mut ink, mut sat, mut tot, mut edges) = (0u32, 0u32, 0u32, 0u32, 0u32);
            let mut runs: Vec<u32> = Vec::new();
            let luma = |p: &image::Rgba<u8>| -> u8 {
                let a = p[3] as u32;
                let mix = |c: u8| -> u32 { (c as u32 * a + 255 * (255 - a)) / 255 };
                ((mix(p[0]) * 299 + mix(p[1]) * 587 + mix(p[2]) * 114) / 1000) as u8
            };
            for y in (0..h).step_by(step) {
                let mut left: Option<u8> = None;
                let mut run = 0u32;
                for x in (0..w).step_by(step) {
                    let p = rgba.get_pixel(x, y);
                    let l = luma(p);
                    if l > 200 {
                        paper += 1;
                    }
                    if l < 80 {
                        ink += 1;
                    }
                    let mx = p[0].max(p[1]).max(p[2]);
                    let mn = p[0].min(p[1]).min(p[2]);
                    sat += (mx - mn) as u32;
                    if let Some(pl) = left {
                        if (l as i32 - pl as i32).unsigned_abs() > 32 {
                            edges += 1;
                        }
                    }
                    left = Some(l);
                    // длины горизонтальных «чернильных» штрихов
                    if l < 128 {
                        run += 1;
                    } else if run > 0 {
                        if run <= 32 {
                            runs.push(run);
                        }
                        run = 0;
                    }
                    tot += 1;
                }
                if run > 0 && run <= 32 {
                    runs.push(run);
                }
            }
            runs.sort_unstable();
            let med = if runs.is_empty() {
                0.0
            } else {
                runs[runs.len() / 2] as f64
            };
            let t = tot.max(1) as f64;
            (
                edges as f64 / t,
                paper as f64 / t,
                ink as f64 / t,
                sat as f64 / t,
                med,
                runs.len() as f64 / t * 1000.0,
                (w, h),
            )
        }

        let mut all: Vec<(String, Vec<u8>)> = Vec::new();
        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir("/tmp/conv")
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        dirs.sort();
        for p in dirs {
            if p.extension().is_some_and(|e| e == "epub") {
                if let Ok(entries) = read_entries(&p) {
                    for (n, d) in entries {
                        if is_raster_entry(&n, &d) {
                            all.push((n, d));
                        }
                    }
                }
            }
        }
        if all.is_empty() {
            eprintln!("нет /tmp/conv/*.epub — тест пропущен");
            return;
        }
        // (штрих, штрихов/1000, края, бумага, насыщ, имя, размеры)
        type Row = (f64, f64, f64, f64, f64, String, (u32, u32));
        let mut rows: Vec<Row> = Vec::new();
        for (n, d) in &all {
            let Ok(img) = image::load_from_memory(d) else {
                continue;
            };
            let (e, paper, _ink, sat, med, per1k, dim) = stats(&img);
            rows.push((med, per1k, e, paper, sat, n.clone(), dim));
        }
        rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        println!("\nвсего {} картинок", rows.len());
        println!("\nбольше всего тонких штрихов (кандидаты в «текст»):");
        for r in rows.iter().take(12) {
            println!(
                "  штрих {:>4.0} px, {:>6.1}/1000, края {:>5.3}, бумага {:>5.2}, насыщ {:>5.1}, {:>4}x{:<4}  {}",
                r.0, r.1, r.2, r.3, r.4, r.6 .0, r.6 .1, r.5
            );
        }
        println!("\nменьше всего (кандидаты в «фото»):");
        for r in rows.iter().rev().take(8) {
            println!(
                "  штрих {:>4.0} px, {:>6.1}/1000, края {:>5.3}, бумага {:>5.2}, насыщ {:>5.1}, {:>4}x{:<4}  {}",
                r.0, r.1, r.2, r.3, r.4, r.6 .0, r.6 .1, r.5
            );
        }
        let tot_bytes: usize = all.iter().map(|(_, d)| d.len()).sum();
        println!("\n--- правила (доля по исходным байтам) ---");
        type Pred = Box<dyn Fn(&Row) -> bool>;
        let rules: Vec<(&str, Pred)> = vec![
            ("серое (насыщ<30)", Box::new(|r: &Row| r.4 < 30.0)),
            (
                "серое + штрихи>15/1000",
                Box::new(|r: &Row| r.4 < 30.0 && r.1 > 15.0),
            ),
            (
                "серое + штрихи>15 + бумага>0.3",
                Box::new(|r: &Row| r.4 < 30.0 && r.1 > 15.0 && r.3 > 0.3),
            ),
            (
                "штрих<=2px + штрихи>15/1000",
                Box::new(|r: &Row| r.0 <= 2.0 && r.1 > 15.0),
            ),
            (
                "края>0.10 + насыщ<30",
                Box::new(|r: &Row| r.2 > 0.10 && r.4 < 30.0),
            ),
            (
                "края>0.08",
                Box::new(|r: &Row| r.2 > 0.08),
            ),
        ];
        let mut by_name: std::collections::HashMap<&str, usize> = Default::default();
        for (n, d) in &all {
            by_name.insert(n.as_str(), d.len());
        }
        for (name, f) in &rules {
            let sel: Vec<&Row> = rows.iter().filter(|r| f(r)).collect();
            let b: usize = sel.iter().map(|r| by_name[r.5.as_str()]).sum();
            println!(
                "  {name:<34} {:>3} шт ({:>4.0} %), {:>9} Б ({:>4.0} % исходных)",
                sel.len(),
                sel.len() as f64 / rows.len() as f64 * 100.0,
                b,
                b as f64 / tot_bytes as f64 * 100.0
            );
        }
        // сколько «серых» картинок имеют тонкий штрих
        let gray: Vec<&Row> = rows.iter().filter(|r| r.4 < 30.0).collect();
        println!("\nиз {} серых картинок:", gray.len());
        for lim in [5.0f64, 10.0, 15.0, 25.0] {
            let n = gray.iter().filter(|r| r.1 > lim).count();
            println!("  штрихов > {lim:>4.0}/1000: {n:>3} шт");
        }
        let med_med = {
            let mut v: Vec<f64> = gray.iter().map(|r| r.0).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v.get(v.len() / 2).copied().unwrap_or(0.0)
        };
        println!("  медиана толщины штриха у серых: {med_med:.0} px");
    }

    /// Ищет **похожие** картинки (не байтовые дубликаты): перцептивный хеш по
    /// уменьшенной до 8×8 серой копии, расстояние Хэмминга. Для каждой группы
    /// показывает, сколько разных результатов даёт наш пайплайн и сколько
    /// байт теряется на повторах.
    ///
    ///     cargo test --release -- --ignored img_neardup --nocapture
    #[test]
    #[ignore]
    fn img_neardup() {
        fn dhash(img: &image::DynamicImage) -> u64 {
            let g = img.to_luma8();
            let small = image::imageops::resize(&g, 9, 8, FilterType::Triangle);
            let mut h = 0u64;
            let mut bit = 0;
            for y in 0..8u32 {
                for x in 0..8u32 {
                    if small.get_pixel(x, y)[0] < small.get_pixel(x + 1, y)[0] {
                        h |= 1 << bit;
                    }
                    bit += 1;
                }
            }
            h
        }

        let mut all: Vec<(String, Vec<u8>)> = Vec::new();
        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir("/tmp/conv")
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        dirs.sort();
        for p in dirs {
            if p.extension().is_some_and(|e| e == "epub") {
                if let Ok(entries) = read_entries(&p) {
                    for (n, d) in entries {
                        if is_raster_entry(&n, &d) {
                            all.push((n, d));
                        }
                    }
                }
            }
        }
        if all.is_empty() {
            eprintln!("нет /tmp/conv/*.epub — тест пропущен");
            return;
        }

        let cfg = ImageOptConfig::default();
        // (хеш, книга, имя, размер после пайплайна, байты после пайплайна, исходный размер)
        // (dHash, книга, имя, размер после пайплайна, байты, размер исходника)
        type Row = (u64, String, String, usize, Option<Vec<u8>>, usize);
        let mut info: Vec<Row> = Vec::new();
        for (n, d) in &all {
            let Ok(img) = image::load_from_memory(d) else {
                continue;
            };
            let h = dhash(&img);
            let enc = encode_image(n, d, &cfg, false).ok().flatten().map(|e| e.data);
            let sz = enc.as_ref().map_or(0, |e| e.len());
            let book = n.split(':').next().unwrap_or("").to_string();
            info.push((h, book, n.clone(), sz, enc, d.len()));
        }

        let mut used = vec![false; info.len()];
        let mut within = (0usize, 0usize, 0usize); // групп, всего Б, можно сэкономить
        let mut across = (0usize, 0usize, 0usize);
        let mut already = 0usize;
        for i in 0..info.len() {
            if used[i] {
                continue;
            }
            let mut g = vec![i];
            used[i] = true;
            for j in (i + 1)..info.len() {
                if !used[j] && (info[i].0 ^ info[j].0).count_ones() <= 5 {
                    used[j] = true;
                    g.push(j);
                }
            }
            if g.len() < 2 {
                continue;
            }
            // разные ли книги в группе
            let books: std::collections::HashSet<&str> =
                g.iter().map(|&i| info[i].1.as_str()).collect();
            // сколько уже совпало побайтово
            let mut uniq: Vec<&Vec<u8>> = Vec::new();
            for &i in g.iter() {
                if let Some(e) = &info[i].4 {
                    if !uniq.contains(&e) {
                        uniq.push(e);
                    }
                }
            }
            already += g.len() - uniq.len();
            // если свести группу к одной кодировке — сколько останется
            let total: usize = g.iter().map(|&i| info[i].3).sum();
            let canonical = g.iter().map(|&i| info[i].3).min().unwrap_or(0);
            let save = total.saturating_sub(canonical);
            let slot = if books.len() == 1 { &mut within } else { &mut across };
            slot.0 += 1;
            slot.1 += total;
            slot.2 += save;
        }
        println!("\nвсего картинок: {}", info.len());
        println!("уже совпало побайтово (arx схлопывает даром): {already} шт");
        println!(
            "группы ВНУТРИ одной книги: {} групп, {} Б, свести к одной = сэкономить {} Б",
            within.0, within.1, within.2
        );
        println!(
            "группы МЕЖДУ книгами:      {} групп, {} Б, свести к одной = сэкономить {} Б",
            across.0, across.1, across.2
        );

        // Проверяем группы пиксельно: настоящие дубликаты после приведения к
        // 32×32 должны совпадать почти точно (низкий MSE), «просто похожие» —
        // нет. Считаем ещё и разброс яркости: у пустых/однотонных страниц он
        // около нуля, и они липнут в одну группу ложно.
        let mut small: Vec<Vec<u8>> = Vec::new();
        for (_, _, n, _, _, _) in &info {
            let d = &all.iter().find(|(x, _)| *x == *n).unwrap().1;
            let g = image::load_from_memory(d)
                .map(|i| i.to_luma8())
                .ok();
            let v = g.map(|g| {
                image::imageops::resize(&g, 32, 32, FilterType::Triangle)
                    .into_raw()
            });
            small.push(v.unwrap_or_default());
        }
        let mse = |a: &[u8], b: &[u8]| -> f64 {
            if a.is_empty() || b.is_empty() {
                return f64::NAN;
            }
            a.iter()
                .zip(b)
                .map(|(x, y)| {
                    let d = *x as f64 - *y as f64;
                    d * d
                })
                .sum::<f64>()
                / a.len() as f64
        };
        let mut used2 = vec![false; info.len()];
        let mut groups2: Vec<Vec<usize>> = Vec::new();
        for i in 0..info.len() {
            if used2[i] {
                continue;
            }
            let mut g = vec![i];
            used2[i] = true;
            for j in (i + 1)..info.len() {
                if !used2[j] && (info[i].0 ^ info[j].0).count_ones() <= 5 {
                    used2[j] = true;
                    g.push(j);
                }
            }
            if g.len() > 1 {
                groups2.push(g);
            }
        }
        let flat = |v: &[u8]| -> f64 {
            if v.is_empty() {
                return f64::NAN;
            }
            let m = v.iter().map(|x| *x as f64).sum::<f64>() / v.len() as f64;
            (v.iter().map(|x| (*x as f64 - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
        };
        // Для каждой группы считаем: средний MSE между членами и разброс
        // яркости (пустые страницы липнут в группы ложно — у них разброс ~0).
        let group_stats = |g: &Vec<usize>| -> (f64, f64) {
            let mut sum = 0.0;
            let mut cnt = 0;
            for a in 0..g.len() {
                for b in (a + 1)..g.len() {
                    let m = mse(&small[g[a]], &small[g[b]]);
                    if m.is_finite() {
                        sum += m;
                        cnt += 1;
                    }
                }
            }
            let m = if cnt > 0 { sum / cnt as f64 } else { f64::NAN };
            let sd = g.iter().map(|&i| flat(&small[i])).sum::<f64>() / g.len() as f64;
            (m, sd)
        };

        let mut report: Vec<(usize, f64, f64, usize)> = groups2
            .iter()
            .map(|g| {
                let (m, sd) = group_stats(g);
                let save = g.iter().map(|&i| info[i].3).sum::<usize>()
                    - g.iter().map(|&i| info[i].3).min().unwrap_or(0);
                (g.len(), m, sd, save)
            })
            .collect();
        report.sort_by_key(|r| std::cmp::Reverse(r.3));
        println!("\nразбор {} групп (топ-12 по потерям):", groups2.len());
        for (n, m, sd, save) in report.iter().take(12) {
            println!("  {n:>3} шт, MSE {m:>8.1}, разброс яркости {sd:>6.1}, теряем {save:>7} Б");
        }

        // Настоящие дубликаты: сколько arx уже схлопывает сам (наши выходы
        // совпали побайтово) и сколько осталось бы на перцептивную дедупликацию.
        println!("\n--- настоящие дубликаты (разброс яркости >= 3) ---");
        for lim in [50.0f64, 10.0, 2.0] {
            let (mut n, mut al, mut left, mut tot) = (0usize, 0usize, 0usize, 0usize);
            for g in &groups2 {
                let (m, sd) = group_stats(g);
                if !(sd >= 3.0 && m < lim) {
                    continue;
                }
                let mut uniq: Vec<&Vec<u8>> = Vec::new();
                for &i in g {
                    if let Some(e) = &info[i].4 {
                        if !uniq.contains(&e) {
                            uniq.push(e);
                        }
                    }
                }
                let total: usize = g.iter().map(|&i| info[i].3).sum();
                let uniq_b: usize = uniq.iter().map(|u| u.len()).sum();
                let min = g.iter().map(|&i| info[i].3).min().unwrap_or(0);
                n += 1;
                tot += total;
                al += total - uniq_b;
                left += uniq_b - min;
            }
            println!(
                "  MSE < {lim:>5}: групп {n:>2}, всего {tot:>8} Б, arx уже схлопнул {al:>7} Б, осталось {left:>7} Б"
            );
        }
    }

    /// Собирает `/tmp/imgcmp/index.html` — один и тот же кадр при разных
    /// настройках, чтобы глазами выбрать компромисс «размер/качество».
    ///
    ///     cargo test --release -- --ignored img_compare --nocapture
    #[test]
    #[ignore]
    fn img_compare() {
        fn b64(data: &[u8]) -> String {
            const T: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
            for c in data.chunks(3) {
                let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
                let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
                out.push(T[(n >> 18 & 63) as usize] as char);
                out.push(T[(n >> 12 & 63) as usize] as char);
                out.push(if c.len() > 1 {
                    T[(n >> 6 & 63) as usize] as char
                } else {
                    '='
                });
                out.push(if c.len() > 2 {
                    T[(n & 63) as usize] as char
                } else {
                    '='
                });
            }
            out
        }

        let mut all: Vec<(String, Vec<u8>)> = Vec::new();
        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir("/tmp/conv")
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        dirs.sort();
        for p in dirs {
            if p.extension().is_some_and(|e| e == "epub") {
                if let Ok(entries) = read_entries(&p) {
                    for (n, d) in entries {
                        if is_raster_entry(&n, &d) {
                            all.push((n, d));
                        }
                    }
                }
            }
        }
        if all.is_empty() {
            eprintln!("нет /tmp/conv/*.epub — тест пропущен");
            return;
        }

        // Берём по классам: отдельно «графику» (текст, схемы) и отдельно
        // фотографии — именно на графике видно, когда текст перестаёт читаться.
        let mut graphics: Vec<&(String, Vec<u8>)> = Vec::new();
        let mut photos: Vec<&(String, Vec<u8>)> = Vec::new();
        for it in &all {
            if it.1.len() >= 400_000 {
                continue;
            }
            let Ok(img) = image::load_from_memory(&it.1) else {
                continue;
            };
            let (per_1k, _st, paper, sat) = graphic_features(&img.to_rgba8());
            if is_graphic_features(per_1k, paper, sat) {
                graphics.push(it);
            } else {
                photos.push(it);
            }
        }
        graphics.sort_by_key(|(_, d)| std::cmp::Reverse(d.len()));
        photos.sort_by_key(|(_, d)| std::cmp::Reverse(d.len()));
        let mut picked: Vec<(&(String, Vec<u8>), bool)> = Vec::new();
        for (set, is_g) in [(&graphics, true), (&photos, false)] {
            for k in [0, set.len() / 3, set.len() * 2 / 3] {
                if let Some(x) = set.get(k) {
                    if !picked.iter().any(|(p, _)| p.0 == x.0) {
                        picked.push((x, is_g));
                    }
                }
            }
        }
        println!(
            "в корпусе: {} графики, {} фото; на странице {} кадров",
            graphics.len(),
            photos.len(),
            picked.len()
        );

        let base = ImageOptConfig::default();
        let mut v: Vec<(String, ImageOptConfig)> = Vec::new();
        let mut add = |name: &str, f: &dyn Fn(&mut ImageOptConfig)| {
            let mut c = base.clone();
            f(&mut c);
            v.push((name.to_string(), c));
        };
        // Прежний режим — тот, на котором картинки стали выглядеть хуже.
        let base_old = |c: &mut ImageOptConfig| {
            c.quality = 30.0;
            c.shrink_percent = 50.0;
            c.quality_small = 45.0;
            c.min_side = 200;
            c.text_quality = 36.0;
            c.text_shrink_percent = 50.0;
            c.text_min_stroke = 1.5;
        };
        let text_d = |c: &mut ImageOptConfig, q: f32, shrink: f64, stroke: f64| {
            c.text_quality = q;
            c.text_shrink_percent = shrink;
            c.text_min_stroke = stroke;
        };
        add("БЫЛО (что не понравилось)", &|c| base_old(c));
        add("A: q40, −35 %, штрих 2.0", &|c| {
            c.quality = 40.0;
            c.shrink_percent = 35.0;
            c.quality_small = 48.0;
            c.min_side = 240;
            text_d(c, 50.0, 10.0, 2.0);
        });
        add("B: фото q35, −45 %", &|c| {
            c.quality = 35.0;
            c.shrink_percent = 45.0;
            c.quality_small = 48.0;
            c.min_side = 240;
            text_d(c, 50.0, 10.0, 2.0);
        });
        add("C: фото q32, −50 %", &|c| {
            c.quality = 32.0;
            c.shrink_percent = 50.0;
            c.quality_small = 48.0;
            c.min_side = 240;
            text_d(c, 50.0, 10.0, 2.0);
        });
        add("D: B + текст q45, −25 %, 1.8", &|c| {
            c.quality = 35.0;
            c.shrink_percent = 45.0;
            c.quality_small = 48.0;
            c.min_side = 240;
            text_d(c, 45.0, 25.0, 1.8);
        });
        let dir = std::path::Path::new("/tmp/imgcmp");
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();

        // Каждый кадр встраивается в страницу один раз: data-URI лежат в
        // объекте `D`, а <img data-k=...> получают src скриптом.
        let mut data = String::new();
        let mut html = String::from(
            "<!doctype html><meta charset=utf-8><title>Сжатие картинок: варианты</title>\
             <style>body{background:#1b1b1f;color:#e6e6e6;font:14px/1.4 system-ui,sans-serif;margin:24px}\
             table{border-collapse:collapse}td,th{border:1px solid #3a3a44;padding:8px;vertical-align:middle;text-align:center}\
             img{display:block;background:#000}\
             .zoom{overflow:hidden;width:200px;height:200px;position:relative;background:#000}\
             .zoom img{image-rendering:pixelated;transform:scale(2);transform-origin:0 0}\
             .sz{color:#8fd694;font-weight:600}.src{color:#888;font-size:12px}\
             .cls{color:#ffc857}\
             h2{margin-top:36px;margin-bottom:4px}</style>\
             <h1>Сжатие картинок: что даёт каждый шаг</h1>\
             <p>Слева — исходник, дальше варианты. Верхний ряд — как есть, нижний — увеличение 2×, \
             на нём видны артефакты. Размер указан для этой картинки. На картинках с текстом \
             смотрите нижний ряд: если буквы расплылись в серое — режим не годится.</p>",
        );

        for (idx, (it, is_graphic)) in picked.iter().enumerate() {
            let (name, src) = (it.0.as_str(), it.1.as_slice());
            let short = name.rsplit('/').next().unwrap_or(name);
            let (sw, sh) = image::load_from_memory(src)
                .map(|i| (i.width(), i.height()))
                .unwrap_or((0, 0));
            let (per_1k, stroke, paper, sat) = image::load_from_memory(src)
                .map(|i| graphic_features(&i.to_rgba8()))
                .unwrap_or((0.0, 0.0, 0.0, 0.0));
            let cls = if *is_graphic {
                "ГРАФИКА (текст/схема)"
            } else {
                "фотография"
            };
            html.push_str(&format!(
                "<h2>{short} — <span class=cls>{cls}</span></h2>\
                 <p class=src>исходник {} Б, {sw}×{sh}; штрих {stroke:.0} px, \
                 штрихов {per_1k:.0}/1000, фон {paper:.2}, насыщенность {sat:.0}</p>",
                src.len()
            ));

            let orig_mime = if short.to_lowercase().ends_with(".png") {
                "image/png"
            } else {
                "image/jpeg"
            };
            let key = format!("k{idx}_orig");
            data.push_str(&format!("'{key}':'data:{orig_mime};base64,{}',", b64(src)));
            let mut cells: Vec<(String, String, String)> = vec![(
                "исходник".into(),
                key,
                format!("{} Б", src.len()),
            )];

            for (vi, (vname, cfg)) in v.iter().enumerate() {
                if let Ok(Some(e)) = encode_image(name, src, cfg, false) {
                    let mime = match e.format {
                        Format::Avif => "image/avif",
                        Format::WebP => "image/webp",
                    };
                    let key = format!("k{idx}_{vi}");
                    data.push_str(&format!(
                        "'{key}':'data:{mime};base64,{}',",
                        b64(&e.data)
                    ));
                    cells.push((
                        vname.clone(),
                        key,
                        format!("{} Б, {}×{}", e.data.len(), e.w, e.h),
                    ));
                }
            }

            html.push_str("<table><tr><th>вариант</th>");
            for (vname, _, _) in &cells {
                html.push_str(&format!("<th>{vname}</th>"));
            }
            html.push_str("</tr><tr><td>как есть</td>");
            for (_, key, _) in &cells {
                html.push_str(&format!("<td><img data-k=\"{key}\"></td>"));
            }
            html.push_str("</tr><tr><td>2×</td>");
            for (_, key, _) in &cells {
                html.push_str(&format!("<td><div class=zoom><img data-k=\"{key}\"></div></td>"));
            }
            html.push_str("</tr><tr><td>размер</td>");
            for (_, _, size) in &cells {
                html.push_str(&format!("<td class=sz>{size}</td>"));
            }
            html.push_str("</tr></table>");
        }

        html.push_str("<script>const D={");
        html.push_str(&data);
        html.push_str(
            "};for(const i of document.querySelectorAll('img[data-k]'))i.src=D[i.dataset.k];</script>",
        );

        let path = dir.join("index.html");
        std::fs::write(&path, &html).unwrap();
        println!("готово: {} ({} КБ)", path.display(), html.len() / 1024);
    }

    /// Стенд подбора параметров сжатия картинок: один и тот же набор
    /// картинок прогоняется через разные конфиги, печатается суммарный размер.
    /// Нужны исходные (до оптимизации) EPUB в `/tmp/conv`.
    ///
    ///     cargo test --release -- --ignored img_sweep --nocapture
    #[test]
    #[ignore]
    fn img_sweep() {
        let mut all: Vec<(String, Vec<u8>)> = Vec::new();
        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir("/tmp/conv")
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        dirs.sort();
        for p in dirs {
            if p.extension().is_some_and(|e| e == "epub") {
                if let Ok(entries) = read_entries(&p) {
                    for (n, d) in entries {
                        if is_raster_entry(&n, &d) {
                            all.push((n, d));
                        }
                    }
                }
            }
        }
        if all.is_empty() {
            eprintln!("нет /tmp/conv/*.epub — стенд пропущен");
            return;
        }

        let base = ImageOptConfig::default();
        let mut v: Vec<(String, ImageOptConfig)> = Vec::new();
        let mut add = |name: &str, f: &dyn Fn(&mut ImageOptConfig)| {
            let mut c = base.clone();
            f(&mut c);
            v.push((name.to_string(), c));
        };
        add("00 базовая (q35, −35 %, speed 8)", &|_| {});
        add("01 shrink 45 %", &|c| c.shrink_percent = 45.0);
        add("02 shrink 50 %", &|c| c.shrink_percent = 50.0);
        add("03 q30 + shrink 45 %", &|c| {
            c.quality = 30.0;
            c.quality_small = 45.0;
            c.cover_quality = 45.0;
            c.shrink_percent = 45.0;
        });
        add("04 q30 + shrink 50 %", &|c| {
            c.quality = 30.0;
            c.quality_small = 45.0;
            c.cover_quality = 45.0;
            c.shrink_percent = 50.0;
        });
        add("05 shrink 45 % + speed 4", &|c| {
            c.shrink_percent = 45.0;
            c.avif_speed = 4;
        });
        add("06 shrink 50 % + speed 4", &|c| {
            c.shrink_percent = 50.0;
            c.avif_speed = 4;
        });
        add("07 q30 + shrink 45 % + speed 4", &|c| {
            c.quality = 30.0;
            c.quality_small = 45.0;
            c.cover_quality = 45.0;
            c.shrink_percent = 45.0;
            c.avif_speed = 4;
        });
        add("08 q32 + shrink 40 % + speed 4", &|c| {
            c.quality = 32.0;
            c.quality_small = 46.0;
            c.cover_quality = 46.0;
            c.shrink_percent = 40.0;
            c.avif_speed = 4;
        });
        add("09 q30 + shrink 45 %, min_side 250", &|c| {
            c.quality = 30.0;
            c.quality_small = 45.0;
            c.cover_quality = 45.0;
            c.shrink_percent = 45.0;
            c.min_side = 250;
        });
        add("10 max 480x720 + q30 + speed 4", &|c| {
            c.max_w = 480;
            c.max_h = 720;
            c.quality = 30.0;
            c.quality_small = 45.0;
            c.cover_quality = 45.0;
            c.avif_speed = 4;
        });

        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        let chunk = all.len().div_ceil(threads);
        let started = std::time::Instant::now();
        let per_thread: Vec<Vec<(usize, usize)>> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for part in all.chunks(chunk) {
                let v = &v;
                handles.push(
                    std::thread::Builder::new()
                        .stack_size(16 * 1024 * 1024)
                        .spawn_scoped(scope, move || {
                            let mut acc = vec![(0usize, 0usize); v.len()];
                            for (name, data) in part {
                                for (i, (_, cfg)) in v.iter().enumerate() {
                                    if let Ok(Some(e)) = encode_image(name, data, cfg, false) {
                                        acc[i].0 += e.data.len();
                                        if e.format == Format::Avif {
                                            acc[i].1 += 1;
                                        }
                                    }
                                }
                            }
                            acc
                        })
                        .unwrap(),
                );
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut totals = vec![(0usize, 0usize); v.len()];
        for acc in per_thread {
            for (i, (b, a)) in acc.into_iter().enumerate() {
                totals[i].0 += b;
                totals[i].1 += a;
            }
        }

        let first = totals[0].0 as f64;
        println!("\nкартинок {} за {:?}\n", all.len(), started.elapsed());
        println!("{:<28} {:>10} {:>8}  AVIF/WebP", "вариант", "байт", "к базе");
        for (i, (name, _)) in v.iter().enumerate() {
            let (bytes, avif) = totals[i];
            let delta = (bytes as f64 / first - 1.0) * 100.0;
            println!(
                "{:<28} {:>10} {:>7.1}%  {}/{}",
                name,
                bytes,
                delta,
                avif,
                all.len() - avif
            );
        }
    }

    /// Диагностика переполнения стека: те же данные, что и в импорте, но на
    /// заведомо маленьком стеке. Требует распакованных EPUB в `/tmp/conv`
    /// (`--convert` по всем книгам), поэтому помечен `#[ignore]`.
    ///
    ///     REPRO_STACK=1048576 cargo test --release -- --ignored stack_regression --nocapture
    #[test]
    #[ignore]
    fn stack_regression() {
        let stack = std::env::var("REPRO_STACK")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1024 * 1024);
        let mut books: Vec<std::path::PathBuf> = std::fs::read_dir("/tmp/conv")
            .map(|d| {
                d.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|e| e == "epub"))
                    .collect()
            })
            .unwrap_or_default();
        if books.is_empty() {
            eprintln!("нет /tmp/conv/*.epub — тест пропущен");
            return;
        }
        books.sort();
        for book in books {
            let stem = book.file_stem().unwrap().to_string_lossy().into_owned();
            let out = format!("/tmp/repro_{stem}.epub");
            let handle = std::thread::Builder::new()
                .stack_size(stack)
                .spawn(move || {
                    let s =
                        optimize_epub(&book, Path::new(&out), &ImageOptConfig::default()).unwrap();
                    println!("{stem}: {} картинок", s.images);
                })
                .unwrap();
            handle.join().unwrap();
        }
    }

    #[test]
    fn pack_epub_writes_fixed_timestamp() {
        let dir = std::env::temp_dir().join(format!("lib_gui_stamp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entries = vec![
            ("mimetype".to_string(), b"application/epub+zip".to_vec()),
            ("OEBPS/ch1.xhtml".to_string(), b"<p>x</p>".to_vec()),
        ];
        let out = dir.join("out.epub");
        pack_epub(&entries, &out, false, 9).unwrap();

        let file = File::open(&out).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();
        // Фиксированная метка (1980-01-01) — иначе байты EPUB меняются от
        // прогона к прогону, и `.arx` поверх них тоже.
        for i in 0..zip.len() {
            assert_eq!(
                zip.by_index(i).unwrap().last_modified(),
                Some(zip::DateTime::default()),
                "у записи {i} плавающая метка времени"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn benchmark_reports_each_stage() {
        let dir = std::env::temp_dir().join(format!("lib_gui_bench_{}", std::process::id()));
        let work = dir.join("work");
        let defl = dir.join("defl");
        let stor = dir.join("stor");
        std::fs::create_dir_all(&work).unwrap();
        let src = work.join("in.epub");
        sample_epub(&src);

        let r = benchmark_epub(&src, &defl, &stor, &work, &ImageOptConfig::default()).unwrap();
        assert!(r.base > 0);
        assert!(r.clean <= r.base, "минификация не должна увеличивать EPUB");
        assert!(r.minified <= r.base);
        assert!(r.fonts <= r.minified);
        assert!(defl.join("in.epub").exists());
        assert!(stor.join("in.epub").exists());
        assert_eq!(r.entries_dropped, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn webp_encode_rgb_matches_rgba_size() {
        let img = RgbaImage::from_pixel(40, 30, image::Rgba([90, 120, 60, 255]));
        let p = WebpParams::lossy(50.0, 4, 80, 70, 1, false);
        let rgb = encode_webp_rgb(&rgba_to_rgb(&img), 40, 30, &p).unwrap();
        let rgba = encode_webp(img.as_raw(), 40, 30, &p).unwrap();
        assert!(rgb.len() <= rgba.len(), "RGB {} > RGBA {}", rgb.len(), rgba.len());
        assert_eq!(sniff_image(&rgb), Some(("webp", "image/webp")));
    }
}

/// Один режим сжатия для замера.
#[cfg(test)]
struct BenchMode {
    label: &'static str,
    avif: bool,
    speed: u8,
    speed_graphic: u8,
    shrink: f64,
    max_w: u32,
    max_h: u32,
    method: i32,
    quality: f32,
}

/// Замер режимов сжатия на настоящих картинках корпуса.
///
/// `cargo test --release -- --ignored bench_settings --nocapture`
///
/// Отвечает на два вопроса сразу: сколько стоит каждая настройка и куда именно
/// уходит время (кодирование против декодирования с уменьшением).
#[test]
#[ignore]
fn bench_settings() {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    let dir = std::path::Path::new("/tmp/bench_imgs");
    if !dir.exists() {
        eprintln!("нет /tmp/bench_imgs — нечего мерить");
        return;
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    // Каждая четвёртая картинка: набор остаётся представительным, а прогон
    // укладывается в минуты.
    let imgs: Vec<(String, Vec<u8>)> = files
        .iter()
        .step_by(4)
        .map(|p| (p.to_string_lossy().to_string(), std::fs::read(p).unwrap()))
        .collect();
    let source_bytes: u64 = imgs.iter().map(|(_, d)| d.len() as u64).sum();
    println!(
        "картинок в наборе: {} (из {}), исходник {:.2} МБ",
        imgs.len(),
        files.len(),
        source_bytes as f64 / 1e6
    );

    let modes = [
        BenchMode { label: "СЕЙЧАС avif 2/2  -45% 600x900", avif: true, speed: 2, speed_graphic: 2, shrink: 45.0, max_w: 600, max_h: 900, method: 6, quality: 40.0 },
        BenchMode { label: "avif 6/4  -55% 600x900", avif: true, speed: 6, speed_graphic: 4, shrink: 55.0, max_w: 600, max_h: 900, method: 6, quality: 40.0 },
        BenchMode { label: "avif 8/6  -55% 600x900", avif: true, speed: 8, speed_graphic: 6, shrink: 55.0, max_w: 600, max_h: 900, method: 6, quality: 40.0 },
        BenchMode { label: "avif 10/8 -55% 600x900", avif: true, speed: 10, speed_graphic: 8, shrink: 55.0, max_w: 600, max_h: 900, method: 6, quality: 40.0 },
        BenchMode { label: "avif 10/8 -55% 540x810", avif: true, speed: 10, speed_graphic: 8, shrink: 55.0, max_w: 540, max_h: 810, method: 6, quality: 40.0 },
        BenchMode { label: "avif 10/8 -55% 480x720", avif: true, speed: 10, speed_graphic: 8, shrink: 55.0, max_w: 480, max_h: 720, method: 6, quality: 40.0 },
        BenchMode { label: "avif 8/6  -55% 480x720", avif: true, speed: 8, speed_graphic: 6, shrink: 55.0, max_w: 480, max_h: 720, method: 6, quality: 40.0 },
        BenchMode { label: "avif 10/8 -55% 480x720 q35", avif: true, speed: 10, speed_graphic: 8, shrink: 55.0, max_w: 480, max_h: 720, method: 6, quality: 35.0 },
        BenchMode { label: "webp m6 q40 -55% 600x900", avif: false, speed: 0, speed_graphic: 0, shrink: 55.0, max_w: 600, max_h: 900, method: 6, quality: 40.0 },
        BenchMode { label: "webp m4 q40 -55% 600x900", avif: false, speed: 0, speed_graphic: 0, shrink: 55.0, max_w: 600, max_h: 900, method: 4, quality: 40.0 },
        BenchMode { label: "webp m4 q50 -55% 600x900", avif: false, speed: 0, speed_graphic: 0, shrink: 55.0, max_w: 600, max_h: 900, method: 4, quality: 50.0 },
    ];

    let mut rows: Vec<(String, f64, u64, f64, usize)> = Vec::new();
    for m in &modes {
        let cfg = ImageOptConfig {
            avif: m.avif,
            avif_speed: m.speed,
            avif_speed_graphic: m.speed_graphic,
            shrink_percent: m.shrink,
            max_w: m.max_w,
            max_h: m.max_h,
            method: m.method,
            quality: m.quality,
            ..ImageOptConfig::default()
        };

        AVIF_CALLS.store(0, Ordering::Relaxed);
        AVIF_NS.store(0, Ordering::Relaxed);
        let t = Instant::now();
        let mut bytes = 0u64;
        let mut done = 0usize;
        let mut smallest = u64::MAX;
        for (n, d) in &imgs {
            if let Ok(Some(e)) = encode_image(n, d, &cfg, false) {
                bytes += e.data.len() as u64;
                done += 1;
                smallest = smallest.min(e.data.len() as u64);
            }
        }
        let total = t.elapsed().as_secs_f64();
        let enc = AVIF_NS.load(Ordering::Relaxed) as f64 / 1e9;
        let calls = AVIF_CALLS.load(Ordering::Relaxed);
        let _ = smallest;
        println!("  прогон: {:<28} {done} картинок", m.label);
        rows.push((m.label.to_string(), total, bytes, enc, calls));
    }

    let base_t = rows[0].1;
    let base_b = rows[0].2;
    println!(
        "\n{:<30} {:>8} {:>7} {:>11} {:>8} {:>12} {:>10}",
        "режим", "время", "ускор.", "байт", "размер", "на кодир.", "кодир./шт"
    );
    for (l, t, b, enc, calls) in &rows {
        let _ = (l, t, b, enc, calls);
    }
    for (l, t, b, enc, calls) in rows {
        println!(
            "{l:<30} {t:>7.1}с {:>6.2}x {b:>10} Б {:>+7.1}% {:>10.1}с {:>10.2}",
            base_t / t,
            (b as f64 / base_b as f64 - 1.0) * 100.0,
            enc,
            calls as f64 / imgs.len() as f64
        );
    }
    println!("\n«на кодир.» — наносекунды внутри ravif/WebP; остальное — декодирование и Lanczos3.");
}

/// Диагностика: масштабируется ли AVIF-энкодер по потокам.
/// `cargo test --release -- --ignored avif_parallel_scaling --nocapture`
#[test]
#[ignore]
fn avif_parallel_scaling() {
    use std::time::Instant;
    let n = 16usize;
    let (w, h) = (600u32, 900u32);
    // Синтетическая «фотография»: градиент + шум, чтобы кодек не схитрил.
    let imgs: Vec<Vec<u8>> = (0..n)
        .map(|k| {
            let mut v = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h {
                for x in 0..w {
                    let r = ((x * 255 / w) as u8).wrapping_add((k * 7) as u8);
                    let g = (y * 255 / h) as u8;
                    let b = ((x ^ y) as u8).wrapping_mul(3);
                    v.extend_from_slice(&[r, g, b, 255]);
                }
            }
            v
        })
        .collect();

    let t = Instant::now();
    for d in &imgs {
        let _ = encode_avif(d, w, h, ExtendedColorType::Rgba8, 40.0, 4).unwrap();
    }
    let seq = t.elapsed();

    let t = Instant::now();
    std::thread::scope(|s| {
        for d in &imgs {
            s.spawn(move || {
                let _ = encode_avif(d, w, h, ExtendedColorType::Rgba8, 40.0, 4).unwrap();
            });
        }
    });
    let par = t.elapsed();

    println!(
        "последовательно {n} шт: {:.2} с ({:.2} с/шт)\nв {n} потоков:      {:.2} с\nускорение: {:.2}x",
        seq.as_secs_f64(),
        seq.as_secs_f64() / n as f64,
        par.as_secs_f64(),
        seq.as_secs_f64() / par.as_secs_f64()
    );
}

/// Страница сравнения: как выглядит одна и та же картинка при разных настройках.
///
/// `cargo test --release -- --ignored img_profiles_page --nocapture`
///
/// Нужна, чтобы размен «скорость против размера» можно было посмотреть глазами,
/// а не по таблице чисел.
#[test]
#[ignore]
fn img_profiles_page() {
    fn b64(data: &[u8]) -> String {
        const T: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for c in data.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
            out.push(T[(n >> 18 & 63) as usize] as char);
            out.push(T[(n >> 12 & 63) as usize] as char);
            out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
            out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
        }
        out
    }

    let dir = std::path::Path::new("/tmp/bench_imgs");
    if !dir.exists() {
        eprintln!("нет /tmp/bench_imgs");
        return;
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let imgs: Vec<(std::path::PathBuf, Vec<u8>)> = files
        .iter()
        .step_by(20)
        .take(9)
        .map(|p| (p.clone(), std::fs::read(p).unwrap()))
        .collect();

    let modes: [(&str, ImageOptConfig); 4] = [
        (
            "БЫЛО: AVIF q40, 600×900, −45 %",
            ImageOptConfig {
                avif: true,
                avif_speed: 2,
                avif_speed_graphic: 2,
                avif_speed_cover: 2,
                max_w: 600,
                max_h: 900,
                shrink_percent: 45.0,
                method: 6,
                ..ImageOptConfig::default()
            },
        ),
        ("СЕЙЧАС: AVIF q35, 480×720, −55 %", ImageOptConfig::default()),
        (
            "AVIF q40, 480×720, −55 %",
            ImageOptConfig {
                quality: 40.0,
                ..ImageOptConfig::default()
            },
        ),
        (
            "WebP m4 q40, 480×720 (в 13 раз быстрее)",
            ImageOptConfig {
                avif: false,
                ..ImageOptConfig::default()
            },
        ),
    ];

    let mut html = String::from(
        "<!doctype html><meta charset=\"utf-8\"><title>Сравнение сжатия картинок</title>\
         <style>body{background:#1b1b1f;color:#ddd;font:14px system-ui;margin:24px}\
         table{border-collapse:collapse}td,th{padding:8px;vertical-align:top;text-align:center}\
         img{max-width:240px;image-rendering:auto;border:1px solid #444}\
         .meta{font-size:12px;color:#9aa}th{position:sticky;top:0;background:#1b1b1f}\
         h1{font-size:20px}</style><h1>Сравнение сжатия картинок</h1>\
         <p>Каждая строка — одна исходная картинка. Размеры под картинкой: пиксели и байты.</p><table><tr><th>исходник</th>",
    );
    for (label, _) in &modes {
        html.push_str(&format!("<th>{label}</th>"));
    }
    html.push_str("</tr>");

    for (path, data) in &imgs {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
        let mime = match ext {
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "image/jpeg",
        };
        html.push_str(&format!(
            "<tr><td><img src=\"data:{mime};base64,{}\"><div class=\"meta\">{}<br>{} Б</div></td>",
            b64(data),
            path.file_name().unwrap().to_string_lossy(),
            data.len()
        ));
        for (_, cfg) in &modes {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            match encode_image(&name, data, cfg, false) {
                Ok(Some(e)) => html.push_str(&format!(
                    "<td><img src=\"data:{};base64,{}\"><div class=\"meta\">{}×{}<br>{} Б</div></td>",
                    if e.format == Format::Avif { "image/avif" } else { "image/webp" },
                    b64(&e.data),
                    e.w,
                    e.h,
                    e.data.len()
                )),
                _ => html.push_str("<td>не закодировалось</td>"),
            }
        }
        html.push_str("</tr>");
    }
    html.push_str("</table>");

    let out = std::path::Path::new("/home/anton/Загрузки/сравнение_картинок.html");
    std::fs::write(out, html).unwrap();
    println!("страница: {}", out.display());
}
