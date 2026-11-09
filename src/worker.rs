//! Фоновый поток импорта: ZIP → FB2 → EPUB → 100 % WebP → ARX → индекс.
//!
//! Поток ничего не знает про UI: он только шлёт [`WorkerMsg`] в канал.
//! UI читает канал раз в кадр и обновляет прогресс-бар — поэтому окно
//! не подвисает даже на архиве в тысячи книг.
//!
//! Шаги (ни один не пропускается молча):
//!   1. распаковать ZIP во временную папку;
//!   2. на каждую книгу: метаданные + обложка, свой конвертер FB2 → EPUB 3.3
//!      (`fb2epub`, без внешних процессов), оптимизация картинок
//!      (100 % WebP + уменьшение), проверка;
//!   3. упаковать всё в один `data/archives/<от>-<до>.arx`;
//!   4. добавить метаданные в Tantivy и в `data/metadata/`;
//!   5. убрать временные файлы.

use crate::db::{BookMetadata, Details};
use crate::genres;
use crate::images::{self, ImageOptConfig};
use anyhow::{bail, Context, Result};
use crossbeam_channel::Sender;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Пофазный профиль импорта: где на самом деле уходит время.
///
/// Значения — наносекунды, накопленные **всеми потоками**. Поэтому сравнивать
/// их между собой можно (это процессорное время), а с календарным временем —
/// только поделив на число занятых ядер. Печатается при `LIBRARY_PROFILE=1`.
#[derive(Default)]
pub struct Profile {
    pub unzip: AtomicU64,
    pub convert: AtomicU64,
    pub images: AtomicU64,
    pub cover: AtomicU64,
    pub chapters: AtomicU64,
    pub pack: AtomicU64,
    pub index: AtomicU64,
    pub wall: AtomicU64,
}

/// Общий счётчик на процесс: заполняется из всех потоков импорта.
pub static PROFILE: Profile = Profile {
    unzip: AtomicU64::new(0),
    convert: AtomicU64::new(0),
    images: AtomicU64::new(0),
    cover: AtomicU64::new(0),
    chapters: AtomicU64::new(0),
    pack: AtomicU64::new(0),
    index: AtomicU64::new(0),
    wall: AtomicU64::new(0),
};

/// Прибавить к счётчику прошедшее с `t` время.
fn tally(slot: &AtomicU64, t: Instant) {
    slot.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// Напечатать пофазный отчёт, если попросили.
fn print_profile(cores: usize) {
    if std::env::var_os("LIBRARY_PROFILE").is_none() {
        return;
    }
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64 / 1e9;
    let wall = g(&PROFILE.wall);
    if wall <= 0.0 {
        return;
    }
    let parts = [
        ("распаковка ZIP", g(&PROFILE.unzip)),
        ("конвертация FB2→EPUB", g(&PROFILE.convert)),
        ("картинки", g(&PROFILE.images)),
        ("обложки", g(&PROFILE.cover)),
        ("оглавление", g(&PROFILE.chapters)),
        ("упаковка arx", g(&PROFILE.pack)),
        ("индекс+метаданные", g(&PROFILE.index)),
    ];
    let sum: f64 = parts.iter().map(|(_, v)| *v).sum();
    eprintln!("\n[профиль] календарное время {wall:.1} с, потоков {cores}");
    for (name, v) in parts {
        eprintln!("  {name:<24} {v:>9.1} с процессорного  {:>5.1} %", v / sum * 100.0);
    }
    eprintln!("  {:<24} {sum:>9.1} с", "ИТОГО по фазам");
    eprintln!(
        "  средняя загрузка: {:.1} ядер из {cores} (простой — ожидание упаковки и хвост)",
        sum / wall
    );

    // Разбивка самой тяжёлой фазы: чем именно занято кодирование картинок.
    use images as im;
    let calls = |a: &std::sync::atomic::AtomicUsize| a.load(Ordering::Relaxed);
    eprintln!("  внутри «картинок»:");
    let mut inner = [
        ("декодирование исходника", g(&im::DECODE_NS)),
        ("анализ графика/фото", g(&im::FEATURES_NS)),
        ("уменьшение (Lanczos3)", g(&im::RESIZE_NS)),
        ("энкодер AVIF", g(&im::AVIF_NS)),
        ("энкодер WebP", g(&im::WEBP_NS)),
        ("замер штриха", g(&im::STROKE_NS)),
    ];
    inner.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (name, v) in inner {
        eprintln!("    {name:<24} {v:>9.1} с");
    }
    eprintln!(
        "    вызовов энкодеров: AVIF {}, WebP {}",
        calls(&im::AVIF_CALLS),
        calls(&im::WEBP_CALLS)
    );
}

/// Сколько потоков задействовать: 0 в настройках = все доступные ядра.
pub fn threads_for(settings: &ImportSettings) -> usize {
    if settings.jobs > 0 {
        settings.jobs
    } else {
        // Книг в работе должно быть в 3 раза больше, чем ядер: поток книги
        // значительную часть времени ждёт кодирования своих картинок, и пока
        // он ждёт, пул не должен простаивать. Очередь книг дешёвая (ожидание
        // на канале), а «следующая книга с готовыми кадрами» должна быть всегда.
        std::thread::available_parallelism()
            .map(|n| (n.get() * 3).max(4))
            .unwrap_or(8)
    }
}

/// Сжатие для `arx create` по умолчанию.
///
/// Если EPUB внутри лежит **без Deflate** (`ImageOptConfig::epub_stored`),
/// zstd жмёт батч целиком и даёт на ~20 % меньше, чем Deflate по книгам:
/// на 14 тестовых книгах 5.81 МБ против 7.41 МБ. Уровень 19 — колено кривой,
/// дальше 22 почти не выигрывает, а времени тратит больше.
pub const ARX_COMPRESSION: &str = "zstd=19";

/// Сообщения из фонового потока в UI.
#[derive(Debug, Clone)]
pub enum WorkerMsg {
    /// Импорт начался: сколько всего книг нашли.
    Started { total: usize, zip: String },
    /// Очередная книга обработана (или упала — тогда `error` не пустой).
    Progress {
        done: usize,
        total: usize,
        status: String,
        error: Option<String>,
    },
    /// Батч упакован в `.arx`.
    Batch {
        archive: String,
        from_id: u64,
        to_id: u64,
        books: usize,
    },
    /// Всё готово.
    Finished {
        books: usize,
        /// Сколько книг не удалось обработать.
        failed: usize,
        /// Сколько книг отброшено фильтром «только русские».
        skipped: usize,
        /// Сколько битых ссылок на картинки нашли в исходниках (дефект FB2, не наш).
        defects: usize,
        seconds: f64,
    },
    /// Фатальная ошибка импорта — показывается в окне.
    Error(String),
}

/// Почему книга не прошла фильтр языка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LangVerdict {
    /// Буквы в тексте в основном не кириллица.
    NotCyrillic,
    /// Кириллица, но с заметной долей букв, которых в русском алфавите нет.
    NotRussianCyrillic,
}

impl LangVerdict {
    fn reason(self) -> &'static str {
        match self {
            LangVerdict::NotCyrillic => "текст не на кириллице",
            LangVerdict::NotRussianCyrillic => "кириллица, но не русский алфавит",
        }
    }
}

/// Книга, отброшенная фильтром языка. Идёт в отчёт, а не в архив.
#[derive(Debug, Clone)]
struct ForeignBook {
    id: u64,
    title: String,
    author: String,
    reason: &'static str,
    stats: crate::fb2epub::ScriptStats,
}

/// Итог обработки одной книги.
enum BookOutcome {
    /// Книга готова: метаданные и замечания к исходнику.
    ///
    /// `Box` не для красоты: `BookMetadata` весит под 700 байт, и без него
    /// каждый `Err`/`Foreign` в общем векторе результатов тянул бы столько же.
    Ready(Box<BookMetadata>, Vec<String>),
    /// Книга отброшена фильтром языка.
    Foreign(Box<ForeignBook>),
}

/// Настройки фильтра «только русские книги».
#[derive(Debug, Clone, Copy)]
struct LangFilter {
    enabled: bool,
    /// Не отбрасывать кириллические книги, даже если алфавит не русский.
    keep_cyrillic: bool,
}

impl LangFilter {
    fn from_env() -> Self {
        Self {
            enabled: std::env::var("LIBRARY_LANG_FILTER").map_or(true, |v| v != "0"),
            keep_cyrillic: std::env::var_os("LIBRARY_LANG_KEEP_CYRILLIC").is_some(),
        }
    }

    /// Вердикт по статистике букв.
    ///
    /// Порог намеренно грубый и безопасный. На реальном корпусе русские книги
    /// дают 97–100 % кириллицы, а нерусские — меньше 1 %, то есть зазор в сотни
    /// раз. Книга с английским названием, английской аннотацией и русским
    /// текстом уверенно попадает в русские, потому что считаются буквы всего
    /// текста, а не заголовка. Всё сомнительное остаётся: потерять русскую
    /// книгу хуже, чем пропустить нерусскую.
    fn judge(&self, st: &crate::fb2epub::ScriptStats) -> Option<LangVerdict> {
        /// Меньше этого числа букв — судить не о чем (картинки, обложка, обрывок).
        const MIN_LETTERS: u64 = 200;
        if !self.enabled || st.letters() < MIN_LETTERS {
            return None;
        }
        if st.cyrillic_share() < 0.5 {
            return Some(LangVerdict::NotCyrillic);
        }
        if !self.keep_cyrillic && st.foreign_share() > 0.03 {
            return Some(LangVerdict::NotRussianCyrillic);
        }
        None
    }
}

/// Настройки импорта.
#[derive(Debug, Clone)]
pub struct ImportSettings {
    /// Параметры оптимизации картинок.
    pub image: ImageOptConfig,
    /// Сколько книг обрабатывать параллельно (0 = по числу ядер).
    pub jobs: usize,
    /// Аргумент `-c` для `arx create`: `zstd=19`, `none`, `lzma=9`…
    pub arx_compression: String,
}

impl Default for ImportSettings {
    fn default() -> Self {
        Self {
            image: ImageOptConfig::from_env(),
            jobs: 0,
            arx_compression: ARX_COMPRESSION.to_string(),
        }
    }
}

/// Запустить импорт в отдельном потоке. Возвращает сразу, UI не блокируется.
pub fn spawn_import(
    zip: PathBuf,
    data_dir: PathBuf,
    settings: ImportSettings,
    tx: Sender<WorkerMsg>,
) {
    std::thread::spawn(move || {
        if let Err(err) = run_import(&zip, &data_dir, &settings, &tx) {
            let _ = tx.send(WorkerMsg::Error(format!("{err:#}")));
        }
    });
}

/// Основной проход. Любая ошибка — наверх, в [`WorkerMsg::Error`].
fn run_import(
    zip: &Path,
    data_dir: &Path,
    settings: &ImportSettings,
    tx: &Sender<WorkerMsg>,
) -> Result<()> {
    let started = Instant::now();
    if !zip.exists() {
        bail!("входной архив не найден: {}", zip.display());
    }

    let tmp = data_dir.join("tmp");
    let work = tmp.join("work");
    let stage = tmp.join("stage");
    let archives_dir = data_dir.join("archives");

    // Импорт **дописывает** метаданные и никогда не чистит архивы. Если в
    // каталоге уже лежит библиотека, повторный импорт того же ZIP оставит в
    // `metadata.jsonl` две записи на книгу и два архива на диске. Это не
    // ошибка, но сказать об этом надо: иначе непонятно, откуда взялись
    // двойники и почему место кончилось.
    let meta_dir = data_dir.join("metadata");
    let has_meta = fs::read_dir(&meta_dir)
        .map(|d| d.filter_map(|e| e.ok()).any(|e| e.path().is_file()))
        .unwrap_or(false)
        || data_dir
            .join("metadata.jsonl")
            .metadata()
            .map(|m| m.len() > 0)
            .unwrap_or(false);
    if has_meta {
        eprintln!(
            "[!] в {} уже есть библиотека: импорт допишет книги к ней, а старые \
             архивы останутся на диске. Для чистой пересборки удалите каталоги \
             metadata/, archives/, covers/, index/.",
            data_dir.display()
        );
    }
    let covers_dir = data_dir.join("covers");
    for dir in [&work, &stage, &archives_dir, &covers_dir] {
        fs::create_dir_all(dir)
            .with_context(|| format!("не удалось создать {}", dir.display()))?;
    }
    // Прошлый неудачный прогон не должен подмешиваться в новый.
    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&work)?;
    fs::create_dir_all(&stage)?;

    // ---- шаг 1: распаковка -------------------------------------------------
    let t_unzip = Instant::now();
    let mut fb2_files = unzip_fb2(zip, &work)
        .with_context(|| format!("не удалось распаковать {}", zip.display()))?;
    tally(&PROFILE.unzip, t_unzip);
    if fb2_files.is_empty() {
        bail!("в архиве {} не найдено ни одного .fb2", zip.display());
    }
    let total = fb2_files.len();

    // Порядок обработки — от больших книг к маленьким (LPT).
    //
    // Книги неравномерны на порядки: в корпусе есть FB2 на 59 МБ (45 МБ
    // готового EPUB) и книги с единственной картинкой. Если пустить их в
    // порядке ZIP, к концу импорта досчитывают две-три книги, а остальные
    // потоки стоят: машина простаивает больше половины времени. «Сначала
    // самые длинные» — классика планирования, даёт расписание не хуже 4/3
    // от оптимального по времени завершения.
    fb2_files.sort_by_cached_key(|p| std::cmp::Reverse(fs::metadata(p).map(|m| m.len()).unwrap_or(0)));

    let _ = tx.send(WorkerMsg::Started {
        total,
        zip: zip
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| zip.display().to_string()),
    });

    let threads = threads_for(settings).max(1);
    // Размер стека на поток. 16 МиБ — это виртуальная память, реально
    // тратится меньше; переопределяется `LIBRARY_STACK_KIB` для диагностики.
    let stack = std::env::var("LIBRARY_STACK_KIB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|k| k * 1024)
        .unwrap_or(16 * 1024 * 1024);

    let done = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let defects = AtomicUsize::new(0);
    let mut all_books: Vec<BookMetadata> = Vec::with_capacity(total);

    // ---- шаги 2–3: все книги параллельно, одна упаковка в конце -------------
    //
    // Книги берутся из общей очереди: освободившийся поток сразу берёт
    // следующую. Раньше они шли группами по числу потоков, и каждая группа
    // была барьером — одна тяжёлая книга (в корпусе есть FB2 на 59 МБ и
    // промежуточные EPUB на 45 МБ) держала остальные потоки впустую.
    let stage_dir = stage.join("books");
    let _ = fs::remove_dir_all(&stage_dir);
    fs::create_dir_all(&stage_dir)?;

    // Ошибки отдельных книг не роняют весь импорт: их копим и отдаём в UI.
    //
    // Потоки — обычные `std::thread`, а не rayon, и это принципиально.
    // AVIF-энкодер (`image` → `ravif` → `rav1e`) сам внутри вызывает
    // `rayon::join`, а `rav1e` уводит работу в свой пул через
    // `Registry::in_worker_cross`. Пока поток ждёт, rayon разрешает ему
    // «украсть» следующую задачу из своего же реестра — то есть следующую
    // картинку из нашего `par_iter`. Каждый такой уровень добавляет полный
    // кадр энкодера, и вложенность растёт вплоть до числа картинок в книге:
    // на дефолтных 2 МиБ это гарантированный `has overflowed its stack`.
    // Без наших rayon-воркеров воровать нечего, вложенность ограничена.
    let lang = LangFilter::from_env();
    let skipped = AtomicUsize::new(0);
    let next = AtomicUsize::new(0);
    type Outcome = Result<BookOutcome>;
    let collected: Mutex<Vec<(usize, Outcome)>> = Mutex::new(Vec::with_capacity(total));

    std::thread::scope(|scope| {
        for _ in 0..threads.min(total.max(1)) {
            let stage_dir = &stage_dir;
            let covers_dir = &covers_dir;
            let image = &settings.image;
            let done = &done;
            let failed = &failed;
            let skipped = &skipped;
            let lang = &lang;
            let defects = &defects;
            let next = &next;
            let collected = &collected;
            let fb2_files = &fb2_files;
            std::thread::Builder::new()
                .stack_size(stack)
                .spawn_scoped(scope, move || loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(fb2) = fb2_files.get(i) else { break };
                    let outcome = process_one(fb2, stage_dir, covers_dir, image, *lang);
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    match &outcome {
                        Ok(BookOutcome::Foreign(foreign)) => {
                            // Нерусская книга: в архив не идёт, но попадает в
                            // отчёт — чтобы решение можно было перепроверить.
                            skipped.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(WorkerMsg::Progress {
                                done: n,
                                total,
                                status: format!(
                                    "{} — нерусская книга ({}), пропущена",
                                    foreign.title, foreign.reason
                                ),
                                error: None,
                            });
                        }
                        Ok(BookOutcome::Ready(book, warnings)) => {
                            // Битые ссылки — дефект исходного FB2, а не сбой
                            // пайплайна: книгу оставляем, но говорим вслух.
                            if !warnings.is_empty() {
                                defects.fetch_add(warnings.len(), Ordering::Relaxed);
                                for w in warnings {
                                    eprintln!("[!] {w}");
                                }
                            }
                            let _ = tx.send(WorkerMsg::Progress {
                                done: n,
                                total,
                                status: format!("{} — {}", book.title, book.author),
                                error: None,
                            });
                        }
                        Err(err) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(WorkerMsg::Progress {
                                done: n,
                                total,
                                status: format!("{} — пропущено", short_name(fb2)),
                                error: Some(format!("{err:#}")),
                            });
                        }
                    }
                    collected.lock().unwrap().push((i, outcome));
                })
                .expect("не удалось создать поток импорта");
        }
    });

    let mut results = collected.into_inner().unwrap_or_else(|e| e.into_inner());
    results.sort_by_key(|(i, _)| *i);

    let mut batch: Vec<BookMetadata> = Vec::with_capacity(total);
    let mut foreign: Vec<ForeignBook> = Vec::new();
    for (_, r) in results {
        // Ошибку уже отправили в UI выше.
        match r {
            Ok(BookOutcome::Ready(book, _warnings)) => batch.push(*book),
            Ok(BookOutcome::Foreign(f)) => foreign.push(*f),
            Err(_) => {}
        }
    }

    // Отчёт по отброшенным книгам: решение принималось по буквам текста, но
    // проверить его человеком никто не мешает — поэтому пишем и цифры, по
    // которым оно принято.
    if !foreign.is_empty() {
        let report = data_dir.join("skipped_foreign.txt");
        let mut text = String::from(
            "Книги, отброшенные фильтром «только русские». Решение принято\n\
             по буквам текста, а не по названию: русская книга с английским\n\
             заголовком остаётся, а эта — нет.\n\n",
        );
        for f in &foreign {
            text.push_str(&format!(
                "{} | {} | {} | {} | букв {}, кириллицы {:.1}%, нерусских кириллических {:.2}%\n",
                f.id,
                f.title,
                f.author,
                f.reason,
                f.stats.letters(),
                f.stats.cyrillic_share() * 100.0,
                f.stats.foreign_share() * 100.0,
            ));
        }
        let _ = fs::write(&report, text);
        eprintln!(
            "[язык] отброшено нерусских книг: {} — список в {}",
            foreign.len(),
            report.display()
        );
    }

    // ---- упаковка всего ZIP в один ARX -------------------------------------
    //
    // ZIP уже приходит нарезанным по диапазону id, поэтому резать его ещё раз
    // на батчи незачем: получается один архив на весь входной файл.
    if !batch.is_empty() {
        let from_id = batch.iter().map(|b| b.id).min().unwrap_or(0);
        let to_id = batch.iter().map(|b| b.id).max().unwrap_or(0);
        let archive_name = format!("{from_id}-{to_id}.arx");
        let archive_path = archives_dir.join(&archive_name);
        let t_phase = Instant::now();
        pack_arx(&stage_dir, &archive_path, &settings.arx_compression)
            .with_context(|| format!("не удалось упаковать архив {archive_name}"))?;
        tally(&PROFILE.pack, t_phase);
        let _ = fs::remove_dir_all(&stage_dir);

        for book in batch.iter_mut() {
            book.archive = archive_name.clone();
        }
        let _ = tx.send(WorkerMsg::Batch {
            archive: archive_name,
            from_id,
            to_id,
            books: batch.len(),
        });
        all_books.extend(batch);
    }

    // ---- шаг 4: индекс и метаданные ----------------------------------------
    let t_phase = Instant::now();
    let library = crate::db::Library::open(data_dir)?;
    library.append_metadata(&all_books)?;
    {
        let mut writer = library.writer()?;
        library.index_books(&mut writer, &all_books)?;
        writer.wait_merging_threads().ok();
    }
    tally(&PROFILE.index, t_phase);

    // ---- шаг 5: уборка -----------------------------------------------------
    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_dir_all(&stage);

    PROFILE
        .wall
        .store(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    print_profile(threads);
    let _ = tx.send(WorkerMsg::Finished {
        books: all_books.len(),
        failed: failed.load(Ordering::Relaxed),
        skipped: skipped.load(Ordering::Relaxed),
        defects: defects.load(Ordering::Relaxed),
        seconds: started.elapsed().as_secs_f64(),
    });
    Ok(())
}

/// Обработать одну книгу: конвертация → 100 % WebP → проверка.
fn process_one(
    fb2: &Path,
    work_dir: &Path,
    covers_dir: &Path,
    cfg: &ImageOptConfig,
    lang: LangFilter,
) -> Result<BookOutcome> {
    let stem = file_stem(fb2);
    let id = parse_id(&stem);
    let name = format!("{stem}.epub");
    let converted = work_dir.join(format!("{stem}.converted.epub"));
    let optimized = work_dir.join(&name);

    // Метаданные и обложка читаются прямо из FB2 — до конвертации.
    let meta = read_fb2_meta(fb2).unwrap_or_default();

    // ---- фильтр языка: строго до всей дорогой работы -----------------------
    //
    // Язык определяется по буквам самого текста, а не по названию. Название и
    // аннотация на английском у русской книги — обычное дело, поэтому
    // метаданным здесь верить нельзя.
    if lang.enabled {
        match fs::read(fb2) {
            Ok(bytes) => {
                let stats = crate::fb2epub::script_stats(&bytes);
                if let Some(verdict) = lang.judge(&stats) {
                    return Ok(BookOutcome::Foreign(Box::new(ForeignBook {
                        id,
                        title: meta.title.clone(),
                        author: meta.authors.first().cloned().unwrap_or_default(),
                        reason: verdict.reason(),
                        stats,
                    })));
                }
            }
            Err(err) => eprintln!(
                "[!] не удалось прочитать {} для проверки языка: {err}",
                fb2.display()
            ),
        }
    }

    // ---- конвертация: нативный конвертер, без внешних процессов ------------
    let t_phase = Instant::now();
    let report = crate::fb2epub::convert(fb2, &converted)
        .with_context(|| format!("не удалось сконвертировать {}", fb2.display()))?;
    tally(&PROFILE.convert, t_phase);
    if fs::metadata(&converted)?.len() == 0 {
        bail!("конвертер создал пустой файл для {}", fb2.display());
    }
    // Дефекты исходного FB2, которые видно в тексте: ссылка на примечание или
    // на иллюстрацию, которой в файле нет. Это не сбой, но и не молчание.
    let mut warnings = Vec::new();
    for note in &report.missing_notes {
        warnings.push(format!("{name}: ссылка на примечание `{note}`, которого нет в FB2"));
    }
    for img in &report.missing_images {
        warnings.push(format!("{name}: ссылка на иллюстрацию `{img}`, которой нет в FB2"));
    }
    for anchor in &report.missing_anchors {
        warnings.push(format!("{name}: внутренняя ссылка на отсутствующий якорь `{anchor}`"));
    }

    // ---- оптимизация картинок: WebP/AVIF + уменьшение ----------------------
    let t_phase = Instant::now();
    let stats = images::optimize_epub(&converted, &optimized, cfg)
        .with_context(|| format!("оптимизация картинок не удалась для {name}"))?;
    tally(&PROFILE.images, t_phase);

    let (images_total, images_webp, images_avif) = images::count_images(&optimized)?;
    if images_total != images_webp + images_avif {
        bail!(
            "{name}: из {images_total} картинок только {} в WebP/AVIF — \
             требование «100 % WebP/AVIF» нарушено",
            images_webp + images_avif
        );
    }

    // Ссылки на картинки, которых в EPUB нет. Это дефект исходного FB2
    // (битый base64), а не сбой конвертации — поэтому не ошибка, но и не молчание.
    for (from, target) in images::find_dangling_refs(&optimized).unwrap_or_default() {
        warnings.push(format!("{name}: {from} ссылается на отсутствующий {target}"));
    }

    let _ = fs::remove_file(&converted);

    // ---- обложка -----------------------------------------------------------
    // Берём готовый AVIF из оптимизированного EPUB: он закодирован тем же
    // энкодером и с тем же качеством, что нужны обложке, а повторное кодирование
    // из исходника стоило около 12 % всей процессорной работы импорта.
    let t_phase = Instant::now();
    let cover = save_cover(&optimized, id, covers_dir).ok().flatten();
    tally(&PROFILE.cover, t_phase);

    // ---- оглавление --------------------------------------------------------
    // Берём из готового EPUB: навигационный документ (nav.xhtml) построен по
    // секциям FB2, поэтому совпадает с тем, что покажет читалка.
    let t_phase = Instant::now();
    let chapters = read_chapters(&optimized).unwrap_or_default();
    tally(&PROFILE.chapters, t_phase);

    // Подробный отчёт по картинкам: включается переменной окружения, чтобы не
    // шуметь в обычном импорте, но был доступен для сравнения вариантов сжатия.
    if std::env::var_os("LIBRARY_IMAGE_STATS").is_some() {
        eprintln!(
            "[img] {name}: {} картинок (webp {}, avif {}), {} → {} Б, уменьшено {}, \
             near-lossless {}, сбоев {}",
            stats.images,
            stats.webp,
            stats.avif,
            stats.bytes_before,
            stats.bytes_after,
            stats.resized,
            stats.near_lossless,
            stats.failed,
        );
    }

    let details = Details {
        authors: meta.authors.clone(),
        annotation: meta.annotation.clone(),
        keywords: meta.keywords.clone(),
        lang: meta.lang.clone(),
        src_lang: meta.src_lang.clone(),
        translators: meta.translators.clone(),
        series: meta.series.clone(),
        series_number: meta.series_number.clone(),
        date: meta.date.clone(),
        publisher: meta.publisher.clone(),
        city: meta.city.clone(),
        year: meta.year.clone(),
        isbn: meta.isbn.clone(),
        doc_id: meta.doc_id.clone(),
        doc_version: meta.doc_version.clone(),
        doc_date: meta.doc_date.clone(),
        program_used: meta.program_used.clone(),
        src_url: meta.src_url.clone(),
        src_ocr: meta.src_ocr.clone(),
        chapters,
    };

    let size = fs::metadata(&optimized)?.len();
    Ok(BookOutcome::Ready(Box::new(BookMetadata {
        id,
        title: if meta.title.is_empty() {
            stem.clone()
        } else {
            meta.title.clone()
        },
        author: meta.authors.first().cloned().unwrap_or_default(),
        // Коды храним как есть, названия посчитаются при загрузке.
        genres: genres::names(&meta.genre_codes),
        genre_codes: meta.genre_codes.clone(),
        cover,
        details,
        // Имя архива проставится после упаковки батча.
        archive: String::new(),
        internal_path: name,
        size,
    }), warnings))
}

/// Упаковать каталог в `.arx` (`--dir-as-root`, чтобы EPUB лежали в корне).
/// Упаковать папку в `.arx` и вернуть размер архива.
pub fn make_arx(dir: &Path, archive: &Path, compression: &str) -> Result<u64> {
    if archive.exists() {
        fs::remove_file(archive)?;
    }
    let output = Command::new("arx")
        .arg("create")
        .arg("-f")
        .arg("-c")
        .arg(compression)
        .arg("--dir-as-root")
        .arg("-o")
        .arg(archive)
        .arg(dir)
        .output()
        .context("не удалось запустить `arx create`")?;
    if !output.status.success() {
        bail!(
            "arx create -c {compression} завершился с ошибкой ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let size = fs::metadata(archive)?.len();
    if size == 0 {
        bail!("arx create создал пустой архив {}", archive.display());
    }
    Ok(size)
}

fn pack_arx(dir: &Path, archive: &Path, compression: &str) -> Result<()> {
    make_arx(dir, archive, compression).map(|_| ())
}

/// Распаковать только `.fb2` из ZIP, защитившись от выхода за пределы папки.
pub(crate) fn unzip_fb2(zip: &Path, dest: &Path) -> Result<Vec<PathBuf>> {
    let file = fs::File::open(zip)?;
    let mut archive = zip::ZipArchive::new(file).context("это не ZIP-архив")?;
    let mut out = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if !entry.is_file() {
            continue;
        }
        let raw = entry.name().to_string();
        if !raw.to_lowercase().ends_with(".fb2") {
            continue;
        }
        // basename: архивы lib.rus.ec плоские, но полагаться на это нельзя
        let base = raw.rsplit(['/', '\\']).next().unwrap_or(&raw).to_string();
        if base.is_empty() || base.contains("..") {
            continue;
        }
        let path = dest.join(&base);
        let mut sink = fs::File::create(&path)
            .with_context(|| format!("не удалось создать {}", path.display()))?;
        io::copy(&mut entry, &mut sink)?;
        out.push(path);
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Метаданные FB2
// ---------------------------------------------------------------------------

/// Всё, что удалось достать из `<description>` файла FB2.
#[derive(Debug, Default, Clone)]
struct Fb2Meta {
    title: String,
    /// Все авторы из `<title-info>` (первый — основной).
    authors: Vec<String>,
    /// Коды жанров как в FB2 (`sci_history`, `popadancy`), в порядке файла.
    genre_codes: Vec<String>,
    annotation: String,
    keywords: String,
    lang: String,
    src_lang: String,
    translators: Vec<String>,
    /// `<sequence name="..." number="..."/>` из `<title-info>`.
    series: String,
    series_number: String,
    /// Год написания/издания.
    date: String,
    publisher: String,
    city: String,
    year: String,
    isbn: String,
    // --- document-info ---
    doc_id: String,
    doc_version: String,
    doc_date: String,
    program_used: String,
    src_url: String,
    src_ocr: String,
}

/// Достать метаданные из FB2. Формат XML, но namespace-префиксы встречаются
/// произвольно, поэтому ищем по локальному имени тега.
fn read_fb2_meta(fb2: &Path) -> Result<Fb2Meta> {
    // Читаем только начало файла: `<description>` всегда идёт первым,
    // а картинки в base64 лежат ниже и читать их незачем.
    let text = read_prefix(fb2, 512 * 1024)?;

    let title_info = tag_block(&text, "title-info").unwrap_or("");
    let doc_info = tag_block(&text, "document-info").unwrap_or("");
    let pub_info = tag_block(&text, "publish-info").unwrap_or("");

    let mut meta = Fb2Meta {
        title: tag_text(&text, "book-title").unwrap_or_default(),
        authors: author_list(title_info),
        genre_codes: tag_texts(title_info, "genre"),
        annotation: annotation_text(title_info),
        keywords: tag_text(title_info, "keywords").unwrap_or_default(),
        lang: tag_text(title_info, "lang").unwrap_or_default(),
        src_lang: tag_text(title_info, "src-lang").unwrap_or_default(),
        translators: author_list_of(title_info, "translator"),
        series: tag_attr(title_info, "sequence", "name").unwrap_or_default(),
        series_number: tag_attr(title_info, "sequence", "number").unwrap_or_default(),
        // Год: у `<date>` бывает атрибут `value` с полной датой.
        date: tag_attr(title_info, "date", "value")
            .filter(|v| !v.is_empty())
            .or_else(|| tag_text(title_info, "date"))
            .unwrap_or_default(),
        publisher: tag_text(pub_info, "publisher").unwrap_or_default(),
        city: tag_text(pub_info, "city").unwrap_or_default(),
        year: tag_text(pub_info, "year").unwrap_or_default(),
        isbn: tag_text(pub_info, "isbn").unwrap_or_default(),
        doc_id: tag_text(doc_info, "id").unwrap_or_default(),
        doc_version: tag_text(doc_info, "version").unwrap_or_default(),
        doc_date: tag_attr(doc_info, "date", "value")
            .or_else(|| tag_text(doc_info, "date"))
            .unwrap_or_default(),
        program_used: tag_text(doc_info, "program-used").unwrap_or_default(),
        src_url: tag_text(doc_info, "src-url").unwrap_or_default(),
        src_ocr: tag_text(doc_info, "src-ocr").unwrap_or_default(),
    };

    // Если год не нашли в publish-info, берём его из даты.
    if meta.year.is_empty() {
        meta.year = year_of(&meta.date);
    }
    Ok(meta)
}

/// Год из строки даты (`2005-01-01` → `2005`).
fn year_of(date: &str) -> String {
    let digits: String = date.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() == 4 {
        digits
    } else {
        String::new()
    }
}

/// Аннотация: абзацы через перевод строки, теги убраны.
fn annotation_text(title_info: &str) -> String {
    let Some(block) = tag_block(title_info, "annotation") else {
        return String::new();
    };
    let with_breaks = block
        .replace("</p>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");
    strip_tags(&with_breaks)
}

/// Все авторы из блока (первый — основной).
fn author_list(block: &str) -> Vec<String> {
    author_list_of(block, "author")
}

/// Все авторы указанного тега (`author`, `translator`) из блока.
fn author_list_of(block: &str, tag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let close = format!("</{tag}>");
    let open = format!("<{tag}");
    let mut from = 0usize;
    while let Some(p) = block[from..].find(&open) {
        let start = from + p;
        let Some(gt) = block[start..].find('>') else { break };
        let body_start = start + gt + 1;
        let Some(end) = block[body_start..].find(&close) else { break };
        let body = &block[body_start..body_start + end];

        let mut parts = Vec::new();
        for t in ["first-name", "middle-name", "last-name", "nickname"] {
            if let Some(v) = tag_text(body, t) {
                if !v.is_empty() {
                    parts.push(v);
                }
            }
        }
        if !parts.is_empty() {
            out.push(parts.join(" "));
        }
        from = body_start + end;
    }
    out
}

/// Прочитать не больше `limit` байт (FB2 с картинками бывают по 20 МБ).
fn read_prefix(path: &Path, limit: usize) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut buf = vec![0u8; limit];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    // Кодировку разбирает тот же код, что и конвертер: в корпусе встречаются
    // FB2 в windows-1251, и `from_utf8_lossy` превратил бы их в кашу.
    Ok(crate::fb2epub::decode_fb2(&buf))
}

/// Внутренний текст первого тега с таким локальным именем (сырой).
fn tag_block<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut from = 0usize;
    while from < xml.len() {
        let rest = &xml[from..];
        let lt = rest.find('<')?;
        let after = &rest[lt + 1..];
        if after.starts_with('/') || after.starts_with('?') || after.starts_with('!') {
            from += lt + 1;
            continue;
        }
        let Some(name_end) = after
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        else {
            break;
        };
        let full_name = &after[..name_end];
        let local = full_name.rsplit(':').next().unwrap_or(full_name);
        let Some(gt) = after.find('>') else { break };
        let self_closing = after[..gt].trim_end().ends_with('/');
        let value_start = from + lt + 1 + gt + 1;

        if local.eq_ignore_ascii_case(tag) && !self_closing {
            let close = format!("</{full_name}>");
            if let Some(end) = xml[value_start..].find(&close) {
                return Some(&xml[value_start..value_start + end]);
            }
        }
        from = value_start;
    }
    None
}

/// Тексты всех тегов с таким локальным именем (без учёта namespace).
fn tag_texts(xml: &str, tag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while from < xml.len() {
        let rest = &xml[from..];
        let Some(lt) = rest.find('<') else { break };
        let after = &rest[lt + 1..];
        // пропускаем закрывающие и служебные теги
        if after.starts_with('/') || after.starts_with('?') || after.starts_with('!') {
            from += lt + 1;
            continue;
        }
        let name_end = after
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(after.len());
        let full_name = &after[..name_end];
        let local = full_name.rsplit(':').next().unwrap_or(full_name);
        let Some(gt) = after.find('>') else { break };
        let self_closing = after[..gt].trim_end().ends_with('/');
        let value_start = from + lt + 1 + gt + 1;

        // `<genre/>` без содержимого пропускаем: иначе «значением» станет
        // всё до следующего закрывающего тега.
        if local.eq_ignore_ascii_case(tag) && !self_closing {
            let close = format!("</{full_name}>");
            let tail = &xml[value_start..];
            if let Some(end) = tail.find(&close).or_else(|| tail.find("</")) {
                let value = unescape(tail[..end].trim());
                if !value.is_empty() {
                    out.push(value);
                }
            }
        }
        from = value_start;
    }
    out
}

/// Текст первого тега с таким локальным именем.
fn tag_text(xml: &str, tag: &str) -> Option<String> {
    tag_texts(xml, tag).into_iter().next()
}

/// Значение атрибута у первого тега `tag` (например, `number` у `<sequence>`).
fn tag_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut from = 0usize;
    while let Some(p) = xml[from..].find(&open) {
        let start = from + p;
        let Some(gt) = xml[start..].find('>') else {
            break;
        };
        let head = &xml[start..start + gt];
        // отсекаем `sequence` от `sequence_number` и т.п.
        let name: String = head[1..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '-'))
            .collect();
        if name.rsplit(':').next().unwrap_or(&name).eq_ignore_ascii_case(tag) {
            if let Some(v) = attr_in(head, attr) {
                return Some(v);
            }
        }
        from = start + gt + 1;
    }
    None
}

/// Вытащить `attr="значение"` из строки открывающего тега.
fn attr_in(head: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=");
    let p = head.find(&needle)? + needle.len();
    let rest = &head[p..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let body = &rest[1..];
        let end = body.find(quote)?;
        Some(unescape(body[..end].trim()))
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(unescape(rest[..end].trim()))
    }
}

/// Убрать теги и схлопнуть пробелы, сохранив переводы строк.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    let unescaped = unescape(&out);
    unescaped
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Минимальное раскодирование XML-сущностей — этого хватает для названий.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Оглавление из EPUB
// ---------------------------------------------------------------------------

/// Достать список глав из готового EPUB: сначала `toc.ncx`, потом `nav.xhtml`.
///
/// EPUB — это ZIP, поэтому оглавление читается из уже собранного файла:
/// Calibre строит его по секциям FB2, и оно совпадает с тем, что увидит читалка.
fn read_chapters(epub: &Path) -> Result<Vec<String>> {
    let file = fs::File::open(epub)?;
    let mut archive = zip::ZipArchive::new(file)?;

    let mut ncx: Option<String> = None;
    let mut nav: Option<String> = None;
    for name in archive.file_names() {
        let lower = name.to_lowercase();
        if lower.ends_with(".ncx") {
            if ncx.is_none() {
                ncx = Some(name.to_string());
            }
        } else if (lower.ends_with("nav.xhtml") || lower.ends_with("toc.xhtml")) && nav.is_none() {
            nav = Some(name.to_string());
        }
    }

    if let Some(name) = ncx {
        let mut text = String::new();
        use std::io::Read;
        archive.by_name(&name)?.read_to_string(&mut text)?;
        let chapters = chapters_from_ncx(&text);
        if !chapters.is_empty() {
            return Ok(chapters);
        }
    }
    if let Some(name) = nav {
        let mut text = String::new();
        use std::io::Read;
        archive.by_name(&name)?.read_to_string(&mut text)?;
        return Ok(chapters_from_nav(&text));
    }
    Ok(Vec::new())
}

/// Главы из `toc.ncx`: текст внутри `<navLabel><text>…</text></navLabel>`.
fn chapters_from_ncx(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(p) = xml[from..].find("<navLabel") {
        let start = from + p;
        let Some(gt) = xml[start..].find('>') else { break };
        let body_start = start + gt + 1;
        let Some(end) = xml[body_start..].find("</navLabel>") else {
            break;
        };
        let body = &xml[body_start..body_start + end];
        if let Some(t) = tag_text(body, "text") {
            if !t.is_empty() {
                out.push(t);
            }
        }
        from = body_start + end;
    }
    out
}

/// Главы из `nav.xhtml`: подписи ссылок внутри `<nav>`.
fn chapters_from_nav(xml: &str) -> Vec<String> {
    let Some(nav) = tag_block(xml, "nav") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(p) = nav[from..].find("<a ") {
        let start = from + p;
        let Some(gt) = nav[start..].find('>') else { break };
        let body_start = start + gt + 1;
        let Some(end) = nav[body_start..].find("</a>") else {
            break;
        };
        let title = strip_tags(&nav[body_start..body_start + end]).replace('\n', " ");
        if !title.trim().is_empty() {
            out.push(title.trim().to_string());
        }
        from = body_start + end;
    }
    out
}

// ---------------------------------------------------------------------------
// Обложка
// ---------------------------------------------------------------------------

/// Положить обложку из готового EPUB в `data/covers/<id>.avif`.
///
/// Отдельного кодирования здесь нет намеренно: `optimize_epub` уже записала
/// обложку в EPUB как AVIF с `cover_quality`. Второй проход (декодировать
/// исходник, уменьшить, сжать заново) давал тот же кадр за ~12 % всего
/// процессорного времени импорта.
fn save_cover(epub: &Path, id: u64, covers_dir: &Path) -> Result<Option<String>> {
    let Some((source_name, bytes)) = images::cover_bytes(epub)? else {
        return Ok(None);
    };
    // Расширение берём у настоящей записи EPUB, а не пишем `.avif` всегда:
    // обложка теперь кодируется тем же форматом, что и остальные картинки, и
    // файл `821884.avif` с содержимым WebP ломал бы определение формата.
    let ext = source_name
        .rsplit('.')
        .next()
        .filter(|e| !e.is_empty() && *e != source_name)
        .unwrap_or("img");
    // Обложки лежат в подкаталогах: на миллион книг плоский каталог — это
    // миллион файлов в одном `readdir`, что плохо и для файловой системы, и
    // для бэкапа. См. `db::Library::cover_rel_path`.
    let file_name = crate::db::Library::cover_rel_path(id, ext);
    if let Some(parent) = covers_dir.join(&file_name).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(covers_dir.join(&file_name), &bytes)
        .with_context(|| format!("не удалось записать обложку {file_name}"))?;
    Ok(Some(file_name))
}

// ---------------------------------------------------------------------------
// Мелочи
// ---------------------------------------------------------------------------

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "book".to_string())
}

fn parse_id(stem: &str) -> u64 {
    stem.trim()
        .rsplit(['-', '_'])
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| stem.parse::<u64>().ok())
        .unwrap_or(0)
}

fn short_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}


#[cfg(test)]
mod lang_tests {
    use super::*;
    use crate::fb2epub::script_stats;

    fn filter() -> LangFilter {
        LangFilter {
            enabled: true,
            keep_cyrillic: false,
        }
    }

    #[test]
    fn keeps_russian_and_drops_latvian() {
        let ru = "Это русская книга с английским названием и аннотацией. ".repeat(10);
        let lv = "Sidraba vītols ir latviešu grāmata, tā nav krieviska. ".repeat(10);
        assert_eq!(filter().judge(&script_stats(ru.as_bytes())), None);
        assert_eq!(
            filter().judge(&script_stats(lv.as_bytes())),
            Some(LangVerdict::NotCyrillic)
        );
    }

    #[test]
    fn drops_ukrainian_but_keeps_it_when_asked() {
        let uk = "Це українська книга, і вона не російська, її треба відрізнити. ".repeat(10);
        let st = script_stats(uk.as_bytes());
        assert_eq!(filter().judge(&st), Some(LangVerdict::NotRussianCyrillic));
        let lenient = LangFilter {
            enabled: true,
            keep_cyrillic: true,
        };
        assert_eq!(lenient.judge(&st), None, "с LIBRARY_LANG_KEEP_CYRILLIC всё кириллическое остаётся");
    }

    #[test]
    fn short_text_is_never_judged() {
        // Картинка без текста или обрывок: судить не о чем, книга остаётся.
        let tiny = "Hello world";
        assert_eq!(filter().judge(&script_stats(tiny.as_bytes())), None);
    }

    #[test]
    fn disabled_filter_keeps_everything() {
        let off = LangFilter {
            enabled: false,
            keep_cyrillic: false,
        };
        let lv = "Sidraba vītols ir latviešu grāmata. ".repeat(20);
        assert_eq!(off.judge(&script_stats(lv.as_bytes())), None);
    }
}
