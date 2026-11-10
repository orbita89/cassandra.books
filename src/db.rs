//! Доступ к библиотеке: поиск через Tantivy и извлечение книг из ARX.
//!
//! Метаданные живут в двух местах:
//!   * `data/index/`      — индекс Tantivy (только для поиска);
//!   * `data/metadata/<диапазон>.jsonl` — полные метаданные, по файлу на входной ZIP;
//!     читаются в память целиком при старте.
//!
//! Tantivy используется исключительно как поисковый движок: по запросу он
//! возвращает `id`, а сам `BookMetadata` берётся из памяти по этому `id`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, INDEXED, STORED, STRING, TEXT, Value};
use tantivy::{
    doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument,
};

/// Бюджет памяти Tantivy при индексации — не больше 50 МБ (требование ТЗ).
pub const INDEX_MEMORY_BUDGET: usize = 50 * 1024 * 1024;

/// Сколько результатов отдавать на один поисковый запрос по умолчанию.
///
/// Это размер **страницы**: выдача листается, а не обрезается на первом
/// пределе. Раньше здесь стояло 500, и это был весь доступный корпус — дальше
/// книг просто не было видно.
pub const DEFAULT_SEARCH_LIMIT: usize = 100;

/// Сколько книг можно запросить одной страницей.
pub const MAX_PAGE_SIZE: usize = 2000;

/// Размеры страницы, которые предлагает интерфейс.
pub const PAGE_SIZES: [usize; 6] = [50, 100, 200, 500, 1000, MAX_PAGE_SIZE];

/// Сколько книг Tantivy отдаёт до применения фильтров по автору и жанру.
/// Ограничение нужно, чтобы запрос не тянул всю библиотеку в память.
pub const SEARCH_FETCH_CAP: usize = 20_000;

/// Сколько пунктов показывать в выпадающих списках авторов и жанров.
/// В библиотеке на 500 000 книг авторов могут быть десятки тысяч, и рисовать
/// их все в списке нельзя. Редкого автора проще найти через строку поиска.
pub const FACET_LIMIT: usize = 500;

/// Одна книга в библиотеке.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookMetadata {
    /// Числовой id (обычно номер книги в lib.rus.ec).
    pub id: u64,
    /// Название книги.
    pub title: String,
    /// Автор (одной строкой).
    pub author: String,
    /// Коды жанров как в FB2 (`sci_history`, `popadancy`).
    ///
    /// В файлах метаданных хранятся именно коды, а не названия: тогда
    /// уточнение таблицы перевода не требует переимпорта библиотеки.
    #[serde(default, alias = "genres")]
    pub genre_codes: Vec<String>,
    /// Названия жанров; первый — основной. Считаются из `genre_codes`
    /// при загрузке и в память не сериализуются.
    #[serde(skip)]
    pub genres: Vec<String>,
    /// Имя `.arx` внутри `data/archives`.
    pub archive: String,
    /// Имя записи внутри `.arx` (обычно `{id}.epub`).
    pub internal_path: String,
    /// Файл обложки внутри `data/covers`, если её удалось достать.
    pub cover: Option<String>,
    /// Подробные метаданные: аннотация, год, серия, издательство, главы.
    #[serde(default)]
    pub details: Details,
    /// Размер EPUB в байтах.
    pub size: u64,
}

/// Подробные метаданные книги — всё, что удалось достать из FB2 и EPUB.
///
/// Отдельная структура, чтобы не раздувать `BookMetadata`: она целиком
/// уезжает в `data/metadata/` и читается обратно при загрузке библиотеки.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Details {
    /// Все авторы из `<title-info>`, включая основного.
    #[serde(default)]
    pub authors: Vec<String>,
    /// Аннотация: теги убраны, абзацы через перевод строки.
    #[serde(default)]
    pub annotation: String,
    #[serde(default)]
    pub keywords: String,
    /// Язык книги и язык оригинала.
    #[serde(default)]
    pub lang: String,
    #[serde(default)]
    pub src_lang: String,
    /// Переводчики.
    #[serde(default)]
    pub translators: Vec<String>,
    /// Серия и номер книги в ней.
    #[serde(default)]
    pub series: String,
    #[serde(default)]
    pub series_number: String,
    /// Год написания или издания.
    #[serde(default)]
    pub date: String,
    /// Выходные данные.
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub year: String,
    #[serde(default)]
    pub isbn: String,
    /// `document-info` исходного FB2.
    #[serde(default)]
    pub doc_id: String,
    #[serde(default)]
    pub doc_version: String,
    #[serde(default)]
    pub doc_date: String,
    #[serde(default)]
    pub program_used: String,
    #[serde(default)]
    pub src_url: String,
    #[serde(default)]
    pub src_ocr: String,
    /// Оглавление из EPUB — то же, что увидит читалка.
    #[serde(default)]
    pub chapters: Vec<String>,
}

/// Результат поиска вместе с фасетами для фильтров.
#[derive(Debug, Default)]
pub struct SearchOutcome {
    /// Книги запрошенной страницы после фильтров, по алфавиту.
    pub books: Vec<BookMetadata>,
    /// Автор → сколько книг подошло под текстовый запрос.
    pub authors: Vec<(String, usize)>,
    /// Жанр → сколько книг подошло под текстовый запрос.
    pub genres: Vec<(String, usize)>,
    /// Сколько книг подошло под текстовый запрос (фильтры не учтены).
    pub matched: usize,
    /// Сколько книг прошло фильтры — по этому числу и листается выдача.
    pub filtered: usize,
}

/// Книга в куче отбора страницы.
///
/// `Ord` смотрит **только на ключ** (название в нижнем регистре): куча нужна
/// ровно затем, чтобы оставить `offset + limit` самых левых по алфавиту книг,
/// а порядок внутри неё значения не имеет.
struct PageEntry {
    key: String,
    book: BookMetadata,
}

impl Ord for PageEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

impl PartialOrd for PageEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for PageEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for PageEntry {}

/// Промежуточный накопитель: фасеты и книги страницы за один проход.
struct Facets {
    authors: HashMap<String, usize>,
    genres: HashMap<String, usize>,
    /// Сколько книг нужно вместе с пропущенными: `offset + limit`.
    ///
    /// Весь корпус в векторе держать нельзя — в библиотеке бывают сотни тысяч
    /// книг, — поэтому в куче остаются только `keep` самых левых по алфавиту,
    /// и память не зависит от размера библиотеки.
    keep: usize,
    books: BinaryHeap<PageEntry>,
    matched: usize,
    filtered: usize,
}

impl Facets {
    fn new(keep: usize) -> Self {
        Self {
            authors: HashMap::new(),
            genres: HashMap::new(),
            keep,
            books: BinaryHeap::new(),
            matched: 0,
            filtered: 0,
        }
    }

    /// Учесть одну книгу: обновить фасеты и, если она проходит фильтры,
    /// положить её в отбор страницы.
    fn add(&mut self, book: &BookMetadata, author: Option<&str>, genre: Option<&str>) {
        self.matched += 1;
        if !book.author.trim().is_empty() {
            *self.authors.entry(book.author.clone()).or_default() += 1;
        }
        for g in &book.genres {
            *self.genres.entry(g.clone()).or_default() += 1;
        }
        let by_author = author.is_none_or(|a| book.author == a);
        let by_genre = genre.is_none_or(|g| book.genres.iter().any(|x| x == g));
        if !(by_author && by_genre) {
            return;
        }
        self.filtered += 1;

        let key = book.title.to_lowercase();
        // Книга, которая в алфавите правее самой правой из уже набранных, на
        // страницу не попадёт — и копировать её незачем.
        if self.books.len() >= self.keep {
            if let Some(worst) = self.books.peek() {
                if key >= worst.key {
                    return;
                }
            }
        }
        self.books.push(PageEntry {
            key,
            book: book.clone(),
        });
        if self.books.len() > self.keep {
            self.books.pop();
        }
    }
}

/// Отсортировать фасет: сначала по числу книг, затем по алфавиту.
fn sort_counts(map: HashMap<String, usize>) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(FACET_LIMIT);
    v
}

/// Открытая библиотека.
pub struct Library {
    /// Каталог `data/`.
    pub data_dir: PathBuf,
    /// Индекс Tantivy.
    pub index: Index,
    /// Читатель индекса (нужен для поиска).
    pub reader: IndexReader,
    f_id: Field,
    f_title: Field,
    f_author: Field,
    f_genres: Field,
    f_archive: Field,
    f_path: Field,
    /// `id` → метаданные, загружено из `data/metadata/`.
    pub books: HashMap<u64, BookMetadata>,
}

impl Library {
    /// Открыть библиотеку в `data_dir`, создав индекс и файлы при необходимости.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        for sub in ["archives", "covers", "index", "tmp", "cache", "metadata"] {
            fs::create_dir_all(data_dir.join(sub))
                .with_context(|| format!("не удалось создать {}/{sub}", data_dir.display()))?;
        }

        let mut builder = Schema::builder();
        let f_id = builder.add_u64_field("id", INDEXED | STORED);
        let f_title = builder.add_text_field("title", TEXT | STORED);
        let f_author = builder.add_text_field("author", TEXT | STORED);
        let f_genres = builder.add_text_field("genres", TEXT | STORED);
        let f_archive = builder.add_text_field("archive", STRING | STORED);
        let f_path = builder.add_text_field("internal_path", STRING | STORED);
        let schema = builder.build();

        let index_dir = data_dir.join("index");
        // `meta.json` появляется только после первого коммита, поэтому
        // «пустой каталог» и «готовый индекс» надо различать явно.
        let index = if index_dir.join("meta.json").exists() {
            Index::open_in_dir(&index_dir)
                .with_context(|| format!("не удалось открыть индекс {}", index_dir.display()))?
        } else {
            Index::create_in_dir(&index_dir, schema.clone()).with_context(|| {
                format!("не удалось создать индекс {}", index_dir.display())
            })?
        };

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("не удалось создать reader Tantivy")?;

        let mut library = Self {
            data_dir,
            index,
            reader,
            f_id,
            f_title,
            f_author,
            f_genres,
            f_archive,
            f_path,
            books: HashMap::new(),
        };
        library.reload_metadata()?;
        Ok(library)
    }

    /// Каталог с метаданными: `data/metadata/`.
    ///
    /// Раньше всё лежало в одном `data/metadata.jsonl`. Один файл на
    /// библиотеку в миллион книг — это 4 ГБ и одна точка отказа. Поэтому
    /// записей несколько: по файлу на каждый входной ZIP, `<диапазон>.jsonl`.
    /// Формат тот же (по строке JSON на книгу), но обрыв записи стоит одной
    /// партии, а не всей библиотеки, и файлов ровно столько, сколько было
    /// импортов, — а не по одному на книгу.
    pub fn metadata_dir(&self) -> PathBuf {
        self.data_dir.join("metadata")
    }

    /// Старый единый файл: читается, если каталога ещё нет.
    pub fn legacy_metadata_path(&self) -> PathBuf {
        self.data_dir.join("metadata.jsonl")
    }

    /// Путь к `data/archives`.
    pub fn archives_dir(&self) -> PathBuf {
        self.data_dir.join("archives")
    }

    /// Путь к `data/covers`.
    pub fn covers_dir(&self) -> PathBuf {
        self.data_dir.join("covers")
    }

    /// Относительный путь обложки внутри `data/covers`.
    ///
    /// Один входной ZIP — одна папка (`821884-821898/821884.avif`), а не
    /// отдельный каталог на книгу: на библиотеку в миллион книг плоский
    /// каталог — это миллион файлов в одном `readdir`, а папка на книгу —
    /// миллион каталогов. И то и другое плохо для файловой системы, `ls`
    /// и бэкапа. Папок ровно столько, сколько было импортов.
    ///
    /// Старые библиотеки, где обложки лежали в `id % 1000`, продолжают
    /// работать: путь хранится в метаданных книги, а не вычисляется заново.
    pub fn cover_rel_path(batch: &str, id: u64, ext: &str) -> String {
        format!("{batch}/{id}.{ext}")
    }

    /// Путь к `data/tmp`.
    pub fn tmp_dir(&self) -> PathBuf {
        self.data_dir.join("tmp")
    }

    /// Путь к `data/cache` — сюда распаковываются книги для чтения.
    ///
    /// Раньше они шли в системный `/tmp`, но оттуда их неудобно чистить
    /// и они теряются при перезагрузке. Кэш лежит рядом с библиотекой,
    /// его размер видно в интерфейсе, и его можно очистить одной кнопкой.
    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir.join("cache")
    }

    /// Сколько файлов в кэше чтения и сколько они занимают.
    pub fn cache_usage(&self) -> (usize, u64) {
        let dir = self.cache_dir();
        let Ok(entries) = fs::read_dir(&dir) else {
            return (0, 0);
        };
        let mut count = 0usize;
        let mut bytes = 0u64;
        for e in entries.flatten() {
            if let Ok(meta) = e.metadata() {
                if meta.is_file() {
                    count += 1;
                    bytes += meta.len();
                }
            }
        }
        (count, bytes)
    }

    /// Полностью очистить кэш чтения. Возвращает, сколько файлов удалили.
    pub fn clear_cache(&self) -> Result<usize> {
        let dir = self.cache_dir();
        let mut removed = 0usize;
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Ok(0),
        };
        for e in entries.flatten() {
            if e.metadata().map(|m| m.is_file()).unwrap_or(false)
                && fs::remove_file(e.path()).is_ok()
            {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Урезать кэш до `max_bytes`, удаляя самые старые файлы.
    pub fn prune_cache(&self, max_bytes: u64) -> Result<usize> {
        let dir = self.cache_dir();
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(0);
        };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            total += meta.len();
            let at = meta
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            files.push((at, meta.len(), e.path()));
        }
        if total <= max_bytes {
            return Ok(0);
        }
        files.sort_by_key(|(at, _, _)| *at);
        let mut removed = 0usize;
        for (_, size, path) in files {
            if total <= max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Перечитать метаданные в память: все файлы `data/metadata/*.jsonl`
    /// плюс старый единый файл, если он остался с прошлых версий.
    pub fn reload_metadata(&mut self) -> Result<()> {
        self.books.clear();
        let dir = self.metadata_dir();
        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .map(|d| {
                d.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        // Старый единый файл читаем последним: если он остался с прошлых
        // версий, книги из него всё равно попадут в библиотеку.
        let legacy = self.legacy_metadata_path();
        if legacy.exists() {
            files.push(legacy);
        }
        for path in files {
            self.load_metadata_file(&path)?;
        }
        Ok(())
    }

    /// Прочитать один файл метаданных. Битые строки пропускаются поштучно:
    /// из-за одной испорченной книги терять весь файл незачем.
    fn load_metadata_file(&mut self, path: &Path) -> Result<()> {
        let file = fs::File::open(path)
            .with_context(|| format!("не удалось открыть {}", path.display()))?;
        let mut bad = 0usize;
        let mut total = 0usize;
        for (n, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            total += 1;
            match serde_json::from_str::<BookMetadata>(&line) {
                Ok(mut book) => {
                    // Названия жанров считаем здесь, а не при импорте.
                    book.genres = crate::genres::names(&book.genre_codes);
                    self.books.insert(book.id, book);
                }
                Err(err) => {
                    bad += 1;
                    eprintln!("[{}] строка {}: {err}", path.display(), n + 1);
                }
            }
        }
        if bad > 0 {
            eprintln!(
                "[!] {}: прочитано {total} книг, {bad} строк битые и пропущены",
                path.display()
            );
        }
        Ok(())
    }

    /// Открыть writer для добавления книг в индекс (не больше 50 МБ памяти).
    pub fn writer(&self) -> Result<IndexWriter<TantivyDocument>> {
        self.index
            .writer::<TantivyDocument>(INDEX_MEMORY_BUDGET)
            .context("не удалось открыть writer Tantivy")
    }

    /// Добавить книги в индекс и закоммитить.
    pub fn index_books(
        &self,
        writer: &mut IndexWriter<TantivyDocument>,
        books: &[BookMetadata],
    ) -> Result<()> {
        for book in books {
            // Переиндексация той же книги не должна плодить дубликаты.
            writer.delete_term(tantivy::Term::from_field_u64(self.f_id, book.id));
            writer
                .add_document(doc!(
                    self.f_id => book.id,
                    self.f_title => book.title.clone(),
                    self.f_author => book.author.clone(),
                    // Жанры кладём одной строкой: искать по ним всё равно
                    // будет `QueryParser`, а фильтр работает по метаданным.
                    self.f_genres => book.genres.join(" "),
                    self.f_archive => book.archive.clone(),
                    self.f_path => book.internal_path.clone(),
                ))
                .with_context(|| format!("не удалось добавить в индекс книгу {}", book.id))?;
        }
        writer.commit().context("не удалось закоммитить индекс")?;
        self.reader.reload().context("не удалось перечитать индекс")?;
        Ok(())
    }

    /// Дописать книги в файл своей партии: `data/metadata/<диапазон>.jsonl`.
    ///
    /// Один входной ZIP — один файл, сколько бы книг в нём ни было. Запись
    /// идёт с `flush` и `sync_all`: если процесс убьют посреди импорта,
    /// потеряется максимум одна незавершённая строка, а не вся партия.
    /// Дописывание в конец означает, что уже записанные книги не пострадают
    /// ни при каком сбое.
    pub fn append_metadata(&self, batch: &str, books: &[BookMetadata]) -> Result<()> {
        let dir = self.metadata_dir();
        let path = dir.join(format!("{batch}.jsonl"));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("не удалось открыть {}", path.display()))?;
        for book in books {
            writeln!(file, "{}", serde_json::to_string(book)?)?;
        }
        file.flush()?;
        file.sync_all()
            .with_context(|| format!("не удалось сбросить {}", path.display()))?;
        Ok(())
    }

    /// Поиск с фильтрами по автору и жанру.
    ///
    /// Пустой текстовый запрос означает «вся библиотека» — тогда перебираются
    /// все книги в памяти, поэтому фильтр по жанру работает по всей библиотеке,
    /// а не только по первой странице выдачи.
    ///
    /// Фасеты (списки авторов и жанров) считаются по книгам, подошедшим под
    /// **текстовый запрос**, без учёта самих фильтров, — иначе после выбора
    /// автора в выпадающем списке остался бы только он один.
    pub fn search(
        &self,
        query: &str,
        author: Option<&str>,
        genre: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SearchOutcome> {
        let query = query.trim();

        // Непустой запрос идёт через Tantivy (там порядок по релевантности),
        // пустой — просто по всей библиотеке.
        let hits = if query.is_empty() {
            None
        } else {
            Some(self.full_text_hits(query, SEARCH_FETCH_CAP)?)
        };

        let mut acc = Facets::new(offset.saturating_add(limit));
        match &hits {
            Some(books) => {
                for book in books {
                    acc.add(book, author, genre);
                }
            }
            None => {
                for book in self.books.values() {
                    acc.add(book, author, genre);
                }
            }
        }

        // `into_sorted_vec` отдаёт кучу по возрастанию ключа — это и есть
        // алфавитный порядок; страница вырезается уже из него.
        let books = acc
            .books
            .into_sorted_vec()
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|e| e.book)
            .collect();

        Ok(SearchOutcome {
            books,
            authors: sort_counts(acc.authors),
            genres: sort_counts(acc.genres),
            matched: acc.matched,
            filtered: acc.filtered,
        })
    }

    /// Полнотекстовый поиск по названию, автору и жанрам.
    fn full_text_hits(&self, query: &str, cap: usize) -> Result<Vec<BookMetadata>> {
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(
            &self.index,
            vec![self.f_title, self.f_author, self.f_genres],
        );
        let parsed = parser
            .parse_query(query)
            .with_context(|| format!("не удалось разобрать запрос `{query}`"))?;
        let hits = searcher
            .search(&parsed, &TopDocs::with_limit(cap).order_by_score())
            .context("ошибка поиска")?;

        let mut out = Vec::with_capacity(hits.len());
        for (_score, addr) in hits {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let Some(id) = doc.get_first(self.f_id).and_then(|v| v.as_u64()) else {
                continue;
            };
            if let Some(book) = self.books.get(&id) {
                out.push(book.clone());
            } else {
                // Индекс и метаданные разошлись — собираем из индекса,
                // чтобы книга всё равно нашлась.
                let text = |f: Field| {
                    doc.get_first(f)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                out.push(BookMetadata {
                    id,
                    title: text(self.f_title),
                    author: text(self.f_author),
                    genre_codes: Vec::new(),
                    genres: Vec::new(),
                    archive: text(self.f_archive),
                    internal_path: text(self.f_path),
                    cover: None,
                    details: Details::default(),
                    size: 0,
                });
            }
        }
        Ok(out)
    }

    /// Путь к обложке книги, если файл существует.
    pub fn cover_path(&self, book: &BookMetadata) -> Option<PathBuf> {
        let name = book.cover.as_ref()?;
        let path = self.covers_dir().join(name);
        path.exists().then_some(path)
    }

    /// Извлечь книгу из `.arx` в файл `dest`.
    ///
    /// `arx dump` пишет запись в stdout, поэтому просто перенаправляем поток.
    pub fn extract_to(&self, book: &BookMetadata, dest: &Path) -> Result<()> {
        let archive = self.archives_dir().join(&book.archive);
        if !archive.exists() {
            bail!("архив не найден: {}", archive.display());
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let output = Command::new("arx")
            .arg("dump")
            .arg(&archive)
            .arg(&book.internal_path)
            .output()
            .with_context(|| {
                format!(
                    "не удалось запустить `arx dump {} {}`",
                    archive.display(),
                    book.internal_path
                )
            })?;
        if !output.status.success() {
            bail!(
                "arx dump завершился с ошибкой ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if output.stdout.is_empty() {
            bail!(
                "arx dump вернул пустой файл для `{}` из {}",
                book.internal_path,
                archive.display()
            );
        }
        fs::write(dest, &output.stdout)
            .with_context(|| format!("не удалось записать {}", dest.display()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Отдельный временный каталог на каждый тест.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("library_gui_test_{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Кэш чтения: считает файлы, режет до предела и очищается.
    #[test]
    fn cache_usage_prune_and_clear() -> Result<()> {
        let dir = scratch("cache");
        let lib = Library::open(&dir)?;
        let cache = lib.cache_dir();
        assert!(cache.is_dir(), "каталог кэша должен создаваться сам");

        for name in ["a.epub", "b.epub", "c.epub"] {
            fs::write(cache.join(name), vec![0u8; 1000])?;
        }
        assert_eq!(lib.cache_usage(), (3, 3000));

        // Предел 1000 байт — должны уйти ровно два файла.
        assert_eq!(lib.prune_cache(1000)?, 2);
        let (files, bytes) = lib.cache_usage();
        assert_eq!(files, 1);
        assert!(bytes <= 1000, "после обрезки осталось {bytes} байт");

        // Ничего не режем, если уже влезаем.
        assert_eq!(lib.prune_cache(1000)?, 0);

        assert_eq!(lib.clear_cache()?, 1);
        assert_eq!(lib.cache_usage(), (0, 0));

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Жанры переводятся из кодов при загрузке, а не хранятся готовыми.
    #[test]
    fn genres_come_from_codes() {
        assert_eq!(crate::genres::name("popadancy"), "Попаданцы");
        assert_eq!(crate::genres::name("sci_history"), "История");
    }

    fn sample_book(id: u64) -> BookMetadata {
        BookMetadata {
            id,
            title: format!("Книга {id}"),
            author: "Автор".into(),
            genre_codes: vec!["prose".into()],
            genres: Vec::new(),
            archive: "1-2.arx".into(),
            internal_path: format!("books/{id}.epub"),
            cover: None,
            details: Details::default(),
            size: 100,
        }
    }

    /// Обложки лежат в папке своей партии импорта, а не одной кучей.
    #[test]
    fn covers_live_in_the_batch_folder() {
        assert_eq!(
            Library::cover_rel_path("821884-821898", 821884, "avif"),
            "821884-821898/821884.avif"
        );
        assert_eq!(Library::cover_rel_path("1-2", 1, "webp"), "1-2/1.webp");
        // Разные партии не перемешиваются.
        assert_ne!(
            Library::cover_rel_path("1-2", 1, "avif"),
            Library::cover_rel_path("3-4", 1, "avif")
        );
    }

    /// Метаданные уезжают в файл партии и читаются обратно, а единый файл не пишется.
    #[test]
    fn metadata_goes_to_the_batch_file_and_reads_back() -> Result<()> {
        let dir = scratch("meta_batch");
        let books: Vec<BookMetadata> = (1..=5u64).map(sample_book).collect();
        let meta_dir;
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("1-5", &books)?;
            meta_dir = lib.metadata_dir();
            assert!(
                !lib.legacy_metadata_path().exists(),
                "единый metadata.jsonl больше не создаётся"
            );
        }
        // Один входной ZIP — один файл, сколько бы книг в нём ни было.
        let files = fs::read_dir(&meta_dir)?.count();
        assert_eq!(files, 1, "пять книг одной партии — один файл");
        assert!(meta_dir.join("1-5.jsonl").exists());

        let lib = Library::open(&dir)?;
        assert_eq!(lib.books.len(), 5);
        assert_eq!(lib.books[&3].title, "Книга 3");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Две партии — два файла, и обе читаются.
    #[test]
    fn two_batches_are_two_files() -> Result<()> {
        let dir = scratch("meta_two_batches");
        let meta_dir;
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("1-3", &(1..=3u64).map(sample_book).collect::<Vec<_>>())?;
            lib.append_metadata("4-6", &(4..=6u64).map(sample_book).collect::<Vec<_>>())?;
            meta_dir = lib.metadata_dir();
        }
        assert_eq!(fs::read_dir(&meta_dir)?.count(), 2);
        let lib = Library::open(&dir)?;
        assert_eq!(lib.books.len(), 6);
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Старая библиотека с единым `metadata.jsonl` должна открываться.
    #[test]
    fn legacy_single_file_is_still_read() -> Result<()> {
        let dir = scratch("meta_legacy");
        {
            let lib = Library::open(&dir)?;
            let mut f = fs::File::create(lib.legacy_metadata_path())?;
            for id in 1..=2u64 {
                writeln!(f, "{}", serde_json::to_string(&sample_book(id))?)?;
            }
        }
        let lib = Library::open(&dir)?;
        assert_eq!(lib.books.len(), 2, "старый единый файл должен читаться");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Битая строка стоит одной книги, а не всего шарда.
    #[test]
    fn broken_line_loses_one_book_not_the_file() -> Result<()> {
        let dir = scratch("meta_broken");
        let meta_dir;
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("1-3", &(1..=3u64).map(sample_book).collect::<Vec<_>>())?;
            meta_dir = lib.metadata_dir();
        }
        // Портим одну запись, как это сделал бы оборванный на середине импорт.
        let shard = meta_dir.join("1-3.jsonl");
        let mut text = fs::read_to_string(&shard)?;
        text.push_str("{\"id\": 999, \"title\": \"оборва");
        fs::write(&shard, text)?;

        let lib = Library::open(&dir)?;
        assert_eq!(lib.books.len(), 3, "целые книги должны остаться");
        assert!(!lib.books.contains_key(&999), "оборванная запись не читается");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
    /// Выдача листается страницами: без пропусков, без повторов, по алфавиту.
    ///
    /// Страницы собираются кучей ограниченного размера, а не сортировкой всей
    /// библиотеки, — этот тест стережёт, что отбор при этом остаётся честным.
    #[test]
    fn search_pages_partition_the_result() -> Result<()> {
        let dir = scratch("paging");
        let books: Vec<BookMetadata> = (1..=25u64).map(sample_book).collect();
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("1-25", &books)?;
        }
        let lib = Library::open(&dir)?;

        // По три книги на страницу: девять страниц, последняя неполная.
        let mut seen: Vec<u64> = Vec::new();
        for page in 0..9 {
            let out = lib.search("", None, None, page * 3, 3)?;
            assert_eq!(out.matched, 25, "всего книг в выдаче");
            assert!(out.books.len() <= 3, "страница не больше запрошенного");
            seen.extend(out.books.iter().map(|b| b.id));
        }
        assert_eq!(seen.len(), 25, "страницы должны покрыть всю выдачу");
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 25, "одна книга не должна попасть дважды");

        // Сквозной алфавитный порядок: страница продолжает предыдущую.
        let all = lib.search("", None, None, 0, 25)?;
        assert_eq!(all.books.len(), 25);
        assert!(
            all.books.windows(2).all(|w| w[0].title <= w[1].title),
            "выдача должна идти по алфавиту"
        );
        assert_eq!(
            all.books.iter().map(|b| b.id).collect::<Vec<_>>(),
            seen,
            "страницы и цельная выдача — один и тот же порядок"
        );

        // Смещение за пределами выдачи даёт пустую страницу, а не панику.
        let tail = lib.search("", None, None, 100, 10)?;
        assert!(tail.books.is_empty());
        assert_eq!(tail.matched, 25);

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
}
