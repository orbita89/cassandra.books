//! Локальное GUI-приложение для библиотеки FB2/EPUB.
//!
//! Без HTTP-серверов: всё живёт в одном процессе.
//!   * UI-поток — egui/eframe, рисует кадры и НИКОГДА не блокируется;
//!   * фоновый поток — импорт ZIP (см. `worker`), общается с UI через
//!     `crossbeam-channel`;
//!   * `db` — поиск по Tantivy и извлечение книг из ARX.
//!
//! Все данные лежат в `data/` (путь можно передать первым аргументом).

mod author_suggest;
mod db;
mod dedup;
mod fb2epub;
mod genres;
mod images;
mod ui;
mod worker;

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver, Sender};
use db::{BookMetadata, Library};
use images::ImageOptConfig;
use std::collections::VecDeque;
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

/// Короткое имя файла для интерфейса: без каталогов.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
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
    /// Имя архива, по которому получен `finished` (он уже импортирован).
    pub finished_name: Option<String>,
    /// Имя архива, который импортируется сейчас.
    pub current: Option<String>,
    /// Имена архивов, ждущих своей очереди.
    pub queue: Vec<String>,
    /// Номер текущего архива в пакете, с единицы.
    pub index: usize,
    /// Сколько всего архивов в пакете. Растёт, если добавить файлы на ходу.
    pub files: usize,
    /// Исходники, удалённые после успешного импорта.
    pub deleted: Vec<String>,
    /// Исходники, оставленные из-за ошибок отдельных книг.
    pub kept: Vec<String>,
    /// Архивы, которые не удалось импортировать вовсе (фатальная ошибка).
    pub failed_files: Vec<String>,
    /// Итог по всему пакету: (книги, ошибки, секунды).
    pub batch: (usize, usize, f64),
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

/// Окно выбора ZIP-архивов: список файлов каталога с галочками.
///
/// Нужно потому, что системный диалог отмечает несколько файлов только
/// Ctrl+кликом и не показывает, что уже импортировано. Здесь — обычные
/// галочки, размер и пометка «уже импортирован».
#[derive(Debug)]
pub struct ZipPicker {
    /// Каталог, содержимое которого показано.
    pub dir: PathBuf,
    /// Подстрока для фильтра по имени.
    pub filter: String,
    /// Найденные ZIP-архивы, по алфавиту.
    pub entries: Vec<ZipEntry>,
    /// Ошибка чтения каталога — показывается в окне.
    pub error: Option<String>,
}

/// Один ZIP в списке выбора.
#[derive(Debug)]
pub struct ZipEntry {
    /// Полный путь.
    pub path: PathBuf,
    /// Имя файла без каталогов.
    pub name: String,
    /// Размер в байтах.
    pub size: u64,
    /// Похоже, что этот диапазон уже собран: в `data/archives` есть `.arx`.
    pub imported: bool,
    /// Стоит ли галочка.
    pub checked: bool,
}

impl ZipPicker {
    /// Прочитать каталог и собрать список ZIP.
    pub fn open(dir: PathBuf, archives_dir: &Path) -> Self {
        let mut picker = Self {
            dir,
            filter: String::new(),
            entries: Vec::new(),
            error: None,
        };
        picker.rescan(archives_dir);
        picker
    }

    /// Перечитать каталог, сохранив уже поставленные галочки.
    pub fn rescan(&mut self, archives_dir: &Path) {
        let previous: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.checked)
            .map(|e| e.path.clone())
            .collect();
        match scan_zips(&self.dir, archives_dir) {
            Ok(mut entries) => {
                for entry in &mut entries {
                    entry.checked = previous.contains(&entry.path);
                }
                self.entries = entries;
                self.error = None;
            }
            Err(err) => {
                self.entries.clear();
                self.error = Some(err);
            }
        }
    }

    /// Номера записей, подходящих под фильтр (без учёта регистра).
    pub fn visible_indices(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| needle.is_empty() || e.name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// Поставить или снять галочки у всего, что видно сейчас.
    pub fn set_visible(&mut self, checked: bool) {
        for i in self.visible_indices() {
            self.entries[i].checked = checked;
        }
    }

    /// Отметить видимые архивы, которых ещё нет в библиотеке.
    pub fn check_only_new(&mut self) {
        for i in self.visible_indices() {
            self.entries[i].checked = !self.entries[i].imported;
        }
    }

    /// Сколько выбрано: (файлов, байт).
    pub fn checked_stats(&self) -> (usize, u64) {
        self.entries
            .iter()
            .filter(|e| e.checked)
            .fold((0, 0), |(n, bytes), e| (n + 1, bytes + e.size))
    }

    /// Пути выбранных архивов в порядке списка.
    pub fn checked_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|e| e.checked)
            .map(|e| e.path.clone())
            .collect()
    }
}

/// Найти в каталоге все ZIP-архивы.
fn scan_zips(dir: &Path, archives_dir: &Path) -> std::result::Result<Vec<ZipEntry>, String> {
    let rd = std::fs::read_dir(dir)
        .map_err(|err| format!("не удалось прочитать {}: {err}", dir.display()))?;
    let archives = archive_names(archives_dir);
    let mut entries = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() || !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        // Диапазон id из имени файла — подсказка, а не приговор: галочку
        // всё равно ставит человек, и переимпорт по-прежнему возможен.
        let imported = id_range(&name).is_some_and(|(lo, hi)| {
            let base = format!("{lo}-{hi}");
            let prefixed = format!("{base}-");
            archives.iter().any(|a| *a == base || a.starts_with(&prefixed))
        });
        entries.push(ZipEntry {
            path,
            name,
            size,
            imported,
            checked: false,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// Имена собранных партий (`821884-824013`, `1-1-2`) без расширения `.arx`.
fn archive_names(archives_dir: &Path) -> Vec<String> {
    std::fs::read_dir(archives_dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".arx").map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Диапазон id из имени файла: `fb2-806448-808070.zip` → `(806448, 808070)`.
///
/// В корпусе lib.rus.ec имя архива — это диапазон id, и партия импорта
/// называется так же. Если имя разобрать не удалось, подсказки просто нет.
fn id_range(file_name: &str) -> Option<(u64, u64)> {
    let stem = file_name.rsplit_once('.').map_or(file_name, |(s, _)| s);
    let parts: Vec<&str> = stem.split('-').collect();
    parts.windows(2).find_map(|w| {
        match (w[0].parse::<u64>(), w[1].parse::<u64>()) {
            (Ok(lo), Ok(hi)) => Some((lo, hi)),
            _ => None,
        }
    })
}

/// Состояние выноса дубликатов.
///
/// Сам план читается заранее (кнопкой), а тут живёт то, что видно в окне:
/// подтверждение, прогресс и итог.
#[derive(Default)]
pub struct DedupState {
    /// Идёт вынос — интерфейс показывает прогресс и не даёт запустить второй.
    pub running: bool,
    /// На каком шаге: обложки, архивы, метаданные, индекс.
    pub step: String,
    /// Сколько единиц шага сделано и всего.
    pub done: usize,
    pub total: usize,
    /// Что происходит прямо сейчас.
    pub status: String,
    /// Отчёт после завершения.
    pub finished: Option<String>,
    /// Открытое окно подтверждения вместе с прочитанным планом.
    pub dialog: Option<DedupDialog>,
}

/// Окно подтверждения: что именно уедет в `dublicate/`.
pub struct DedupDialog {
    pub plan: dedup::Plan,
    pub books: usize,
    pub archives: usize,
    pub bytes: u64,
}

impl DedupState {
    /// Доля выполненного шага — для полоски прогресса.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
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
    /// Книги текущей страницы (после фильтров).
    pub results: Vec<BookMetadata>,
    /// Номер страницы выдачи, с нуля.
    pub page: usize,
    /// Сколько книг показывать на странице.
    pub page_size: usize,
    /// Сколько всего книг подошло под текстовый запрос (до фильтров).
    pub matched: usize,
    /// Сколько книг прошло фильтры — по этому числу считается листание.
    pub filtered: usize,
    /// Показанная выдача собрана резервным запросом с одной опечаткой, то есть
    /// совпадения приблизительные. См. [`db::SearchOutcome::approximate`].
    pub approximate: bool,
    /// Фильтр по автору — точное совпадение; `None` значит «все».
    pub filter_author: Option<String>,
    /// Текст в поле автодополнения автора. Подсказки берутся из
    /// [`App::author_dictionary`], поэтому ввод сюда не трогает поиск.
    pub author_query: String,
    /// Полный словарь авторов индекса: подсказки по началу любого слова.
    ///
    /// Собирается из индекса один раз при старте и после перезагрузки
    /// библиотеки (импорт, вынос дубликатов), а не в `refresh_search`: иначе
    /// каждый запрос заново читал бы словарь термов Tantivy.
    pub author_dictionary: author_suggest::AuthorDictionary,
    /// Фильтр по жанру — точное совпадение; `None` значит «все».
    pub filter_genre: Option<String>,
    /// Показывать статьи и заметки: по умолчанию они спрятаны, см.
    /// [`db::SHORT_WORDS`].
    pub show_short: bool,
    /// Какие книги показывать по обложке: всё, только с обложкой или только
    /// без обложки. См. [`db::CoverFilter`].
    pub cover_filter: db::CoverFilter,
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
    /// ZIP-архивы, ждущие импорта. Обрабатываются строго по одному.
    pub import_queue: VecDeque<PathBuf>,
    /// Открытое окно выбора архивов (галочками), если оно открыто.
    pub zip_picker: Option<ZipPicker>,
    /// Каталог, который показывать в окне выбора архивов.
    pub zip_dir: Option<PathBuf>,
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
    /// Вынос дубликатов.
    pub dedup: DedupState,
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
            page: 0,
            page_size: db::DEFAULT_SEARCH_LIMIT,
            matched: 0,
            filtered: 0,
            approximate: false,
            filter_author: None,
            author_query: String::new(),
            author_dictionary: author_suggest::AuthorDictionary::empty(),
            filter_genre: None,
            show_short: false,
            cover_filter: db::CoverFilter::All,
            authors: Vec::new(),
            genres: Vec::new(),
            selected: None,
            details_open: false,
            cache: (0, 0),
            import: ImportState::default(),
            import_queue: VecDeque::new(),
            zip_picker: None,
            zip_dir: None,
            error: None,
            toast: None,
            rx,
            tx,
            // В GUI автоудаление исходников включено: пользователь просил
            // освобождать место после импорта. Галочка рядом с кнопкой
            // импорта это выключает.
            settings: ImportSettings {
                delete_zip: true,
                ..ImportSettings::default()
            },
            dedup: DedupState::default(),
        };
        // Словарь авторов — до первого кадра: в интерфейсе он уже нужен, а
        // поиск от него не зависит, поэтому ошибку сборки показываем окном,
        // но приложение не останавливаем.
        app.rebuild_author_dictionary();
        app.refresh_search()?;
        app.refresh_cache_usage();
        Ok(app)
    }

    /// Пересобрать словарь авторов из индекса.
    ///
    /// Словарь зависит только от индекса, а не от текущей выдачи, поэтому
    /// вызывается при старте, после [`App::reload_library`] и после импорта —
    /// но не в [`App::refresh_search`].
    pub fn rebuild_author_dictionary(&mut self) {
        match self.library.author_dictionary() {
            Ok(authors) => {
                self.author_dictionary = author_suggest::AuthorDictionary::new(authors);
            }
            Err(err) => {
                self.error = Some(format!("не удалось собрать словарь авторов: {err:#}"));
            }
        }
    }

    /// Сколько страниц в текущей выдаче (минимум одна).
    pub fn pages(&self) -> usize {
        self.filtered.div_ceil(self.page_size.max(1)).max(1)
    }

    /// Открыть библиотеку заново после выноса дубликатов.
    ///
    /// Индекс к этому моменту уже пересобран фоновым потоком, поэтому здесь
    /// достаточно подменить `Library` целиком: так в памяти оказываются и
    /// новые метаданные, и новый читатель Tantivy.
    pub fn reload_library(&mut self) {
        match Library::open(&self.library.data_dir) {
            Ok(library) => {
                self.library = library;
                self.selected = None;
                self.details_open = false;
                // Индекс пересобран — словарь авторов тоже должен быть новым.
                self.rebuild_author_dictionary();
                if let Err(err) = self.refresh_search() {
                    self.error = Some(format!("{err:#}"));
                }
                self.refresh_cache_usage();
            }
            Err(err) => self.error = Some(format!("не удалось перечитать библиотеку: {err:#}")),
        }
    }

    /// Пересчитать выдачу по текущему запросу, фильтрам и номеру страницы.
    pub fn refresh_search(&mut self) -> Result<()> {
        // Новый запрос всегда открывается с первой страницы; перелистывание
        // запрос не меняет, поэтому страницу не сбрасывает.
        if self.query != self.last_query {
            self.page = 0;
        }
        self.page_size = self.page_size.clamp(1, db::MAX_PAGE_SIZE);

        loop {
            let outcome = self.library.search(
                &self.query,
                self.filter_author.as_deref(),
                self.filter_genre.as_deref(),
                self.show_short,
                self.cover_filter,
                self.page * self.page_size,
                self.page_size,
            )?;
            self.matched = outcome.matched;
            self.filtered = outcome.filtered;
            // Признак пересчитывается на каждом проходе: если страница
            // «съехала» и поиск повторится, пометка обязана соответствовать
            // последней выдаче.
            self.approximate = outcome.approximate;
            let pages = self.pages();
            // Страница могла исчезнуть — например, после смены фильтра книг
            // стало меньше. Тогда показываем последнюю, а не пустоту.
            if self.page >= pages {
                self.page = pages - 1;
                continue;
            }
            self.results = outcome.books;
            self.authors = outcome.authors;
            self.genres = outcome.genres;
            break;
        }
        self.last_query = self.query.clone();

        // Выбранная книга могла исчезнуть из выдачи после смены фильтра.
        if self.selected.map(|i| i >= self.results.len()).unwrap_or(false) {
            self.selected = None;
        }
        Ok(())
    }

    /// Сбросить фильтры по автору и жанру и очистить поле автодополнения.
    pub fn clear_filters(&mut self) {
        self.filter_author = None;
        self.author_query.clear();
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

    /// Поставить ZIP-архивы в очередь на импорт.
    ///
    /// Архивы обрабатываются строго по одному: пока идёт текущий, новые
    /// просто ждут. Если импорт не запущен, первый стартует сразу. Вызывать
    /// можно и во время импорта — тогда файлы добавятся в конец очереди.
    pub fn start_imports(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.import_queue.extend(paths);
        if self.import.running {
            self.import.queue = self.import_queue.iter().map(|p| display_name(p)).collect();
            self.import.files = self.import.index + self.import_queue.len();
            return;
        }
        self.import = ImportState {
            queue: self.import_queue.iter().map(|p| display_name(p)).collect(),
            files: self.import_queue.len(),
            ..Default::default()
        };
        self.error = None;
        self.toast = None;
        self.start_next_import();
    }

    /// Открыть окно подтверждения выноса дубликатов.
    ///
    /// Сам план не считаем: его составляет `dedup.py` (там вся логика про
    /// авторов, тома и обложки), а кнопка только показывает, что уедет, и
    /// спрашивает разрешения.
    pub fn open_dedup_dialog(&mut self) {
        let Some(path) = dedup::find_plan(&self.library.data_dir) else {
            self.error = Some(format!(
                "не найден план выноса. Сначала постройте его:\n\
                 python3 dedup.py --phash\n\
                 (ожидается {} или {})",
                self.library
                    .data_dir
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(dedup::REPORT_DIR)
                    .join(dedup::PLAN_NAME)
                    .display(),
                self.library
                    .data_dir
                    .join(dedup::REPORT_DIR)
                    .join(dedup::PLAN_NAME)
                    .display(),
            ));
            return;
        };
        match dedup::Plan::load(&path) {
            Ok(plan) => {
                let (books, archives, bytes) = plan.summary();
                self.error = None;
                self.dedup.dialog = Some(DedupDialog {
                    plan,
                    books,
                    archives,
                    bytes,
                });
            }
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    /// Запустить вынос дубликатов из подтверждённого плана.
    pub fn start_dedup(&mut self) {
        let Some(dialog) = self.dedup.dialog.take() else {
            return;
        };
        if self.import.running || self.dedup.running {
            self.dedup.dialog = Some(dialog);
            return;
        }
        self.error = None;
        self.toast = None;
        self.dedup.running = true;
        self.dedup.step = "подготовка".into();
        self.dedup.done = 0;
        self.dedup.total = dialog.plan.items.len();
        self.dedup.status = format!("к выносу: {} книг", dialog.books);
        self.dedup.finished = None;
        // Потолки у импорта и у выноса разные: там 48 потоков гоняют rav1e,
        // здесь каждый поток держит распакованный архив на диске.
        let jobs = if self.settings.jobs > 0 {
            self.settings.jobs
        } else {
            dedup::default_jobs()
        };
        worker::spawn_dedup(
            dialog.plan,
            self.library.data_dir.clone(),
            jobs,
            self.tx.clone(),
        );
    }

    /// Открыть окно выбора архивов: список `.zip` в каталоге с галочками.
    pub fn open_zip_picker(&mut self) {
        let dir = self
            .zip_dir
            .clone()
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads")))
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| PathBuf::from("."));
        let archives = self.library.archives_dir();
        self.zip_picker = Some(ZipPicker::open(dir, &archives));
    }

    /// Запустить следующий архив из очереди. `false` — очередь пуста.
    fn start_next_import(&mut self) -> bool {
        let Some(zip) = self.import_queue.pop_front() else {
            return false;
        };
        // Каталог запоминаем: в следующий раз окно выбора откроется в нём.
        self.zip_dir = zip.parent().map(Path::to_path_buf);
        self.import.index += 1;
        self.import.current = Some(display_name(&zip));
        self.import.queue = self.import_queue.iter().map(|p| display_name(p)).collect();
        self.import.running = true;
        self.import.done = 0;
        self.import.total = 0;
        self.import.last_batch = None;
        self.import.status = "распаковка...".to_string();
        let data_dir = self.library.data_dir.clone();
        worker::spawn_import(zip, data_dir, self.settings.clone(), self.tx.clone());
        true
    }

    /// Забрать всё, что прислал фоновый поток, и обновить состояние.
    pub fn pump_worker(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Started { total, zip } => {
                    self.import.running = true;
                    self.import.total = total;
                    self.import.done = 0;
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
                            // Ошибки копятся по всему пакету, поэтому называем файл.
                            let prefix = self
                                .import
                                .current
                                .as_deref()
                                .map(|name| format!("{name}: "))
                                .unwrap_or_default();
                            self.import.errors.push(format!("{prefix}{err}"));
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
                    deleted,
                } => {
                    self.import.running = false;
                    self.import.skipped = skipped;
                    self.import.defects = defects;
                    self.import.finished = Some((books, failed, seconds));
                    self.import.finished_name = self.import.current.clone();
                    self.import.batch.0 += books;
                    self.import.batch.1 += failed;
                    self.import.batch.2 += seconds;
                    if let Some(name) = self.import.current.take() {
                        if deleted {
                            self.import.deleted.push(name);
                        } else if self.settings.delete_zip {
                            // Автоудаление включено, но файл остался: значит,
                            // были ошибки книг. Так и скажем в панели.
                            self.import.kept.push(name);
                        }
                    }
                    let mut tail = String::new();
                    if skipped > 0 {
                        tail.push_str(&format!(", нерусских отброшено: {skipped}"));
                    }
                    if defects > 0 {
                        tail.push_str(&format!(", битых ссылок в исходниках: {defects}"));
                    }
                    if deleted {
                        tail.push_str(", исходный ZIP удалён");
                    }
                    self.import.status =
                        format!("готово: {books} книг за {seconds:.0} с (ошибок: {failed}{tail})");
                    // Следующий архив — без перезагрузки библиотеки: она
                    // перечитывается один раз, когда очередь опустеет.
                    if !self.start_next_import() {
                        // Новые книги должны сразу находиться поиском, а их
                        // авторы — в подсказках: словарь зависит от индекса.
                        let _ = self.library.reload_metadata();
                        self.rebuild_author_dictionary();
                        let _ = self.refresh_search();
                        self.refresh_cache_usage();
                    }
                }
                WorkerMsg::Error(err) => {
                    self.import.running = false;
                    self.import.status = format!("ошибка импорта: {err}");
                    if let Some(name) = self.import.current.take() {
                        self.import.failed_files.push(name);
                    }
                    self.error = Some(err);
                    // Один битый архив не должен останавливать очередь.
                    if !self.start_next_import() {
                        let _ = self.library.reload_metadata();
                        self.rebuild_author_dictionary();
                        let _ = self.refresh_search();
                        self.refresh_cache_usage();
                    }
                }
                WorkerMsg::DedupProgress {
                    step,
                    done,
                    total,
                    status,
                } => {
                    self.dedup.running = true;
                    self.dedup.step = step.title().to_string();
                    self.dedup.done = done;
                    self.dedup.total = total;
                    self.dedup.status = status;
                }
                WorkerMsg::DedupDone(outcome) => {
                    self.dedup.running = false;
                    self.dedup.step = "готово".into();
                    self.dedup.done = 1;
                    self.dedup.total = 1;
                    let report = outcome.report();
                    self.dedup.status = report.lines().next().unwrap_or_default().to_string();
                    self.dedup.finished = Some(report.clone());
                    self.toast = Some(format!("дубликаты вынесены в {}", dedup::DUBLICATE_DIR));
                    // Библиотека перечитывается целиком: и метаданные, и индекс
                    // уже пересобраны фоновым потоком.
                    self.reload_library();
                }
                WorkerMsg::DedupFailed(err) => {
                    self.dedup.running = false;
                    self.dedup.step = "ошибка".into();
                    self.dedup.status = format!("вынос не удался: {err}");
                    self.dedup.finished = Some(format!(
                        "вынос не удался: {err}\n\n\
                         Данные не пострадали: книги, которые не успели переехать, \
                         остались в библиотеке. Перезапустите — вынос продолжит с места."
                    ));
                    self.error = Some(err);
                    self.reload_library();
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
///
/// `--cover` кодирует картинку как обложку — качество `LIBRARY_COVER_QUALITY`
/// и без пола по `min_side`. Это ровно та ветка, которой пользуется импорт,
/// поэтому найденная со стороны обложка ложится в `data/covers/` такими же
/// байтами, как если бы она пришла из EPUB.
fn run_image_mode(args: &[String]) -> i32 {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut out_dir: Option<PathBuf> = None;
    let mut is_cover = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" | "-o" => out_dir = it.next().map(PathBuf::from),
            "--cover" => is_cover = true,
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.is_empty() {
        eprintln!("использование: library_gui --image <картинка>… [--out каталог] [--cover]");
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

        let done = match images::optimize_single(&name, &data, &cfg, is_cover) {
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
            "  library_gui --import <zip>… [data_dir] [--delete]\n",
            "                                            импорт без окна; архивов\n",
            "                                            можно несколько — по очереди,\n",
            "                                            --delete удаляет исходники\n",
            "  library_gui --search <запрос> [data_dir] [--author X] [--genre Y]\n",
            "                                            поиск без окна\n",
            "  library_gui --convert <fb2> <epub>         конвертация FB2 → EPUB 3.3\n",
            "                                            без окна (отладка конвертера)\n",
            "  library_gui --image <файл>… [--out кат]   сжать картинки так же,\n",
            "                                            как при импорте (AVIF)\n",
            "  library_gui --bench <zip> [каталог]       бенчмарк всех шагов сжатия\n",
            "                                            (по умолчанию data/bench)\n",
            "  library_gui --dedup [data_dir] [--dry-run] [--jobs N]\n",
            "                                            вынести дубликаты по плану\n",
            "                                            dedup_out/plan.tsv в dublicate/\n",
            "                                            (без окна; --dry-run — только\n",
            "                                            показать, что уедет)\n",
            "\n",
            "Каталог данных:\n",
            "  archives/       архивы <от>-<до>.arx (один на входной ZIP)\n",
            "  covers/         обложки <от>-<до>/<id>.avif (папка на входной ZIP)\n",
            "  cache/          распакованные для чтения книги (чистится)\n",
            "  index/          индекс Tantivy для поиска\n",
            "  metadata/       метаданные книг, по файлу на входной ZIP\n",
            "  imports/        отчёты об импорте, по файлу на входной ZIP\n",
            "  failed/         FB2, которые не собрались (копия перед удалением ZIP)\n",
        ));
        std::process::exit(0);
    }

    // Вынос дубликатов без окна: `library_gui --dedup [data_dir] [--dry-run]`.
    // Обычный запуск исполняет план, `--dry-run` только печатает, что уедет.
    if args.first().map(String::as_str) == Some("--dedup") {
        let mut data_dir = PathBuf::from("data");
        let mut dry_run = false;
        let mut jobs = 0usize;
        let mut it = args[1..].iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--dry-run" => dry_run = true,
                "--jobs" => {
                    jobs = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                }
                other => data_dir = PathBuf::from(other),
            }
        }
        std::process::exit(run_headless_dedup(&data_dir, dry_run, jobs));
    }

    // Служебный режим без окна:
    // `library_gui --import <zip>… [data_dir] [--delete]`.
    // Нужен, чтобы проверять пайплайн без дисплея и гонять пакетные импорты.
    // Архивов можно указать сколько угодно — они импортируются по очереди.
    if args.first().map(String::as_str) == Some("--import") {
        let mut zips: Vec<PathBuf> = Vec::new();
        let mut data_dir = PathBuf::from("data");
        let mut delete = false;
        for arg in &args[1..] {
            if arg == "--delete" {
                delete = true;
                continue;
            }
            let path = PathBuf::from(arg);
            // Всё, что не похоже на архив, — каталог данных: так работает
            // и прежняя форма `--import <zip> data`.
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
            {
                zips.push(path);
            } else {
                data_dir = path;
            }
        }
        if zips.is_empty() {
            eprintln!("использование: library_gui --import <zip>… [data_dir] [--delete]");
            std::process::exit(2);
        }
        // Импортируем по очереди в одном процессе: так между архивами
        // остаётся тёплый кэш индекса, а отчёты не перемешиваются.
        let mut code = 0;
        for (i, zip) in zips.iter().enumerate() {
            if zips.len() > 1 {
                emit(format!(
                    "=== архив {}/{}: {} ===",
                    i + 1,
                    zips.len(),
                    zip.display()
                ));
            }
            let result = run_headless_import(zip, &data_dir, delete);
            if result != 0 {
                code = result;
            }
        }
        std::process::exit(code);
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
    // `library_gui --search <запрос> [data_dir] [--author X] [--genre Y]
    // [--show-short]`. Статьи и заметки спрятаны, как и в окне.
    if args.first().map(String::as_str) == Some("--search") {
        let mut query = String::new();
        let mut query_set = false;
        let mut data_dir = PathBuf::from("data");
        let mut data_dir_set = false;
        let mut author: Option<String> = None;
        let mut genre: Option<String> = None;
        let mut page = 1usize;
        let mut per_page = db::DEFAULT_SEARCH_LIMIT;
        let mut show_short = false;
        let mut cover_filter = db::CoverFilter::All;

        let mut it = args[1..].iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--author" => author = it.next().cloned(),
                "--genre" => genre = it.next().cloned(),
                // Показать и статьи с заметками: в окне это галочка.
                "--show-short" => show_short = true,
                // Галочки по обложке: в окне это две галочки.
                "--coverless" => cover_filter = db::CoverFilter::WithoutCover,
                "--with-cover" => cover_filter = db::CoverFilter::WithCover,
                // Листание выдачи из консоли — тем же кодом, что и в окне.
                "--page" => page = it.next().and_then(|v| v.parse().ok()).unwrap_or(1),
                "--per-page" => {
                    per_page = it
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(db::DEFAULT_SEARCH_LIMIT)
                        .clamp(1, db::MAX_PAGE_SIZE)
                }
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
            show_short,
            cover_filter,
            page.max(1),
            per_page,
        ));
    }

    // Пересборка поискового индекса: нужна после ручной правки метаданных,
    // иначе Tantivy продолжает искать по старым названиям и авторам.
    if args.first().map(String::as_str) == Some("--reindex") {
        let data_dir = args
            .get(1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data"));
        std::process::exit(run_reindex(data_dir));
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

/// Вынос дубликатов без окна: тот же код, что и за кнопкой.
///
/// Нужен для двух вещей: прогнать полную очистку библиотеки, не держа окно
/// открытым, и проверить всю цепочку на копии данных. `--dry-run` ничего не
/// трогает — только читает план и печатает, что уедет.
fn run_headless_dedup(data_dir: &Path, dry_run: bool, jobs: usize) -> i32 {
    let Some(path) = dedup::find_plan(data_dir) else {
        eprintln!(
            "не найден план: ожидается {}/{} (постройте: python3 dedup.py --phash)",
            dedup::REPORT_DIR,
            dedup::PLAN_NAME
        );
        return 2;
    };
    let plan = match dedup::Plan::load(&path) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("[фатально] {err:#}");
            return 2;
        }
    };
    let (books, archives, bytes) = plan.summary();
    emit(format!(
        "план {}: книг {books}, архивов {archives}, объём {}",
        path.display(),
        human_size(bytes)
    ));
    emit(format!("дубликаты переезжают в {}", dedup::dublicate_dir(data_dir).display()));
    if dry_run {
        emit("--dry-run: ничего не меняем");
        return 0;
    }
    let jobs = if jobs == 0 { dedup::default_jobs() } else { jobs };
    emit(format!("пересборка архивов в {jobs} потоков"));
    // Прогресс приходит из нескольких потоков сразу, поэтому «предыдущий шаг»
    // лежит под мьютексом: замыкание должно остаться `Fn + Sync`.
    let last_step = std::sync::Mutex::new(String::new());
    let report = |progress: dedup::Progress| {
        let step = progress.step.title().to_string();
        let mut previous = last_step.lock().unwrap_or_else(|err| err.into_inner());
        // В консоли печатаем не каждую книгу, а смену шага и каждые 10 %
        // внутри шага: иначе «архивы» выдают 230 строк подряд.
        let edge = *previous != step
            || progress.total == 0
            || progress.done == progress.total
            || progress.done % (progress.total / 10).max(1) == 0;
        if edge {
            emit(format!(
                "[{}] {}/{} {}",
                step, progress.done, progress.total, progress.status
            ));
        }
        *previous = step;
    };
    match dedup::execute(&plan, data_dir, jobs, &report) {
        Ok(outcome) => {
            emit(outcome.report());
            if outcome.errors.is_empty() { 0 } else { 1 }
        }
        Err(err) => {
            eprintln!("[фатально] {err:#}");
            2
        }
    }
}

fn run_headless_import(zip: &Path, data_dir: &Path, delete: bool) -> i32 {
    let (tx, rx) = unbounded();
    let settings = ImportSettings {
        delete_zip: delete,
        ..ImportSettings::default()
    };
    worker::spawn_import(zip.to_path_buf(), data_dir.to_path_buf(), settings, tx);

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
                deleted,
            } => {
                emit(format!(
                    "[готово] {books} книг за {seconds:.1} с, ошибок: {failed}, \
                     нерусских отброшено: {skipped}, \
                     битых ссылок в исходниках: {defects}{}",
                    if deleted { ", исходный ZIP удалён" } else { "" }
                ));
                return if failed == 0 { 0 } else { 1 };
            }
            WorkerMsg::Error(err) => {
                eprintln!("[фатально] {err}");
                return 2;
            }
            // Вынос дубликатов идёт только через окно, здесь таких сообщений нет.
            WorkerMsg::DedupProgress { .. }
            | WorkerMsg::DedupDone(_)
            | WorkerMsg::DedupFailed(_) => {}
        }
    }
    1
}

/// Поиск без окна: печатает результаты и проверяет извлечение первой книги.
/// Пересобрать поисковый индекс из метаданных.
///
/// Tantivy хранит собственные копии названий и авторов. Если метаданные
/// поправили снаружи (например, скриптом `data/tmp/fix_titles.py`), поиск
/// продолжит искать по старым значениям — пока индекс не собран заново.
fn run_reindex(data_dir: PathBuf) -> i32 {
    let started = std::time::Instant::now();
    let mut library = match Library::open(&data_dir) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("[фатально] не удалось открыть библиотеку: {err:#}");
            return 2;
        }
    };
    println!("книг в метаданных: {}", library.books.len());
    // `Library::open` уже замечает устаревшую схему, убирает старый индекс в
    // `index.bak-*` и пересобирает новый из метаданных. Второй проход сделал
    // бы ровно ту же работу ещё раз, поэтому его пропускаем.
    if library.schema_upgraded {
        println!(
            "схема индекса обновилась — индекс пересобран за {:.0} с",
            started.elapsed().as_secs_f64()
        );
        return 0;
    }
    match library.rebuild_index() {
        Ok(()) => {
            println!(
                "индекс пересобран за {:.0} с",
                started.elapsed().as_secs_f64()
            );
            0
        }
        Err(err) => {
            eprintln!("[фатально] не удалось пересобрать индекс: {err:#}");
            2
        }
    }
}

fn run_headless_search(
    query: &str,
    data_dir: PathBuf,
    author: Option<&str>,
    genre: Option<&str>,
    show_short: bool,
    cover_filter: db::CoverFilter,
    page: usize,
    per_page: usize,
) -> i32 {
    let library = match Library::open(&data_dir) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("[фатально] не удалось открыть библиотеку: {err:#}");
            return 2;
        }
    };
    emit(format!("книг в библиотеке: {}", library.books.len()));

    let offset = (page - 1).saturating_mul(per_page);
    let hits = match library.search(
        query,
        author,
        genre,
        show_short,
        cover_filter,
        offset,
        per_page,
    ) {
        Ok(h) => h,
        Err(err) => {
            eprintln!("[фатально] поиск не удался: {err:#}");
            return 2;
        }
    };
    let pages = hits.filtered.div_ceil(per_page).max(1);
    emit(format!(
        "по запросу `{query}` найдено: {} (страница {page} из {pages}, показано {}){}",
        hits.filtered,
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
    // Строгий поиск ничего не нашёл, и выдача собрана резервным запросом с
    // одной опечаткой: предупреждаем в консоли, как и в интерфейсе.
    if hits.approximate {
        emit("возможно, в запросе опечатка — показаны похожие");
    }
    emit(format!(
        "фасеты: авторов {}, жанров {}",
        hits.authors.len(),
        hits.genres.len()
    ));
    for book in hits.books.iter().take(20) {
        let d = &book.details;
        let year = if d.year.is_empty() { d.date.as_str() } else { d.year.as_str() };
        emit(format!(
            "  {} | {} | {} | {} | {} | год: {} | глав: {} | слов: {}",
            book.id,
            book.title,
            book.author,
            book.genres.join(", "),
            book.archive,
            if year.is_empty() { "—" } else { year },
            d.chapters.len(),
            // Пусто — счёт не проставлен (запись старого импорта).
            book.words.map_or_else(
                || "—".to_string(),
                |words| format!("{words} (≈{} стр.)", words / 250),
            )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Тестовая книга с минимальными полями.
    fn sample_book(id: u64, title: &str) -> BookMetadata {
        BookMetadata {
            id,
            title: title.to_string(),
            author: "Тестовый автор".to_string(),
            genre_codes: Vec::new(),
            genres: Vec::new(),
            archive: "1-1.arx".to_string(),
            internal_path: format!("{id}.epub"),
            cover: None,
            details: db::Details::default(),
            size: 500_000,
            // Обычная книга: 50 000 слов, прятать нечего.
            words: Some(50_000),
            hidden: false,
        }
    }

    /// Приложение листает выдачу по страницам и не выходит за её пределы.
    ///
    /// Это уровень, который видит интерфейс: `App::refresh_search` сам
    /// прижимает страницу к последней, когда фильтр сократил выдачу.
    #[test]
    fn app_pages_through_the_library() -> Result<()> {
        let dir = std::env::temp_dir().join("library_gui_test_app_paging");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let lib = Library::open(&dir)?;
            let books: Vec<BookMetadata> =
                (1..=25).map(|i| sample_book(i, &format!("Книга {i:02}"))).collect();
            lib.append_metadata("1-25", &books)?;
        }

        let mut app = App::new(dir.clone())?;
        // `App::new` уже открыл первую страницу с размером по умолчанию;
        // пересчитываем её уже с нужным размером.
        app.page_size = 10;
        app.refresh_search()?;
        assert_eq!(app.matched, 25, "все 25 книг в выдаче");
        assert_eq!(app.pages(), 3, "три страницы по десять");
        assert_eq!(app.page, 0);
        assert_eq!(app.results.len(), 10);

        let mut seen: Vec<u64> = app.results.iter().map(|b| b.id).collect();
        for page in 1..3 {
            app.page = page;
            app.refresh_search()?;
            let expect = if page == 2 { 5 } else { 10 };
            assert_eq!(app.results.len(), expect, "страница {page}");
            seen.extend(app.results.iter().map(|b| b.id));
        }
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 25, "страницы покрывают все книги без повторов");

        // Номер страницы за пределами выдачи — прижимается к последней,
        // а не показывает пустоту.
        app.page = 99;
        app.refresh_search()?;
        assert_eq!(app.page, 2, "страница прижата к последней");
        assert_eq!(app.results.len(), 5);

        // Фильтр, под который ничего не подходит, обнуляет выдачу и страницу.
        app.page = 2;
        app.filter_author = Some("Никого".to_string());
        app.refresh_search()?;
        assert_eq!(app.filtered, 0, "фильтр не пропускает ни одной книги");
        assert_eq!(app.matched, 25, "по тексту подходят все 25");
        assert_eq!(app.page, 0, "страница сбрасывается в первую");
        assert!(app.results.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Подсказки автора не зависят от текущей выдачи.
    ///
    /// Словарь собирается из всего индекса при старте `App`, а фасеты
    /// (`app.authors`) — только по текущей выдаче. Здесь поиск оставляет в
    /// выдаче одного автора, но второй всё равно находится в подсказках.
    #[test]
    fn app_suggests_authors_outside_the_current_hits() -> Result<()> {
        let dir = std::env::temp_dir().join("library_gui_test_app_author_dict");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let lib = Library::open(&dir)?;
            let books = vec![
                BookMetadata {
                    author: "Михаил Булгаков".to_string(),
                    ..sample_book(1, "Мастер и Маргарита")
                },
                BookMetadata {
                    author: "Фёдор Достоевский".to_string(),
                    ..sample_book(2, "Преступление и наказание")
                },
            ];
            lib.append_metadata("1-2", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }

        let mut app = App::new(dir.clone())?;
        assert_eq!(app.author_dictionary.len(), 2, "словарь собран из индекса");

        // Запрос оставляет в выдаче только Булгакова — фасеты сужаются до него.
        app.query = "Мастер".to_string();
        app.refresh_search()?;
        assert_eq!(app.matched, 1);
        assert_eq!(
            app.authors,
            vec![("Михаил Булгаков".to_string(), 1)],
            "фасеты считаются по текущей выдаче"
        );

        // Подсказки — по всему словарю, а не по фасетам: Достоевский
        // находится, хотя в выдаче его книг нет.
        let found = app
            .author_dictionary
            .suggestions("дост", crate::author_suggest::MAX_SUGGESTIONS);
        assert_eq!(
            found,
            vec![("Фёдор Достоевский".to_string(), 1)],
            "автор вне текущей выдачи всё равно подсказывается"
        );

        // И интерфейс рисует подсказку именно из словаря: в фасетах её нет,
        // поэтому текст кадра может появиться только из `author_dictionary`.
        app.author_query = "дост".to_string();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut rendered = ctx.run_ui(input.clone(), |ui| {
                ui::draw(&mut app, ui);
            });
            rendered.textures_delta.clear();
            texts.clear();
            for clipped in &rendered.shapes {
                collect_texts(&clipped.shape, &mut texts);
            }
        }
        let all = texts.join(" | ");
        assert!(
            all.contains("Фёдор Достоевский"),
            "подсказка автора не появилась в интерфейсе: {all}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Собрать весь текст, который интерфейс отрисовал в кадре.
    fn collect_texts(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => out.push(t.galley.text().to_string()),
            egui::Shape::Vec(items) => {
                for item in items {
                    collect_texts(item, out);
                }
            }
            _ => {}
        }
    }

    /// Отрисовка кадра показывает счётчик и перелистывание страниц.
    ///
    /// Это проверка того, что видит пользователь: интерфейс рисуется
    /// без окна, а в кадре действительно есть текст «страница N из M» и
    /// кнопки перелистывания. Раньше на этом месте стояла только обрезка
    /// по 500 книг, и дальше выдачи было не видно.
    #[test]
    fn ui_renders_paging_controls() -> Result<()> {
        let dir = std::env::temp_dir().join("library_gui_test_ui_paging");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let lib = Library::open(&dir)?;
            let books: Vec<BookMetadata> =
                (1..=25).map(|i| sample_book(i, &format!("Книга {i:02}"))).collect();
            lib.append_metadata("1-25", &books)?;
        }

        let mut app = App::new(dir.clone())?;
        app.page_size = 10;
        app.refresh_search()?;

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        // Первый кадр панель получает высоту по содержимому, поэтому текст
        // рисуется уже во втором. Дальше оба кадра делают одно и то же.
        let frame = |app: &mut App| {
            let mut out = ctx.run_ui(input.clone(), |ui| {
                ui::draw(app, ui);
            });
            out.textures_delta.clear();
            out
        };
        frame(&mut app);
        let output = frame(&mut app);

        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        let all = texts.join(" | ");
        assert!(
            all.contains("Найдено: 25"),
            "в кадре нет счётчика найденных книг: {all}"
        );
        assert!(
            all.contains("страница 1 из 3"),
            "в кадре нет номера страницы: {all}"
        );
        assert!(
            all.contains("вперёд") && all.contains("назад"),
            "в кадре нет кнопок перелистывания: {all}"
        );
        assert!(
            all.contains("на странице"),
            "в кадре нет выбора размера страницы: {all}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Очередь ZIP: архивы запускаются по одному, и следующий стартует
    /// только после того, как предыдущий закончился — успешно или с ошибкой.
    #[test]
    fn import_queue_runs_archives_one_by_one() -> Result<()> {
        let dir = std::env::temp_dir().join("library_gui_test_import_queue");
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = App::new(dir.clone())?;

        // Оба архива не существуют: воркер сразу падает, но очередь обязана
        // доехать до конца, а не остановиться на первом.
        app.start_imports(vec![
            dir.join("нет-такого-1.zip"),
            dir.join("нет-такого-2.zip"),
        ]);
        assert!(app.import.running, "первый архив должен был стартовать");
        assert_eq!(app.import.files, 2, "в пакете два архива");
        assert_eq!(app.import.index, 1, "сейчас первый архив");
        assert_eq!(app.import.queue, vec!["нет-такого-2.zip".to_string()]);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !(!app.import.running && app.import.index == 2) {
            assert!(
                std::time::Instant::now() < deadline,
                "очередь не доехала до второго архива за 10 с"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            app.pump_worker();
        }
        assert_eq!(
            app.import.failed_files,
            vec![
                "нет-такого-1.zip".to_string(),
                "нет-такого-2.zip".to_string()
            ],
            "оба архива должны быть отмечены как несостоявшиеся"
        );
        assert!(app.import.queue.is_empty(), "очередь должна опустеть");

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Кнопка выноса дубликатов стоит в верхней строке главного окна.
    #[test]
    fn ui_renders_dedup_button() -> Result<()> {
        let dir = std::env::temp_dir().join("library_gui_test_ui_dedup_button");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let mut app = App::new(dir.clone())?;

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut rendered = ctx.run_ui(input.clone(), |ui| {
                ui::draw(&mut app, ui);
            });
            rendered.textures_delta.clear();
            texts.clear();
            for clipped in &rendered.shapes {
                collect_texts(&clipped.shape, &mut texts);
            }
        }
        let all = texts.join(" | ");
        assert!(all.contains("Убрать дубликаты"), "нет кнопки выноса: {all}");

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Кнопка находит план и окно показывает, что именно уедет.
    #[test]
    fn ui_renders_dedup_dialog() -> Result<()> {
        let dir = std::env::temp_dir().join("library_gui_test_ui_dedup");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data").join("metadata"))?;
        let mut app = App::new(dir.join("data"))?;
        // План лежит там, где его ищет кнопка: ../dedup_out/plan.tsv.
        let out = dir.join("dedup_out");
        std::fs::create_dir_all(&out)?;
        std::fs::write(
            out.join("plan.tsv"),
            "id\tarchive\tinternal_path\tcover\tgroup_id\ttitle\tauthor\tyear\tsize\tkeeper_id\treason\n\
             7\t1-9.arx\t7.epub\t1-9/7.avif\tg000001\tКнига\tАвтор\t2001\t123\t9\tisbn\n",
        )?;
        app.open_dedup_dialog();
        assert!(app.dedup.dialog.is_some(), "план не прочитался");
        assert_eq!(app.dedup.dialog.as_ref().unwrap().books, 1);

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut rendered = ctx.run_ui(input.clone(), |ui| {
                ui::draw(&mut app, ui);
            });
            rendered.textures_delta.clear();
            texts.clear();
            for clipped in &rendered.shapes {
                collect_texts(&clipped.shape, &mut texts);
            }
        }
        let all = texts.join(" | ");
        assert!(all.contains("Убрать дубликаты"), "нет окна выноса: {all}");
        assert!(all.contains("Переезжает в"), "не сказано, куда уедет: {all}");
        assert!(all.contains("dublicate"), "нет каталога назначения: {all}");
        assert!(all.contains("Вынести дубликаты"), "нет кнопки запуска: {all}");

        // Без плана кнопка объясняет, что делать, а не падает.
        std::fs::remove_file(out.join("plan.tsv"))?;
        app.dedup.dialog = None;
        app.error = None;
        app.open_dedup_dialog();
        assert!(app.dedup.dialog.is_none());
        let message = app.error.clone().unwrap_or_default();
        assert!(message.contains("dedup.py"), "нет подсказки: {message}");

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Панель импорта показывает место архива в очереди и саму очередь.
    #[test]
    fn ui_renders_import_queue() -> Result<()> {
        let dir = std::env::temp_dir().join("library_gui_test_ui_queue");
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = App::new(dir.clone())?;

        app.import.running = true;
        app.import.files = 2;
        app.import.index = 1;
        app.import.total = 10;
        app.import.done = 4;
        app.import.status = "книга — автор".to_string();
        app.import.queue = vec!["второй.zip".to_string()];

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let frame = |app: &mut App| {
            let mut out = ctx.run_ui(input.clone(), |ui| {
                ui::draw(app, ui);
            });
            out.textures_delta.clear();
            out
        };
        // Первый кадр панель получает высоту по содержимому — как и выше.
        frame(&mut app);
        let output = frame(&mut app);

        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        let all = texts.join(" | ");
        assert!(
            all.contains("файл 1 из 2"),
            "в кадре нет места архива в очереди: {all}"
        );
        assert!(
            all.contains("в очереди (1): второй.zip"),
            "в кадре нет очереди архивов: {all}"
        );

        // Исходник, который не удалось удалить, тоже должен быть виден.
        app.import.queue.clear();
        app.import.running = false;
        app.import.kept = vec!["первый.zip".to_string()];
        frame(&mut app);
        let output = frame(&mut app);
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        let all = texts.join(" | ");
        assert!(
            all.contains("не удалось удалить") && all.contains("первый.zip"),
            "в кадре нет сведений о неудалённом исходнике: {all}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Окно выбора архивов находит ZIP, помечает уже собранные диапазоны и
    /// отдаёт на импорт только отмеченные файлы.
    #[test]
    fn zip_picker_scans_and_checks() -> Result<()> {
        let dir = std::env::temp_dir().join("library_gui_test_zip_picker");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("fb2-1-9.zip"), b"x")?;
        std::fs::write(dir.join("fb2-10-19.zip"), b"xx")?;
        std::fs::write(dir.join("не-архив.txt"), b"x")?;
        let archives = dir.join("archives");
        std::fs::create_dir_all(&archives)?;
        std::fs::write(archives.join("1-9.arx"), b"x")?;

        let mut picker = ZipPicker::open(dir.clone(), &archives);
        assert_eq!(picker.entries.len(), 2, "в списке только ZIP-архивы");
        assert!(
            picker
                .entries
                .iter()
                .any(|e| e.name == "fb2-1-9.zip" && e.imported),
            "архив с собранным диапазоном должен быть помечен"
        );
        assert!(
            picker
                .entries
                .iter()
                .any(|e| e.name == "fb2-10-19.zip" && !e.imported),
            "новый диапазон помечать нечем"
        );

        // «Только не импортированные» отмечает ровно новый архив.
        picker.check_only_new();
        assert_eq!(picker.checked_stats().0, 1);
        assert_eq!(picker.checked_paths(), vec![dir.join("fb2-10-19.zip")]);

        // Фильтр сужает список, а «снять/отметить все» работает по видимым.
        picker.set_visible(false);
        picker.filter = "1-9".to_string();
        assert_eq!(picker.visible_indices().len(), 1, "фильтр по подстроке");
        picker.set_visible(true);
        assert_eq!(picker.checked_paths(), vec![dir.join("fb2-1-9.zip")]);

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Окно выбора архивов действительно рисуется и показывает файлы.
    #[test]
    fn ui_renders_zip_picker() -> Result<()> {
        let dir = std::env::temp_dir().join("library_gui_test_ui_picker");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let mut app = App::new(dir.clone())?;
        std::fs::write(dir.join("fb2-1-9.zip"), b"x")?;
        app.zip_dir = Some(dir.clone());
        app.open_zip_picker();

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let frame = |app: &mut App| {
            let mut out = ctx.run_ui(input.clone(), |ui| {
                ui::draw(app, ui);
            });
            out.textures_delta.clear();
            out
        };
        frame(&mut app);
        let output = frame(&mut app);

        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        let all = texts.join(" | ");
        assert!(all.contains("Выбор ZIP-архивов"), "нет окна выбора: {all}");
        assert!(all.contains("fb2-1-9.zip"), "нет списка архивов: {all}");
        assert!(
            all.contains("Импортировать выбранные"),
            "нет кнопки импорта выбранных: {all}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
