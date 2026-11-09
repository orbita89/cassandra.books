//! Локальное GUI-приложение для библиотеки FB2/EPUB.
//!
//! Без HTTP-серверов: всё живёт в одном процессе.
//!   * UI-поток — egui/eframe, рисует кадры и НИКОГДА не блокируется;
//!   * фоновый поток — импорт ZIP (см. `worker`), общается с UI через
//!     `crossbeam-channel`;
//!   * `db` — поиск по Tantivy и извлечение книг из ARX.
//!
//! Все данные лежат в `data/` (путь можно передать первым аргументом).

mod db;
mod fb2epub;
mod genres;
mod images;
mod ui;
mod worker;

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver, Sender};
use db::{BookMetadata, Library};
use images::ImageOptConfig;
use std::path::{Path, PathBuf};
use worker::{ImportSettings, WorkerMsg};

/// Предельный размер кэша распакованных книг (`data/cache`).
/// Когда он превышен, самые старые книги удаляются автоматически.
pub const CACHE_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Размер в удобных единицах: `1536 МБ`, `2.1 ГБ`.
pub fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * MB;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} ГБ", b / GB)
    } else if b >= MB {
        format!("{:.0} МБ", b / MB)
    } else {
        format!("{:.0} КБ", b / 1024.0)
    }
}

/// Состояние импорта, которое рисует нижняя панель.
#[derive(Debug, Default)]
pub struct ImportState {
    /// Идёт ли импорт прямо сейчас.
    pub running: bool,
    /// Сколько книг уже обработано.
    pub done: usize,
    /// Сколько всего книг в текущем архиве.
    pub total: usize,
    /// Что делаем прямо сейчас (название книги).
    pub status: String,
    /// Последний упакованный батч.
    pub last_batch: Option<String>,
    /// Ошибки отдельных книг (импорт продолжается).
    pub errors: Vec<String>,
    /// Сколько битых ссылок на картинки нашли в исходных FB2.
    pub defects: usize,
    /// Сколько книг отброшено фильтром «только русские».
    pub skipped: usize,
    /// Итог: (успешно, с ошибками, секунды).
    pub finished: Option<(usize, usize, f64)>,
}

impl ImportState {
    /// Доля выполнения 0.0..=1.0.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
        }
    }
}

/// Всё состояние приложения.
pub struct App {
    /// Открытая библиотека (индекс + метаданные).
    pub library: Library,
    /// Текст в строке поиска.
    pub query: String,
    /// Запрос, по которому построен текущий `results`.
    pub last_query: String,
    /// Найденные книги (после фильтров).
    pub results: Vec<BookMetadata>,
    /// Сколько всего книг подошло под текстовый запрос (до фильтров).
    pub matched: usize,
    /// Фильтр по автору — точное совпадение; `None` значит «все».
    pub filter_author: Option<String>,
    /// Фильтр по жанру — точное совпадение; `None` значит «все».
    pub filter_genre: Option<String>,
    /// Авторы, встречающиеся в текущей выдаче (для выпадающего списка).
    pub authors: Vec<(String, usize)>,
    /// Жанры, встречающиеся в текущей выдаче.
    pub genres: Vec<(String, usize)>,
    /// Индекс выбранной книги в `results`.
    pub selected: Option<usize>,
    /// Открыто ли окно с информацией о книге (по клику на обложку).
    pub details_open: bool,
    /// Размер кэша распакованных книг: (файлов, байт).
    pub cache: (usize, u64),
    /// Состояние импорта.
    pub import: ImportState,
    /// Ошибка, которую надо показать в окне.
    pub error: Option<String>,
    /// Короткое сообщение об успехе.
    pub toast: Option<String>,
    /// Приёмник сообщений от фонового потока.
    pub rx: Receiver<WorkerMsg>,
    /// Отправитель — клонируется в фоновый поток.
    pub tx: Sender<WorkerMsg>,
    /// Настройки импорта.
    pub settings: ImportSettings,
}

impl App {
    /// Создать приложение и открыть библиотеку в `data_dir`.
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        let library = Library::open(&data_dir)?;
        let (tx, rx) = unbounded();
        let mut app = Self {
            library,
            query: String::new(),
            last_query: String::new(),
            results: Vec::new(),
            matched: 0,
            filter_author: None,
            filter_genre: None,
            authors: Vec::new(),
            genres: Vec::new(),
            selected: None,
            details_open: false,
            cache: (0, 0),
            import: ImportState::default(),
            error: None,
            toast: None,
            rx,
            tx,
            settings: ImportSettings::default(),
        };
        app.refresh_search()?;
        app.refresh_cache_usage();
        Ok(app)
    }

    /// Пересчитать выдачу по текущему запросу и фильтрам.
    pub fn refresh_search(&mut self) -> Result<()> {
        let outcome = self.library.search(
            &self.query,
            self.filter_author.as_deref(),
            self.filter_genre.as_deref(),
            db::DEFAULT_SEARCH_LIMIT,
        )?;
        self.results = outcome.books;
        self.authors = outcome.authors;
        self.genres = outcome.genres;
        self.matched = outcome.matched;
        self.last_query = self.query.clone();

        // Выбранная книга могла исчезнуть из выдачи после смены фильтра.
        if self.selected.map(|i| i >= self.results.len()).unwrap_or(false) {
            self.selected = None;
        }
        Ok(())
    }

    /// Сбросить фильтры по автору и жанру.
    pub fn clear_filters(&mut self) {
        self.filter_author = None;
        self.filter_genre = None;
    }

    /// Есть ли активные фильтры.
    pub fn has_filters(&self) -> bool {
        self.filter_author.is_some() || self.filter_genre.is_some()
    }

    /// Выбранная книга, если она есть.
    pub fn selected_book(&self) -> Option<&BookMetadata> {
        self.selected.and_then(|i| self.results.get(i))
    }

    /// Открыть окно с информацией о книге (клик по обложке).
    pub fn open_details(&mut self, index: usize) {
        self.selected = Some(index);
        self.details_open = true;
    }

    /// Пересчитать размер кэша распакованных книг.
    pub fn refresh_cache_usage(&mut self) {
        self.cache = self.library.cache_usage();
    }

    /// Очистить кэш распакованных книг.
    pub fn clear_cache(&mut self) {
        match self.library.clear_cache() {
            Ok(n) => self.toast = Some(format!("кэш очищен, удалено файлов: {n}")),
            Err(err) => self.error = Some(format!("не удалось очистить кэш: {err:#}")),
        }
        self.refresh_cache_usage();
    }

    /// Запустить импорт выбранного ZIP-архива.
    pub fn start_import(&mut self, zip: PathBuf) {
        self.import = ImportState {
            running: true,
            status: "распаковка...".to_string(),
            ..Default::default()
        };
        self.error = None;
        self.toast = None;
        let data_dir = self.library.data_dir.clone();
        worker::spawn_import(zip, data_dir, self.settings.clone(), self.tx.clone());
    }

    /// Забрать всё, что прислал фоновый поток, и обновить состояние.
    pub fn pump_worker(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Started { total, zip } => {
                    self.import.running = true;
                    self.import.total = total;
                    self.import.done = 0;
                    self.import.errors.clear();
                    self.import.finished = None;
                    self.import.status = format!("{zip}: {total} книг");
                }
                WorkerMsg::Progress {
                    done,
                    total,
                    status,
                    error,
                } => {
                    self.import.done = done;
                    self.import.total = total;
                    self.import.status = status;
                    if let Some(err) = error {
                        // Держим только последние 50 ошибок, чтобы не расти бесконечно.
                        if self.import.errors.len() < 50 {
                            self.import.errors.push(err);
                        }
                    }
                }
                WorkerMsg::Batch {
                    archive,
                    from_id,
                    to_id,
                    books,
                } => {
                    self.import.last_batch =
                        Some(format!("{archive}: {books} книг (id {from_id}..{to_id})"));
                }
                WorkerMsg::Finished {
                    books,
                    failed,
                    skipped,
                    defects,
                    seconds,
                } => {
                    self.import.running = false;
                    self.import.skipped = skipped;
                    self.import.finished = Some((books, failed, seconds));
                    self.import.defects = defects;
                    let mut tail = String::new();
                    if skipped > 0 {
                        tail.push_str(&format!(", нерусских отброшено: {skipped}"));
                    }
                    if defects > 0 {
                        tail.push_str(&format!(", битых ссылок в исходниках: {defects}"));
                    }
                    self.import.status =
                        format!("готово: {books} книг за {seconds:.0} с (ошибок: {failed}{tail})");
                    // Новые книги должны сразу находиться поиском.
                    let _ = self.library.reload_metadata();
                    let _ = self.refresh_search();
                    self.refresh_cache_usage();
                }
                WorkerMsg::Error(err) => {
                    self.import.running = false;
                    self.error = Some(err);
                }
            }
        }
    }
}

/// Ручное сжатие отдельных картинок — теми же настройками, что и импорт.
///
/// `library_gui --image photo.jpg` кладёт рядом `photo.avif`. Это ровно тот же
/// код, что сжимает картинки внутри книг, поэтому результат совпадает с тем,
/// что попадёт в EPUB. Настройки берутся из тех же переменных окружения:
/// `LIBRARY_QUALITY`, `LIBRARY_MAX_W`, `LIBRARY_MAX_H`, `LIBRARY_SHRINK`,
/// `LIBRARY_AVIF_SPEED`, `LIBRARY_WEBP` и остальных.
///
/// `--out <каталог>` кладёт результат в указанный каталог, а не рядом с файлом.
fn run_image_mode(args: &[String]) -> i32 {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut out_dir: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" | "-o" => out_dir = it.next().map(PathBuf::from),
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.is_empty() {
        eprintln!("использование: library_gui --image <картинка>… [--out каталог]");
        return 2;
    }

    let cfg = images::ImageOptConfig::from_env();
    println!(
        "настройки: до {}×{}, уменьшение {:.0} %, качество {} (графика {}), {}",
        cfg.max_w,
        cfg.max_h,
        cfg.shrink_percent,
        cfg.quality,
        cfg.text_quality,
        if cfg.avif {
            format!("AVIF, скорость {}", cfg.avif_speed)
        } else {
            format!("WebP, метод {}", cfg.method)
        }
    );

    let mut failed = 0;
    for src in &files {
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let data = match std::fs::read(src) {
            Ok(data) => data,
            Err(err) => {
                eprintln!("[!] {}: не читается: {err}", src.display());
                failed += 1;
                continue;
            }
        };

        let done = match images::optimize_single(&name, &data, &cfg) {
            Ok(Some(done)) => done,
            Ok(None) => {
                eprintln!("[!] {}: не удалось декодировать", src.display());
                failed += 1;
                continue;
            }
            Err(err) => {
                eprintln!("[!] {}: {err:#}", src.display());
                failed += 1;
                continue;
            }
        };

        let dir = out_dir.clone().unwrap_or_else(|| {
            src.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        });
        let stem = src
            .file_stem()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "image".to_string());
        let dst = dir.join(format!("{stem}.{}", done.ext));
        if let Err(err) = std::fs::create_dir_all(&dir) {
            eprintln!("[!] не удалось создать {}: {err}", dir.display());
            failed += 1;
            continue;
        }
        if let Err(err) = std::fs::write(&dst, &done.data) {
            eprintln!("[!] не удалось записать {}: {err}", dst.display());
            failed += 1;
            continue;
        }

        let before = data.len().max(1) as f64;
        let after = done.data.len() as f64;
        println!("{} → {}", src.display(), dst.display());
        println!(
            "  {}, {}×{} → {}×{}{}, {} Б → {} Б ({:+.1} %)",
            done.mime,
            done.before_w,
            done.before_h,
            done.w,
            done.h,
            if done.resized { " (уменьшено)" } else { "" },
            data.len(),
            done.data.len(),
            100.0 * (after - before) / before
        );
    }
    i32::from(failed > 0)
}

fn main() -> eframe::Result<()> {
    // AVIF-энкодер (`ravif`) зовёт `rayon::join`; когда его вызывают не из
    // rayon-воркера (а именно так теперь работает импорт), работа уходит в
    // глобальный пул rayon. Даём ему большой стек: `rav1e` рекурсивный.
    let _ = rayon::ThreadPoolBuilder::new()
        .stack_size(16 * 1024 * 1024)
        .build_global();

    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(concat!(
            "Библиотека FB2/EPUB — локальное GUI без серверов.\n",
            "\n",
            "Использование:\n",
            "  library_gui [data_dir]                    окно (data_dir по умолчанию `data`)\n",
            "  library_gui --import <zip> [data_dir]     импорт без окна\n",
            "  library_gui --search <запрос> [data_dir] [--author X] [--genre Y]\n",
            "                                            поиск без окна\n",
            "  library_gui --convert <fb2> <epub>         конвертация FB2 → EPUB 3.3\n",
            "                                            без окна (отладка конвертера)\n",
            "  library_gui --image <файл>… [--out кат]   сжать картинки так же,\n",
            "                                            как при импорте (AVIF)\n",
            "  library_gui --bench <zip> [каталог]       бенчмарк всех шагов сжатия\n",
            "                                            (по умолчанию data/bench)\n",
            "\n",
            "Каталог данных:\n",
            "  archives/       архивы <от>-<до>.arx (один на входной ZIP)\n",
            "  covers/         обложки <id%1000>/<id>.avif\n",
            "  cache/          распакованные для чтения книги (чистится)\n",
            "  index/          индекс Tantivy для поиска\n",
            "  metadata/       метаданные книг, по файлу на 1000 книг\n",
        ));
        std::process::exit(0);
    }

    // Служебный режим без окна: `library_gui --import <zip> [data_dir]`.
    // Нужен, чтобы проверять пайплайн без дисплея и гонять пакетные импорты.
    if args.first().map(String::as_str) == Some("--import") {
        let zip = match args.get(1) {
            Some(p) => PathBuf::from(p),
            None => {
                eprintln!("использование: library_gui --import <zip> [data_dir]");
                std::process::exit(2);
            }
        };
        let data_dir = args
            .get(2)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data"));
        std::process::exit(run_headless_import(zip, data_dir));
    }

    // Служебный режим без окна:
    // `library_gui --convert <fb2> <epub>` — только конвертация, без пайплайна.
    if args.first().map(String::as_str) == Some("--convert") {
        let (Some(src), Some(dst)) = (args.get(1), args.get(2)) else {
            eprintln!("использование: library_gui --convert <fb2> <epub>");
            std::process::exit(2);
        };
        match fb2epub::convert(std::path::Path::new(src), std::path::Path::new(dst)) {
            Ok(r) => {
                let bad = r.missing_notes.len() + r.missing_images.len() + r.missing_anchors.len();
                let tail = if bad == 0 {
                    String::new()
                } else {
                    format!(
                        ", битых ссылок: примечаний {}, иллюстраций {}, якорей {}",
                        r.missing_notes.len(),
                        r.missing_images.len(),
                        r.missing_anchors.len()
                    )
                };
                println!(
                    "{src} → {dst}: частей {}, примечаний {}, картинок {}{tail}",
                    r.parts, r.notes, r.images
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("ошибка: {e:#}");
                std::process::exit(1);
            }
        }
    }

    // Служебный режим без окна: `library_gui --bench <zip> [out_dir]`.
    // Прогоняет каждую книгу по стадиям (картинки → минификация → шрифты →
    // контейнер) и печатает таблицу, сколько байт даёт каждый шаг.
    if args.first().map(String::as_str) == Some("--bench") {
        let zip = match args.get(1) {
            Some(p) => PathBuf::from(p),
            None => {
                eprintln!("использование: library_gui --bench <zip> [каталог]");
                std::process::exit(2);
            }
        };
        let out = args
            .get(2)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data/bench"));
        // Бенчмарк кодирует AVIF на этом же потоке, а `rav1e` рекурсивный:
        // на главном потоке 8 МиБ, но лучше не зависеть от платформы.
        let worker = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || run_bench(zip, out))
            .expect("не удалось создать поток для бенчмарка");
        std::process::exit(worker.join().unwrap_or(2));
    }

    // Служебный режим без окна:
    // `library_gui --image <картинка>… [--out каталог]` — сжать картинки
    // ровно так же, как это делает импорт, и положить рядом `.avif`.
    if args.first().map(String::as_str) == Some("--image") {
        std::process::exit(run_image_mode(&args[1..]));
    }

    // Служебный режим без окна:
    // `library_gui --search <запрос> [data_dir] [--author X] [--genre Y]`.
    if args.first().map(String::as_str) == Some("--search") {
        let mut query = String::new();
        let mut query_set = false;
        let mut data_dir = PathBuf::from("data");
        let mut data_dir_set = false;
        let mut author: Option<String> = None;
        let mut genre: Option<String> = None;

        let mut it = args[1..].iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--author" => author = it.next().cloned(),
                "--genre" => genre = it.next().cloned(),
                _ if !query_set => {
                    query = arg.clone();
                    query_set = true;
                }
                _ if !data_dir_set => {
                    data_dir = PathBuf::from(arg);
                    data_dir_set = true;
                }
                other => eprintln!("[!] лишний аргумент проигнорирован: {other}"),
            }
        }
        std::process::exit(run_headless_search(
            &query,
            data_dir,
            author.as_deref(),
            genre.as_deref(),
        ));
    }

    // Первый аргумент — каталог данных (по умолчанию `data` рядом с бинарником).
    let data_dir = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data"));

    // Иконка встроена в бинарник, чтобы окно и панель задач выглядели
    // одинаково независимо от того, установлен ли ярлык.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([820.0, 520.0])
        .with_title("Библиотека — FB2/EPUB")
        .with_app_id("library_gui");
    match eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(err) => eprintln!("[!] встроенная иконка не прочиталась: {err}"),
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Библиотека — FB2/EPUB",
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let app = App::new(data_dir).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("не удалось открыть библиотеку: {e:#}").into()
            })?;
            Ok(Box::new(app))
        }),
    )
}

/// Печать строки в stdout без паники на закрытом канале.
/// `println!` в такой ситуации паникует (`| head` закрывает pipe), а это
/// диагностический режим — он должен просто тихо закончиться.
fn emit(line: impl AsRef<str>) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{}", line.as_ref());
    let _ = lock.flush();
}

/// Импорт без окна: печатает прогресс в stdout, возвращает код выхода.
/// `--bench`: прогнать все шаги оптимизации и напечатать таблицу экономии.
fn run_bench(zip: PathBuf, out_dir: PathBuf) -> i32 {
    const MB: f64 = 1024.0 * 1024.0;
    fn mb(bytes: u64) -> String {
        format!("{:.2} МБ", bytes as f64 / MB)
    }
    /// «−1.23 МБ (Экономия: 4.5 %)»; если стало больше — честный плюс.
    fn saved(from: u64, to: u64) -> String {
        let delta = from as f64 - to as f64;
        let pct = if from == 0 { 0.0 } else { delta / from as f64 * 100.0 };
        if delta < 0.0 {
            format!("+{:.2} МБ (+{:.1} %)", -delta / MB, -pct)
        } else {
            format!("−{:.2} МБ (Экономия: {pct:.1} %)", delta / MB)
        }
    }

    let tmp = out_dir.join("tmp");
    let fb2_dir = tmp.join("fb2");
    let deflated = out_dir.join("deflated");
    let stored = out_dir.join("stored");
    for d in [&deflated, &stored, &fb2_dir] {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("ошибка: не удалось создать {}: {e}", d.display());
            return 2;
        }
    }

    let fb2_files = match worker::unzip_fb2(&zip, &fb2_dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ошибка: {e:#}");
            return 2;
        }
    };
    if fb2_files.is_empty() {
        eprintln!("ошибка: в {} нет .fb2", zip.display());
        return 2;
    }

    let cfg = ImageOptConfig::from_env();
    let mut total = images::BenchReport::default();
    let mut books = 0usize;
    let mut failed = 0usize;
    let started = std::time::Instant::now();

    for fb2 in &fb2_files {
        let stem = fb2
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "book".to_string());
        let epub = tmp.join(format!("{stem}.epub"));
        if let Err(e) = fb2epub::convert(fb2, &epub) {
            eprintln!("[!] {stem}: конвертация не удалась: {e:#}");
            failed += 1;
            continue;
        }
        match images::benchmark_epub(&epub, &deflated, &stored, &tmp, &cfg) {
            Ok(r) => {
                total.merge(&r);
                books += 1;
            }
            Err(e) => {
                eprintln!("[!] {stem}: бенчмарк не удался: {e:#}");
                failed += 1;
            }
        }
        let _ = std::fs::remove_file(&epub);
    }
    let seconds = started.elapsed().as_secs_f64();

    println!("=== БЕНЧМАРК ОПТИМИЗАЦИИ ({books} книг, {seconds:.1} с) ===");
    println!(
        "Исходный EPUB (после WebP/AVIF, Deflate {}): {} ({} Б)",
        cfg.zip_level,
        mb(total.base),
        total.base
    );
    println!(
        "  картинок: {}, текста без сжатия: {}",
        total.images,
        mb(total.text_before)
    );
    println!();
    println!(
        "1. Минификация XHTML/CSS/OPF:  {} ({} Б)  {}",
        mb(total.minified),
        total.minified,
        saved(total.base, total.minified)
    );
    println!(
        "   текст без сжатия: {} → {} ({} Б)",
        mb(total.text_before),
        mb(total.text_after),
        total.text_after
    );
    if total.fonts_before > 0 {
        println!(
            "2. Subsetting шрифтов:         {} ({} Б)  {}  (ужато шрифтов: {})",
            mb(total.fonts),
            total.fonts,
            saved(total.minified, total.fonts),
            total.fonts_subset
        );
        println!(
            "   шрифты: {} → {} ({} Б)",
            mb(total.fonts_before),
            mb(total.fonts_after),
            total.fonts_after
        );
    } else {
        println!("2. Subsetting шрифтов:         встроенных шрифтов нет — экономии нет");
    }
    println!(
        "3. Очистка контейнера:         {} ({} Б)  {}  (удалено записей: {})",
        mb(total.clean),
        total.clean,
        saved(total.fonts, total.clean),
        total.entries_dropped
    );
    println!(
        "ИТОГО чистый EPUB (Deflate):   {} ({} Б)  {}",
        mb(total.clean),
        total.clean,
        saved(total.base, total.clean)
    );
    println!(
        "      тот же EPUB без Deflate:  {} ({} Б)  (для .arx)",
        mb(total.stored),
        total.stored
    );

    // ---- ARX ----
    println!();
    println!("=== БЕНЧМАРК АРХИВА ARX ===");
    let modes: [(&str, &PathBuf, &str); 4] = [
        ("ARX (none)", &deflated, "none"),
        ("ARX (zstd 19)", &deflated, "zstd=19"),
        ("ARX (zstd 22)", &deflated, "zstd=22"),
        ("ARX (zstd 19, Stored внутри)", &stored, "zstd=19"),
    ];
    let mut rows: Vec<(&str, u64)> = Vec::new();
    for (label, dir, compression) in modes {
        let out = out_dir.join(format!(
            "bench_{}.arx",
            compression.replace('=', "_") + if std::ptr::eq(dir, &stored) { "_stored" } else { "" }
        ));
        match worker::make_arx(dir, &out, compression) {
            Ok(size) => rows.push((label, size)),
            Err(e) => eprintln!("[!] {label}: {e:#}"),
        }
    }
    let none = rows.first().map(|r| r.1).unwrap_or(0);
    for (label, size) in &rows {
        let pct = if none == 0 {
            0.0
        } else {
            (none as f64 - *size as f64) / none as f64 * 100.0
        };
        let pct = if pct.abs() < 0.05 {
            "(0 %)".to_string()
        } else {
            format!("(-{pct:.1} %)")
        };
        println!("{label:<30} {:>9} ({size} Б) {pct}", mb(*size));
    }

    if let Some(best) = rows.iter().min_by_key(|r| r.1) {
        println!();
        println!(
            "Вывод: {} — {} ({} Б). Deflate внутри EPUB не нужен: zstd жмёт батч целиком.",
            best.0,
            mb(best.1),
            best.1
        );
    }
    if failed > 0 {
        return 1;
    }
    0
}

fn run_headless_import(zip: PathBuf, data_dir: PathBuf) -> i32 {
    emit(format!(
        "импорт {} → {}",
        zip.display(),
        data_dir.join("archives").display()
    ));
    let (tx, rx) = unbounded();
    worker::spawn_import(zip, data_dir, ImportSettings::default(), tx);

    while let Ok(msg) = rx.recv() {
        match msg {
            WorkerMsg::Started { total, zip } => {
                emit(format!("[старт] {zip}: {total} книг"));
            }
            WorkerMsg::Progress {
                done,
                total,
                status,
                error,
            } => match error {
                Some(err) => eprintln!("[{done}/{total}] ОШИБКА: {err}"),
                None => emit(format!("[{done}/{total}] {status}")),
            },
            WorkerMsg::Batch {
                archive,
                from_id,
                to_id,
                books,
            } => emit(format!(
                "[arx] {archive}: {books} книг (id {from_id}..{to_id})"
            )),
            WorkerMsg::Finished {
                books,
                failed,
                skipped,
                defects,
                seconds,
            } => {
                emit(format!(
                    "[готово] {books} книг за {seconds:.1} с, ошибок: {failed}, \
                     нерусских отброшено: {skipped}, \
                     битых ссылок в исходниках: {defects}"
                ));
                return if failed == 0 { 0 } else { 1 };
            }
            WorkerMsg::Error(err) => {
                eprintln!("[фатально] {err}");
                return 2;
            }
        }
    }
    1
}

/// Поиск без окна: печатает результаты и проверяет извлечение первой книги.
fn run_headless_search(
    query: &str,
    data_dir: PathBuf,
    author: Option<&str>,
    genre: Option<&str>,
) -> i32 {
    let library = match Library::open(&data_dir) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("[фатально] не удалось открыть библиотеку: {err:#}");
            return 2;
        }
    };
    emit(format!("книг в библиотеке: {}", library.books.len()));

    let hits = match library.search(query, author, genre, db::DEFAULT_SEARCH_LIMIT) {
        Ok(h) => h,
        Err(err) => {
            eprintln!("[фатально] поиск не удался: {err:#}");
            return 2;
        }
    };
    emit(format!(
        "по запросу `{query}` найдено: {} (показано {}){}",
        hits.matched,
        hits.books.len(),
        match (author, genre) {
            (None, None) => String::new(),
            _ => format!(
                " [фильтры: автор={}, жанр={}]",
                author.unwrap_or("все"),
                genre.unwrap_or("все")
            ),
        }
    ));
    emit(format!(
        "фасеты: авторов {}, жанров {}",
        hits.authors.len(),
        hits.genres.len()
    ));
    for book in hits.books.iter().take(20) {
        let d = &book.details;
        let year = if d.year.is_empty() { d.date.as_str() } else { d.year.as_str() };
        emit(format!(
            "  {} | {} | {} | {} | {} | год: {} | глав: {}",
            book.id,
            book.title,
            book.author,
            book.genres.join(", "),
            book.archive,
            if year.is_empty() { "—" } else { year },
            d.chapters.len()
        ));
    }

    // Проверяем извлечение первой книги — ровно тем же путём, что и кнопка «Читать».
    let Some(first) = hits.books.first() else {
        return 1;
    };
    let dest = std::env::temp_dir()
        .join("library_gui_check")
        .join(&first.internal_path);
    match library.extract_to(first, &dest) {
        Ok(()) => {
            let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            emit(format!(
                "[извлечение] {} → {} ({size} байт)",
                first.internal_path,
                dest.display()
            ));
            0
        }
        Err(err) => {
            eprintln!("[фатально] извлечение не удалось: {err:#}");
            2
        }
    }
}

impl eframe::App for App {
    /// Вызывается перед отрисовкой кадра — здесь безопасно менять состояние,
    /// но нельзя ничего рисовать.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Забираем сообщения фонового потока в начале кадра.
        self.pump_worker();
        // Пока идёт импорт — перерисовываем постоянно, иначе egui спит.
        if self.import.running {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
    }

    /// Основная отрисовка кадра.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui::draw(self, ui);
    }
}
