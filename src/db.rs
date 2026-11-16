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

use tantivy::collector::{Count, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, FuzzyTermQuery, Occur, PhraseQuery, Query,
    TermQuery,
};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, Term, TextFieldIndexing, TextOptions, Value, FAST, INDEXED,
    STORED, STRING, TEXT,
};
use tantivy::tokenizer::{
    Language, LowerCaser, SimpleTokenizer, Stemmer, TextAnalyzer, TokenStream,
};
use tantivy::{
    doc, DocAddress, Index, IndexReader, IndexWriter, Order, ReloadPolicy, Score, Searcher,
    TantivyDocument,
};

/// Бюджет памяти Tantivy при индексации — не больше 50 МБ (требование ТЗ).
pub const INDEX_MEMORY_BUDGET: usize = 50 * 1024 * 1024;

/// Сколько книг за один коммит при полной пересборке индекса.
///
/// Один коммит на полмиллиона книг копит в памяти сотни сегментов; на пачках
/// по 20 тысяч коммитов 25 штук, и памяти всё равно не больше бюджета writer.
pub const INDEX_CHUNK: usize = 20_000;

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

/// Сколько книг Tantivy отдаёт для построения фасетов (списков авторов и
/// жанров).
///
/// Раньше это был предел выдачи **до** фильтрации; теперь фильтры — часть
/// запроса, и предел ограничивает только вспомогательный проход по фасетам,
/// чтобы не тянуть всю библиотеку в память. Фасеты всё равно обрезаются до
/// [`FACET_LIMIT`] пунктов.
pub const SEARCH_FETCH_CAP: usize = 20_000;

/// Меньше этого числа слов — не книга, а статья, заметка или обрывок.
///
/// 12 500 слов — это примерно 50 страниц в читалке (≈250 слов на страницу).
/// Записи короче прячутся из выдачи, кроме двух случаев: стихов (см.
/// [`is_poem`]) и малых форм по жанру (см. [`is_short_form`]). Имя автора
/// короткую вещь не спасает — вернуть конкретного автора можно списком
/// [`KEEP_AUTHORS_FILE`].
pub const MIN_BOOK_WORDS: u64 = 12_500;

/// Совсем пустые записи: обложка, оглавление и пара абзацев.
///
/// У «Хроник Гонзо» 21 КБ и ровно 5 слов — в EPUB лежат только оглавление и
/// заголовок, это проверено вручную по содержимому архива. Такие записи не
/// спасает ни имя автора, ни жанр: это не книга ни при каком пороге.
pub const JUNK_WORDS: u64 = 100;

/// Меньше этого размера файл — не книга, а брошюра.
///
/// 40 КБ — это тонкая книжка: столько весит EPUB с одной главой. Порог
/// жёсткий, как и у пустышек: имя автора и жанр тут не помогают. Настоящие
/// книги с картинками и нотами (сонатины Бетховена — 5,6 МБ) проходят
/// свободно, потому что весят больше.
pub const MIN_FILE_BYTES: u64 = 40_000;

/// Жанровые коды стихов.
///
/// Стихи короткие по природе: «Стихи» Апухтина — это 1 143 слова, и по общему
/// счёту они попали бы в мусор. Поэтому стихи не прячутся никогда.
const POEM_GENRES: &[&str] = &[
    "poetry",
    "poetry_classical",
    "poetry_east",
    "poetry_rus_classic",
    "poetry_",
    "humor_verse",
    "child_verse",
];

/// Стихи ли это: короткие по природе, поэтому не прячутся.
pub fn is_poem(book: &BookMetadata) -> bool {
    book.genre_codes
        .iter()
        .any(|code| POEM_GENRES.contains(&code.as_str()))
}

/// Жанровые коды малых форм: рассказ, сказка, анекдот, миф.
///
/// Жанр в метаданных размечен неровно — у «Теремка» Булычёва стоит `sf`, у
/// песен Визбора `prose_rus_classic`, — поэтому исключение по жанру ловит
/// только то, что размечено верно, и его дополняет список
/// [`KEEP_AUTHORS_FILE`].
const SHORT_FORM_GENRES: &[&str] = &[
    "short_story",
    "prose_short",
    "child_tale",
    "humor_anecdote",
    "myth",
];

/// Малая форма ли это: рассказ, сказка, анекдот — короткие по природе.
pub fn is_short_form(book: &BookMetadata) -> bool {
    book.genre_codes
        .iter()
        .any(|code| SHORT_FORM_GENRES.contains(&code.as_str()))
}

/// Файлы со списками авторов рядом с данными: по одному автору в строке,
/// `#` — комментарий. Оба файла необязательны.
pub const KEEP_AUTHORS_FILE: &str = "keep_authors.txt";
pub const HIDE_AUTHORS_FILE: &str = "hide_authors.txt";

/// Прочитать список авторов из файла.
///
/// Имена приводим к нижнему регистру и сравниваем по вхождению: тогда хватит
/// фамилии — «Чехов» найдётся и в «Антон Павлович Чехов», и в «А. П. Чехов».
fn read_author_list(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new(); // файла нет — значит и списка нет
    };
    text.lines()
        .map(|line| {
            line.split('#')
                .next()
                .unwrap_or_default()
                .trim()
                .to_lowercase()
        })
        .filter(|line| !line.is_empty())
        .collect()
}

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
    /// Сколько слов в тексте книги.
    ///
    /// Нужно, чтобы отличать книги от статей, заметок и миниатюр: объём
    /// текста у них на порядок меньше, а больше ничем они не выделяются —
    /// жанровый код у них обычный (`sf`, `prose_classic`, `poetry`).
    /// Размер файла для этого не годится: стихи на 79 слов весят 1.2 МБ,
    /// если это ноты или скан.
    ///
    /// `None` — счёт не проставлен (записи, импортированные до появления
    /// поля). Такие считаем книгами и не прячем: пусть лучше лишнее
    /// покажется, чем пропадёт нужное.
    #[serde(default)]
    pub words: Option<u64>,
    /// Прятать ли запись из выдачи — см. [`Library::hides`].
    ///
    /// Считается при загрузке библиотеки и в файлы метаданных не пишется:
    /// это производное от `words`, жанра и списков авторов, и после правки
    /// списков достаточно перезапустить программу, а не переписывать
    /// половину гигабайта записей.
    #[serde(skip)]
    pub hidden: bool,
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
    ///
    /// При непустом запросе считается по верхушке [`SEARCH_FETCH_CAP`], как и
    /// раньше: это счётчик для фасетов, а не для листания.
    pub matched: usize,
    /// Сколько книг прошло фильтры — по этому числу и листается выдача.
    ///
    /// Считается `Count` в том же проходе, что и страница, поэтому не зависит
    /// от [`SEARCH_FETCH_CAP`] и размера страницы.
    pub filtered: usize,
    /// `true`, если книги пришли из **резервного** запроса, допускающего одну
    /// опечатку в слове названия, то есть совпадения приблизительные.
    ///
    /// Признак ставится только когда строгий запрос не нашёл ничего и каскад
    /// сработал (см. [`Library::search`]). Интерфейс и консоль по нему честно
    /// предупреждают, что в запросе, возможно, опечатка.
    pub approximate: bool,
}

/// Режим отбора страницы: что считать лучшей книгой.
///
/// Куча — max-heap: её вершина `peek()` — «худший» из набранных, и именно он
/// вытесняется, когда книг становится больше `keep`. Значит «худший» должен
/// быть **наибольшим** по `Ord`, а `into_sorted_vec` (возрастание по `Ord`)
/// отдаёт книги от лучшей к худшей.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    /// Пустой запрос — обычный просмотр библиотеки: алфавит по названию.
    /// Худший — самый правый по алфавиту.
    Alphabet,
    /// Непустой запрос — релевантность Tantivy: лучшая книга (больший score)
    /// первой. Худший — с наименьшим score.
    Relevance,
}

/// Ключ отбора: по нему куча решает, кто «худший», и он же задаёт порядок
/// страницы после `into_sorted_vec`.
///
/// Порядок зависит от режима:
///   * [`SortMode::Alphabet`] — по названию в нижнем регистре, затем `id`;
///   * [`SortMode::Relevance`] — по score **в обратную сторону**: больший
///     score сравнивается как меньший. Поэтому в вершине max-heap оказывается
///     книга с наименьшим score (её и вытесняем), а `into_sorted_vec` даёт
///     убывание score — лучшая книга первой.
///
/// Название и `id` — устойчивый дополнительный ключ при равных score (и при
/// равных названиях в алфавитном режиме). Без него порядок равных элементов
/// был бы произвольным, и пагинация «дрожала» бы между страницами.
#[derive(Debug, Clone, Copy)]
struct SortKey<'a> {
    key: &'a str,
    score: Score,
    id: u64,
    mode: SortMode,
}

impl Ord for SortKey<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        debug_assert_eq!(self.mode, other.mode, "ключи из разных режимов");
        match self.mode {
            SortMode::Alphabet => self
                .key
                .cmp(other.key)
                .then_with(|| self.id.cmp(&other.id)),
            // Сравнение по score перевёрнуто: наибольший по `Ord` — наименьший
            // по score, то есть худший. `into_sorted_vec` отдаст убывание.
            SortMode::Relevance => other
                .score
                .total_cmp(&self.score)
                .then_with(|| self.key.cmp(other.key))
                .then_with(|| self.id.cmp(&other.id)),
        }
    }
}

impl PartialOrd for SortKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SortKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for SortKey<'_> {}

/// Книга в куче отбора страницы.
///
/// `Ord` целиком опирается на [`SortKey`]: в режиме [`SortMode::Alphabet`] это
/// алфавит по возрастанию, в режиме [`SortMode::Relevance`] — убывание score.
struct PageEntry {
    key: String,
    score: Score,
    mode: SortMode,
    book: BookMetadata,
}

impl PageEntry {
    /// Ключ отбора этой книги.
    fn sort_key(&self) -> SortKey<'_> {
        SortKey {
            key: &self.key,
            score: self.score,
            id: self.book.id,
            mode: self.mode,
        }
    }
}

impl Ord for PageEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl PartialOrd for PageEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for PageEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PageEntry {}

/// Промежуточный накопитель: фасеты и книги страницы за один проход.
/// Какие книги показывать — по наличию обложки.
///
/// Переключателей в окне два, и они взаимоисключающие: «только с обложкой»
/// скрывает книги без обложки, «без обложки» оставляет только их. Снятые оба
/// показывают всё. Вариантов ровно три, поэтому и тип такой: пара независимых
/// флагов допускала бы противоречивое «и с обложкой, и без».
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CoverFilter {
    /// Все книги, как есть.
    #[default]
    All,
    /// Только книги с обложкой — без обложки скрыты.
    WithCover,
    /// Только книги без обложки.
    WithoutCover,
}

impl CoverFilter {
    /// Галочка «только с обложкой».
    pub fn with_cover(self) -> bool {
        self == Self::WithCover
    }

    /// Галочка «без обложки».
    pub fn without_cover(self) -> bool {
        self == Self::WithoutCover
    }
}

struct Facets {
    authors: HashMap<String, usize>,
    genres: HashMap<String, usize>,
    /// Сколько книг нужно вместе с пропущенными: `offset + limit`.
    ///
    /// Весь корпус в векторе держать нельзя — в библиотеке бывают сотни тысяч
    /// книг, — поэтому в куче остаются только `keep` лучших по [`SortMode`]
    /// книг, и память не зависит от размера библиотеки.
    keep: usize,
    books: BinaryHeap<PageEntry>,
    matched: usize,
    filtered: usize,
    /// Показывать ли статьи и заметки: см. [`SHORT_WORDS`].
    show_short: bool,
    /// Какие книги показывать по обложке: см. [`CoverFilter`].
    ///
    /// Сама фильтрация теперь выражена клаузой запроса (см.
    /// [`Library::filter_clauses`]), поэтому в Rust-проходах поле не
    /// участвует. Не удаляем: через него накопитель получает режим обложки, и
    /// на него опирается публичный [`CoverFilter`], который передают GUI и CLI.
    #[allow(dead_code)]
    cover: CoverFilter,
    /// Что считать лучшей книгой — алфавит или релевантность.
    mode: SortMode,
}

impl Facets {
    fn new(keep: usize, show_short: bool, cover: CoverFilter, mode: SortMode) -> Self {
        Self {
            authors: HashMap::new(),
            genres: HashMap::new(),
            keep,
            books: BinaryHeap::new(),
            matched: 0,
            filtered: 0,
            show_short,
            cover,
            mode,
        }
    }

    /// Учесть книгу в фасетах и счётчике `matched`.
    ///
    /// Вызывается по выдаче **без** фильтров: иначе после выбора автора в
    /// выпадающем списке остался бы только он. Фильтры по автору, жанру и
    /// обложке сюда не заходят — они уже клаузы запроса, и книги приходят
    /// отфильтрованными.
    fn count(&mut self, book: &BookMetadata) {
        // Статьи и заметки отсеиваем до всех счётчиков: иначе фасеты
        // предложат автора или жанр, за которыми не видно ни одной книги.
        if !self.show_short && book.hidden {
            return;
        }
        self.matched += 1;
        if !book.author.trim().is_empty() {
            *self.authors.entry(book.author.clone()).or_default() += 1;
        }
        for g in &book.genres {
            *self.genres.entry(g.clone()).or_default() += 1;
        }
    }

    /// Положить книгу в отбор страницы.
    ///
    /// Книга уже прошла фильтры запроса; здесь остаются только правила
    /// видимости (`hidden`), которые сознательно живут пост-фильтром.
    ///
    /// `true` — книга видима и принята (даже если потом вытеснена из кучи);
    /// `false` — спрятана. По этому признаку ветка пустого запроса считает
    /// `filtered`: спрятанное не должно попадать в число страниц.
    fn pick(&mut self, book: &BookMetadata, score: Score) -> bool {
        if !self.show_short && book.hidden {
            return false;
        }
        let key = book.title.to_lowercase();
        // Книга, которая не лучше худшей из уже набранных, на страницу не
        // попадёт — и копировать её незачем. Что значит «лучше», решает
        // [`SortKey`]: в алфавитном режиме — левее по названию, в режиме
        // релевантности — больше score.
        if self.books.len() >= self.keep {
            if let Some(worst) = self.books.peek() {
                let candidate = SortKey {
                    key: &key,
                    score,
                    id: book.id,
                    mode: self.mode,
                };
                if candidate >= worst.sort_key() {
                    return true;
                }
            }
        }
        self.books.push(PageEntry {
            key,
            score,
            mode: self.mode,
            book: book.clone(),
        });
        if self.books.len() > self.keep {
            self.books.pop();
        }
        true
    }
}

/// Отсортировать фасет: сначала по числу книг, затем по алфавиту.
fn sort_counts(map: HashMap<String, usize>) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(FACET_LIMIT);
    v
}

/// Обязательная клауза запроса, не влияющая на релевантность.
///
/// В Tantivy нет `Occur::Filter` (в отличие от Lucene), поэтому фильтр — это
/// `MUST` с `ConstScoreQuery(_, 0.0)`: документ обязан совпасть, но score не
/// меняется. Ноль здесь дешевле `BoostQuery(_, 0.0)`: BM25-вес для такой
/// клаузы не считается вовсе.
fn filter_clause(term: Term) -> (Occur, Box<dyn Query>) {
    (
        Occur::Must,
        Box::new(ConstScoreQuery::new(
            Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            0.0,
        )),
    )
}

/// Открытая библиотека.
/// Имя анализатора `title`/`author` с русским стеммером (см. [`ru_stem_analyzer`]).
const RU_STEM_TOKENIZER: &str = "ru_stem";

/// Анализатор названий и авторов: разбивка на слова + нижний регистр + стеммер.
///
/// Порядок фильтров важен: `Stemmer` ожидает уже приведённый к нижнему
/// регистру текст, поэтому `LowerCaser` идёт до него.
///
/// Tantivy хранит в `meta.json` только **имя** анализатора, а сам менеджер
/// токенизаторов при `Index::open_in_dir` создаётся заново. Поэтому анализатор
/// надо регистрировать и после открытия, и до создания `IndexWriter`:
/// writer работает с тем же менеджером, что и [`Library::index`].
fn ru_stem_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(Stemmer::new(Language::Russian))
        .build()
}

/// Нормализация значения для полей-ключей `title_key`/`author_key`.
///
/// Ключ — это нормализованное название (или автор) целиком, одним термом.
/// Одна и та же функция обязана применяться и при индексации, и при запросе,
/// иначе термы не совпадут. Приводим к нижнему регистру, любую
/// последовательность не-буквенно-цифровых символов — к одному пробелу,
/// обрезаем края и отдельно сворачиваем «ё» в «е»: пользователь печатает
/// «Королев», а на обложке «Королёв», и это должно считаться точным
/// совпадением. Стеммер к ключам не применяется — иначе точное совпадение
/// превратилось бы в совпадение по основе.
///
/// Видна всему крейту: по ней же [`crate::author_suggest::AuthorDictionary`]
/// строит ключи подсказок — они обязаны совпадать с термами `author_key`.
pub(crate) fn normalize_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        // `to_lowercase` для 'Ё' даёт 'ё', поэтому свёртка ниже ловит оба регистра.
        for lower in ch.to_lowercase() {
            let lower = if lower == 'ё' { 'е' } else { lower };
            if lower.is_alphanumeric() {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                out.push(lower);
            } else {
                pending_space = true;
            }
        }
    }
    out
}

/// Год книги числом — для поля `year`.
///
/// Источник — «Год написания или издания» (`Details::year`), а если оно пустое,
/// то дата (`Details::date`): в старых записях год лежит только в дате. Из
/// строки берём ведущие цифры, поэтому «1967 г.» и «1967-1968» дают 1967.
///
/// **0 означает «год неизвестен»**, а не нулевой год. Это записано в контракт
/// поля, потому что по нему дальше будет диапазонный фильтр: `year >= 1900`
/// такие книги сам собой исключит, а `year <= 2000` — включит, и это должно
/// быть осознанным решением, а не случайностью разбора. Пустая строка, «—» и
/// любой мусор без ведущих цифр дают 0.
fn book_year(details: &Details) -> i64 {
    let text = if details.year.trim().is_empty() {
        details.date.as_str()
    } else {
        details.year.as_str()
    };
    let digits: String = text
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    // `parse` не может переполниться на разумных годах, но мусор вроде
    // строки из сотни цифр не должен ронять индексацию: тогда 0.
    digits.parse::<i64>().unwrap_or(0)
}
struct IndexFields {
    id: Field,
    title: Field,
    author: Field,
    title_key: Field,
    author_key: Field,
    genres: Field,
    genre: Field,
    year: Field,
    has_cover: Field,
    archive: Field,
    path: Field,
}

/// Схема индекса — единственное место, где перечислены его поля.
///
/// [`Library::open`] сверяет с этой схемой фактическую схему открытого
/// индекса: если набор полей или их параметры разошлись, индекс устарел и его
/// надо собрать заново. Сравнение по схеме, а не по отдельному файлу-версии,
/// работает и для индексов, созданных до появления такого файла, и не требует
/// синхронизировать два источника истины.
fn build_schema() -> (Schema, IndexFields) {
    let mut builder = Schema::builder();
    // `FAST` у `id` — не для сортировки выдачи, а для ветки пустого запроса:
    // там нужно получить `id` всех книг, подошедших под фильтр, не читая
    // stored-поля каждой из них (на обложечном фильтре это сотни тысяч
    // документов).
    let f_id = builder.add_u64_field("id", INDEXED | STORED | FAST);
    // `title` — стеммер ради «преступления» → «преступление» и позиции ради
    // фразового запроса: без позиций `PhraseQuery` не построится.
    let title_indexing = TextFieldIndexing::default()
        .set_tokenizer(RU_STEM_TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let f_title = builder.add_text_field(
        "title",
        TextOptions::default()
            .set_indexing_options(title_indexing)
            .set_stored(),
    );
    // Автор тоже стеммится, но позиции ему не нужны: фраз по автору нет.
    let author_indexing = TextFieldIndexing::default()
        .set_tokenizer(RU_STEM_TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqs);
    let f_author = builder.add_text_field(
        "author",
        TextOptions::default()
            .set_indexing_options(author_indexing)
            .set_stored(),
    );
    // Ключи — `STRING` (raw-токенизатор): нормализованное значение целиком
    // лежит одним термом, поэтому `Basic` (без частот и позиций) достаточно.
    let f_title_key = builder.add_text_field("title_key", STRING | STORED);
    let f_author_key = builder.add_text_field("author_key", STRING | STORED);
    // Старое поле с жанрами одной строкой. Оставлено (в stored-виде) для
    // совместимости с уже собранными данными и отладки; поиск и фильтр его не
    // используют — для фильтра ниже есть multi-value `genre`.
    let f_genres = builder.add_text_field("genres", TEXT | STORED);
    // Жанр для фильтра: `STRING` — та же [`normalize_key`], что у ключей, без
    // стеммера, значение целиком одним термом. Поле **multi-value**: на каждое
    // название жанра книги пишется отдельное значение, а не склеенная строка,
    // поэтому `TermQuery` по любому из жанров находит книгу. `FAST` — чтобы
    // поле можно было читать колонкой, не разбирая документ.
    let f_genre = builder.add_text_field("genre", STRING | FAST);
    // Год: `i64`, потому что дальше по нему будет диапазонный фильтр.
    // 0 — «год неизвестен» (см. [`book_year`]); это записано в контракт поля.
    let f_year = builder.add_i64_field("year", INDEXED | STORED | FAST);
    // Обложка: 1 — есть, 0 — нет. `u64`, а не bool: фильтр — это `TermQuery`
    // по числу, и `FAST` позволяет читать значение колонкой.
    let f_has_cover = builder.add_u64_field("has_cover", INDEXED | FAST);
    let f_archive = builder.add_text_field("archive", STRING | STORED);
    let f_path = builder.add_text_field("internal_path", STRING | STORED);
    let schema = builder.build();
    (
        schema,
        IndexFields {
            id: f_id,
            title: f_title,
            author: f_author,
            title_key: f_title_key,
            author_key: f_author_key,
            genres: f_genres,
            genre: f_genre,
            year: f_year,
            has_cover: f_has_cover,
            archive: f_archive,
            path: f_path,
        },
    )
}

/// Совпадают ли схемы по составу полей и их параметрам.
///
/// Сравниваются только имена и типы: порядок полей в `meta.json` может
/// отличаться, а `Field`-номера привязаны к конкретной схеме и для сравнения
/// двух разных схем смысла не имеют.
fn schemas_match(actual: &Schema, expected: &Schema) -> bool {
    let mut actual: Vec<(&str, &tantivy::schema::FieldType)> = actual
        .fields()
        .map(|(_, entry)| (entry.name(), entry.field_type()))
        .collect();
    let mut expected: Vec<(&str, &tantivy::schema::FieldType)> = expected
        .fields()
        .map(|(_, entry)| (entry.name(), entry.field_type()))
        .collect();
    actual.sort_by_key(|(name, _)| *name);
    expected.sort_by_key(|(name, _)| *name);
    actual == expected
}

/// Куда переезжает старый каталог индекса при смене схемы.
///
/// Проект не удаляет данные: устаревший индекс не стирается, а переименовывается
/// в `index.bak-<метка времени>`, чтобы его можно было вернуть или разобрать.
/// Если имя уже занято (метка — это секунды, и перезапуск после сбоя мог
/// попасть в ту же секунду), добавляем счётчик: затирать предыдущую копию
/// нельзя.
fn index_backup_path(data_dir: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let base = data_dir.join(format!("index.bak-{stamp}"));
    if !base.exists() {
        return base;
    }
    for n in 1u32.. {
        let candidate = data_dir.join(format!("index.bak-{stamp}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("свободное имя для копии индекса найдётся всегда")
}

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
    /// Нормализованное название целиком — для точного совпадения.
    f_title_key: Field,
    /// Нормализованный автор целиком — для точного совпадения.
    f_author_key: Field,
    /// Старое поле с жанрами одной строкой (stored, поиском не используется).
    f_genres: Field,
    /// Жанр для фильтра — multi-value, нормализованный [`normalize_key`].
    f_genre: Field,
    /// Год: 0 — «неизвестен» (см. [`book_year`]).
    f_year: Field,
    /// 1 — обложка есть, 0 — нет.
    f_has_cover: Field,
    f_archive: Field,
    f_path: Field,
    /// `true`, если при открытии схема индекса оказалась устаревшей и индекс
    /// уже пересобран в новой. Нужен `--reindex`, чтобы не делать ту же работу
    /// второй раз.
    pub schema_upgraded: bool,
    /// `id` → метаданные, загружено из `data/metadata/`.
    pub books: HashMap<u64, BookMetadata>,
    /// Авторы из `keep_authors.txt`: их короткое тоже остаётся. Сравнение —
    /// по вхождению без регистра, чтобы хватило фамилии («Чехов»).
    keep_authors: Vec<String>,
    /// Авторы из `hide_authors.txt`: у них короткое прячется всегда, даже
    /// если правило выше их считает настоящими.
    hide_authors: Vec<String>,
    /// Сколько раз реально выполнялся резервный (fuzzy) запрос.
    ///
    /// Существует только в тестовой сборке: в бою счётчик не нужен, а тестам
    /// он позволяет отличить «резервный запрос не запускался» от «запускался,
    /// но ничего не нашёл».
    #[cfg(test)]
    fallback_searches: std::sync::atomic::AtomicU64,
}

// --- Полнотекстовый запрос: границы и веса ---------------------------------
//
// Константы относятся к [`Library::build_full_text_query`].

/// Максимум термов, взятых из запроса.
///
/// Каждый терм — это `MUST`-группа из двух-трёх клауз, и на длинный ввод
/// получился бы `BooleanQuery` в сотни клауз: он дольше собирается и
/// считается, а пользы не добавляет. Лишние термы просто отбрасываются.
const MAX_QUERY_TERMS: usize = 16;

/// Термы короче этого не ищутся нечётко.
///
/// При расстоянии Левенштейна 1 у трёхбуквенного терма «почти любое» другое
/// слово оказывается соседом, и fuzzy-клауза начинает шуметь сильнее, чем
/// помогать. Четыре буквы — минимальная длина, на которой опечатка ещё
/// выделяется.
const FUZZY_MIN_TERM_LEN: usize = 4;

/// Расстояние Левенштейна для fuzzy: ровно одна опечатка.
const FUZZY_DISTANCE: u8 = 1;

// --- Резервный запрос: fuzzy по названию -----------------------------------
//
// Эти константы относятся только к каскаду [`Library::search`]: строгий запрос
// fuzzy по `title` не использует вовсе (см. [`Library::build_full_text_query`]).

/// Fuzzy по `title` — только для стемов не короче этого.
///
/// Порог выше, чем у автора ([`FUZZY_MIN_TERM_LEN`] = 4), и считается по
/// **стему**: словарь названий на порядок больше словаря авторов, и при
/// расстоянии 1 четырёхбуквенный стем находит «почти любое» слово («дом» ↔
/// «том», «сад» ↔ «зад»). Пять букв — минимум, на котором опечатка ещё
/// выделяется, а мусор не лезет в выдачу.
const FUZZY_TITLE_MIN_TERM_LEN: usize = 5;

/// Расстояние Левенштейна для fuzzy по `title` — ровно одна опечатка.
const FUZZY_TITLE_DISTANCE: u8 = 1;

/// С этой длины стема для fuzzy по `title` допускается вторая опечатка.
///
/// Замер на временном индексе (100 000 книг, тест `benchmark_fallback_query`)
/// **не** показал, что расстояние 2 дороже: запрос из трёх fuzzy-клауз стоил
/// ≈1,1–1,4 мс при расстоянии 1 и ≈1,0–1,4 мс при расстоянии 2 — разница в
/// пределах шума, потому что время съедает не автомат Левенштейна, а
/// объединение термов. Лишних находок расстояние 2 в этом сценарии тоже не
/// дало. Поэтому в боевом режиме оно выключено тумблером
/// [`FUZZY_TITLE_ALLOW_DISTANCE_2`] как более точный вариант, а эти константы
/// оставлены как зафиксированный результат замера.
const FUZZY_TITLE_LONG_TERM_LEN: usize = 9;
const FUZZY_TITLE_DISTANCE_LONG: u8 = 2;

/// Разрешить две опечатки на длинных стемах в резервном запросе.
///
/// Выключено: замер не показал ни ускорения, ни выигрыша в находках, зато
/// расстояние 2 расширяет словарь кандидатов и повышает риск мусора в выдаче —
/// а требование прямо просит при сомнении оставлять 1. `true` включает правило
/// `FUZZY_TITLE_LONG_TERM_LEN` / `FUZZY_TITLE_DISTANCE_LONG`.
const FUZZY_TITLE_ALLOW_DISTANCE_2: bool = false;

/// Русские служебные слова: fuzzy-клаузу по `title` они не получают.
///
/// Список проверяется по **исходному** слову запроса (в нижнем регистре), а не
/// по стему: так он читается человеком («чтобы», а не «чтоб»). Короткие слова
/// («и», «в», «на») и так отсекает порог длины, но проверка нужна отдельно:
/// длинным служебным словам («чтобы», «который») порог не помеха, а fuzzy по
/// ним — чистый шум.
const STOP_WORDS_RU: &[&str] = &[
    "и", "в", "во", "на", "не", "что", "с", "со", "а", "по", "за", "из", "к", "ко", "о", "об",
    "от", "для", "же", "бы", "ли", "или", "но", "да", "ни", "у", "при", "про", "без", "над",
    "под", "это", "как", "то", "все", "чтобы", "который", "когда", "если", "этот", "весь",
];

/// Вес fuzzy-терма по названию внутри резервного запроса.
///
/// Ниже точного терма по `title` ([`BOOST_TITLE_TERM`] = 2.0): приблизительное
/// совпадение обязано стоять ниже точного. Клауза добавлена как `SHOULD` внутри
/// `MUST`-группы терма, а не заменяет точный терм.
const BOOST_TITLE_FUZZY: Score = 0.5;

/// Вес самого строгого запроса как `SHOULD`-клаузы резервного.
///
/// Строгий запрос требует все термы точь-в-точь, поэтому его совпадения — это
/// подмножество совпадений любого варианта с опечаткой. Клауза нужна не для
/// отбора (он и так пройдёт), а для ранга: если точное совпадение всё-таки
/// есть, оно обязано быть выше приблизительного.
const BOOST_FALLBACK_STRICT: Score = 4.0;

/// Вес точной фразы по `title` — самый большой.
///
/// Именно он поднимает «Мастер и маргарита» над «Мастер и маргарита:
/// краткий разбор», когда оба документа содержат все термы запроса.
const BOOST_TITLE_PHRASE: Score = 4.0;

/// Вес точного терма по `title` — средний: название ищут в первую очередь.
const BOOST_TITLE_TERM: Score = 2.0;

/// Вес точного терма по `author` — ниже названия: имя автора ищут реже.
const BOOST_AUTHOR_TERM: Score = 1.0;

/// Вес fuzzy-терма по `author` — самый маленький.
///
/// Клауза добавлена как `SHOULD`, а не `MUST`: точное совпадение должно
/// выигрывать у приблизительного. Маленький boost лишь чуть поднимает
/// документ, который иначе не нашёлся бы из-за опечатки в фамилии.
const BOOST_AUTHOR_FUZZY: Score = 0.3;

/// Вес точного совпадения с `title_key` для запроса из двух и более термов.
///
/// Ключ — нормализованное название целиком, поэтому совпадение по нему и есть
/// «эта книга называется ровно так». При нескольких термах это самый сильный
/// сигнал: он должен перебивать совпадение по отдельным словам.
const BOOST_TITLE_KEY_MULTI: Score = 12.0;

/// Вес точного совпадения с `title_key` для однословного запроса.
///
/// Заметно ниже, чем [`BOOST_TITLE_KEY_MULTI`], намеренно. На запросе
/// «Маргарита» ключ книги с названием ровно «Маргарита» совпадёт целиком, а у
/// «Мастер и Маргарита» — нет, и с весом 12.0 короткая книга перебила бы
/// известный роман. 4.0 оставляет точному однословному совпадению перевес над
/// обычными термами (2.0), но не даёт ему подавить всё остальное.
const BOOST_TITLE_KEY_SINGLE: Score = 4.0;

/// Вес точного совпадения с `author_key`: ниже названия, выше обычного терма.
const BOOST_AUTHOR_KEY: Score = 6.0;

/// Один терм запроса: как его напечатал пользователь (в нижнем регистре) и во
/// что его превратил анализатор `title`.
///
/// Нужны оба: в клаузы уходит `stem` (поле `title` хранит стемы), а стоп-слова
/// проверяются по `raw` — список человеческий, по исходным словам.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryTerm {
    /// Токен после `SimpleTokenizer` + `LowerCaser`, до стеммера.
    raw: String,
    /// Тот же токен после русского стеммера — то, что лежит в индексе.
    stem: String,
}

/// Служебное ли это слово (список — [`STOP_WORDS_RU`]).
fn is_stop_word(raw: &str) -> bool {
    STOP_WORDS_RU.contains(&raw)
}

/// Расстояние Левенштейна для fuzzy по `title`: одна опечатка, а на длинных
/// стемах — вторая, если она разрешена [`FUZZY_TITLE_ALLOW_DISTANCE_2`].
fn fuzzy_title_distance(stem: &str) -> u8 {
    if FUZZY_TITLE_ALLOW_DISTANCE_2 && stem.chars().count() >= FUZZY_TITLE_LONG_TERM_LEN {
        FUZZY_TITLE_DISTANCE_LONG
    } else {
        FUZZY_TITLE_DISTANCE
    }
}

/// Индексы термов, которым в резервном запросе разрешена fuzzy-клауза по
/// стеммированному `title`. Порядок — исходный.
///
/// Отсеиваются два вида термов:
///   * стоп-слова — они не несут смысла, а fuzzy по ним находит что угодно;
///   * стемы короче [`FUZZY_TITLE_MIN_TERM_LEN`] — у короткого стема соседом по
///     Левенштейну оказывается почти любое слово.
///
/// Длина берётся от стема, потому что fuzzy ищет именно по словарю стемов.
fn title_fuzzy_terms(terms: &[QueryTerm]) -> Vec<usize> {
    terms
        .iter()
        .enumerate()
        .filter(|(_, term)| {
            !is_stop_word(&term.raw) && term.stem.chars().count() >= FUZZY_TITLE_MIN_TERM_LEN
        })
        .map(|(index, _)| index)
        .collect()
}

impl Library {
    /// Открыть библиотеку в `data_dir`, создав индекс и файлы при необходимости.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        for sub in ["archives", "covers", "index", "tmp", "cache", "metadata"] {
            fs::create_dir_all(data_dir.join(sub))
                .with_context(|| format!("не удалось создать {}/{sub}", data_dir.display()))?;
        }

        let (schema, fields) = build_schema();

        let index_dir = data_dir.join("index");
        // `meta.json` пишется уже при `create_in_dir`, поэтому его наличие
        // означает «индекс существует» и открывать надо именно его.
        let mut schema_upgraded = false;
        let index = if index_dir.join("meta.json").exists() {
            let existing = Index::open_in_dir(&index_dir)
                .with_context(|| format!("не удалось открыть индекс {}", index_dir.display()))?;
            if schemas_match(&existing.schema(), &schema) {
                existing
            } else {
                // Схема разошлась — открывать её нельзя: поля, которого ждёт
                // код, в ней просто нет, и поиск молча ничего не найдёт.
                // Старый каталог не стираем, а переименовываем, и создаём
                // индекс заново в актуальной схеме. `existing` отпускаем до
                // переименования, чтобы не держать открытыми файлы каталога.
                drop(existing);
                let backup = index_backup_path(&data_dir);
                fs::rename(&index_dir, &backup).with_context(|| {
                    format!(
                        "не удалось отодвинуть устаревший индекс {} в {}",
                        index_dir.display(),
                        backup.display()
                    )
                })?;
                schema_upgraded = true;
                // Переименование унесло каталог вместе с индексом, а
                // `create_in_dir` требует, чтобы каталог уже существовал.
                fs::create_dir_all(&index_dir).with_context(|| {
                    format!("не удалось создать {}", index_dir.display())
                })?;
                Index::create_in_dir(&index_dir, schema.clone()).with_context(|| {
                    format!("не удалось создать индекс {}", index_dir.display())
                })?
            }
        } else {
            Index::create_in_dir(&index_dir, schema.clone())
                .with_context(|| format!("не удалось создать индекс {}", index_dir.display()))?
        };

        // Регистрируем анализатор **после** открытия/создания индекса: в
        // `meta.json` лежит только его имя, а менеджер токенизаторов при
        // открытии создаётся заново. Writer делит этот менеджер с индексом
        // (Arc), поэтому регистрация здесь видна и индексации, и поиску.
        index.tokenizers().register(RU_STEM_TOKENIZER, ru_stem_analyzer());

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("не удалось создать reader Tantivy")?;

        // Списки авторов читаем до сборки: `data_dir` переезжает в структуру.
        let keep_authors = read_author_list(&data_dir.join(KEEP_AUTHORS_FILE));
        let hide_authors = read_author_list(&data_dir.join(HIDE_AUTHORS_FILE));
        let mut library = Self {
            data_dir,
            index,
            reader,
            f_id: fields.id,
            f_title: fields.title,
            f_author: fields.author,
            f_title_key: fields.title_key,
            f_author_key: fields.author_key,
            f_genres: fields.genres,
            f_genre: fields.genre,
            f_year: fields.year,
            f_has_cover: fields.has_cover,
            f_archive: fields.archive,
            f_path: fields.path,
            schema_upgraded,
            books: HashMap::new(),
            keep_authors,
            hide_authors,
            #[cfg(test)]
            fallback_searches: std::sync::atomic::AtomicU64::new(0),
        };
        library.reload_metadata()?;
        // Новый индекс пуст, а метаданные уже прочитаны — наполняем его сразу,
        // чтобы библиотека была согласованной для любого вызывающего (GUI,
        // поиск без окна, `--reindex`), а не только для явной пересборки.
        if schema_upgraded {
            library
                .rebuild_index()
                .context("не удалось пересобрать индекс в новой схеме")?;
        }
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
        self.mark_short_items();
        Ok(())
    }

    /// Собрать полный словарь авторов индекса: `(имя, число книг)`.
    ///
    /// Источник — словарь термов поля `author_key`: это `STRING`, поэтому на
    /// каждую книгу там ровно один терм — нормализованный автор целиком
    /// ([`normalize_key`]), а `doc_freq` терма и есть число книг с таким
    /// автором. Проходим по сегментам и складываем частоты: один и тот же
    /// автор после добавлений и слияний может лежать в нескольких сегментах,
    /// и `doc_freq` каждого из них — только часть ответа.
    ///
    /// В терме лежит **нормализованное** имя («михаил булгаков»), а в
    /// подсказке надо показать написание из книги. Оригиналы берём из уже
    /// загруженных метаданных: у одного ключа бывает несколько вариантов,
    /// отличающихся регистром или пунктуацией, поэтому оставляем
    /// лексикографически наименьший (детерминированно, без зависимости от
    /// порядка обхода `HashMap`). Если книги в метаданных нет — показываем
    /// сам терм: автор всё равно должен быть в подсказках.
    ///
    /// Список отсортирован по имени. В индекс эта функция не пишет ничего:
    /// Tantivy используется только на чтение, поэтому схема не меняется и
    /// размер индекса не растёт.
    pub fn author_dictionary(&self) -> Result<Vec<(String, usize)>> {
        let searcher = self.reader.searcher();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for segment in searcher.segment_readers() {
            let inverted = segment
                .inverted_index(self.f_author_key)
                .context("не удалось открыть инвертированный индекс author_key")?;
            let mut terms = inverted
                .terms()
                .stream()
                .context("не удалось прочитать словарь термов author_key")?;
            while terms.advance() {
                let key = std::str::from_utf8(terms.key())
                    .context("терм author_key оказался не UTF-8")?;
                if key.is_empty() {
                    continue;
                }
                *counts.entry(key.to_string()).or_default() += terms.value().doc_freq as usize;
            }
        }

        // Отображаемое написание: ключ нормализации → оригинал. Смотрим
        // только на ключи, которые реально есть в индексе.
        //
        // Нормализацию кэшируем по исходной строке автора: в библиотеке
        // 417 тыс. книг, но разных написаний автора заметно меньше, и
        // `normalize_key` для каждого повтора заново — это лишние сотни
        // миллисекунд на старте (замер: 808 мс → 607 мс на всей сборке).
        let mut normalized: HashMap<&str, String> = HashMap::new();
        let mut display: HashMap<String, String> = HashMap::new();
        for book in self.books.values() {
            let author = book.author.trim();
            if author.is_empty() {
                continue;
            }
            let key = normalized
                .entry(author)
                .or_insert_with(|| normalize_key(author));
            if key.is_empty() || !counts.contains_key(key.as_str()) {
                continue;
            }
            match display.get_mut(key.as_str()) {
                Some(best) => {
                    if author < best.as_str() {
                        *best = author.to_string();
                    }
                }
                None => {
                    display.insert(key.clone(), author.to_string());
                }
            }
        }

        let mut out: Vec<(String, usize)> = counts
            .into_iter()
            .map(|(key, count)| (display.remove(&key).unwrap_or(key), count))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        Ok(out)
    }

    /// Прятать ли запись из выдачи: статья, заметка или обрывок.
    ///
    /// Правило: файл мельче [`MIN_FILE_BYTES`] (40 КБ) — не книга, и текст
    /// короче [`MIN_BOOK_WORDS`] (≈50 страниц) — не книга. Второе смягчают
    /// стихи, малые формы по жанру и список `keep_authors.txt`; первое —
    /// нет. Имя автора короткую вещь не спасает: 18-страничный рассказ —
    /// такая же брошюра, как и всё остальное. Записи без счёта слов считаем
    /// книгами.
    pub fn hides(&self, book: &BookMetadata) -> bool {
        let author = book.author.trim().to_lowercase();
        // Список «прятать всегда» перебивает всё.
        if self.hide_authors.iter().any(|name| author.contains(name)) {
            return true;
        }
        // Тонкий файл — брошюра, а не книга. Исключений нет ни для кого.
        if book.size < MIN_FILE_BYTES {
            return true;
        }
        let Some(words) = book.words else {
            return false;
        };
        // Пустышка: обложка и оглавление. Тоже без исключений.
        if words < JUNK_WORDS {
            return true;
        }
        if words >= MIN_BOOK_WORDS || is_poem(book) || is_short_form(book) {
            return false;
        }
        !self.keep_authors.iter().any(|name| author.contains(name))
    }

    /// Отметить, что прячется от выдачи.
    ///
    /// Считается один раз при загрузке: библиотека меняется только вместе
    /// с метаданными. Отметка нужна интерфейсу — по ней видно, что запись
    /// показана только из-за галочки «статьи и заметки».
    fn mark_short_items(&mut self) {
        let marks: Vec<(u64, bool)> = self
            .books
            .values()
            .map(|book| (book.id, self.hides(book)))
            .collect();
        for (id, hidden) in marks {
            if let Some(book) = self.books.get_mut(&id) {
                book.hidden = hidden;
            }
        }
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

    /// Собрать индекс заново из `self.books`.
    ///
    /// Нужно после выноса дубликатов: часть книг из метаданных исчезла, и
    /// поиск не должен продолжать их находить. Старый каталог индекса к
    /// этому моменту уже убран с дороги (переехал в `dublicate/`), так что
    /// здесь достаточно очистить то, что есть, и залить метаданные заново.
    ///
    /// Тем же путём [`Library::open`] наполняет индекс, который только что
    /// создан заново из-за устаревшей схемы: там он тоже пуст, а метаданные
    /// уже прочитаны.
    ///
    /// Пачками по [`INDEX_CHUNK`]: `index_books` коммитит на каждой пачке, а
    /// один коммит на 490 тысяч книг копит в памяти слишком много сегментов.
    pub fn rebuild_index(&mut self) -> Result<()> {
        let mut writer = self.writer()?;
        writer
            .delete_all_documents()
            .context("не удалось очистить индекс")?;
        writer.commit().context("не удалось закоммитить очистку")?;
        let books: Vec<BookMetadata> = self.books.values().cloned().collect();
        for chunk in books.chunks(INDEX_CHUNK) {
            self.index_books(&mut writer, chunk)?;
        }
        writer
            .wait_merging_threads()
            .context("не удалось дождаться слияния сегментов")?;
        self.reader.reload().context("не удалось перечитать индекс")?;
        Ok(())
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
            let mut document = doc!(
                self.f_id => book.id,
                self.f_title => book.title.clone(),
                self.f_author => book.author.clone(),
                // Ключи — та же нормализация, что и в запросе
                // ([`normalize_key`]): значение целиком, одним термом.
                self.f_title_key => normalize_key(&book.title),
                self.f_author_key => normalize_key(&book.author),
                // Старое поле с жанрами одной строкой: stored, поиском и
                // фильтром не используется (см. `genre` ниже).
                self.f_genres => book.genres.join(" "),
                // Год числом; 0 — «неизвестен» (см. [`book_year`]).
                self.f_year => book_year(&book.details),
                // 1/0 вместо bool: по полю идёт `TermQuery`.
                self.f_has_cover => u64::from(book.cover.is_some()),
                self.f_archive => book.archive.clone(),
                self.f_path => book.internal_path.clone(),
            );
            // Жанр — **multi-value**: на каждое название книги отдельное
            // значение, а не склеенная строка. Тогда `TermQuery` по любому из
            // жанров книги её находит, и в фильтре не нужны ни `OR`, ни
            // разбор строки. Нормализация — та же [`normalize_key`], что и в
            // запросе, иначе термы не совпадут.
            for genre in &book.genres {
                document.add_text(self.f_genre, normalize_key(genre));
            }
            writer
                .add_document(document)
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

    /// Поиск с фильтрами по автору, жанру и обложке.
    ///
    /// Фильтры — **часть запроса** (`MUST` + `ConstScoreQuery(_, 0.0)`), а не
    /// проверка в Rust после выдачи. Поэтому редкий автор находится, даже если
    /// текстовая выдача забита другими книгами: Tantivy отбирает по фильтру
    /// до того, как обрезать результат, и книга не может «не попасть в первые
    /// [`SEARCH_FETCH_CAP`]». Фильтр по автору идёт по `author_key` через
    /// [`normalize_key`], то есть не зависит от регистра, пунктуации и «ё».
    ///
    /// Пустой текстовый запрос означает «вся библиотека». Tantivy в этом
    /// случае нужен только чтобы применить фильтр (`AllQuery` + клаузы), а
    /// сама страница собирается обходом метаданных: у `AllQuery` все score
    /// равны, порядок документов не определён, а выдача обязана идти по
    /// алфавиту. Обход идёт по всей библиотеке, поэтому предел выдачи здесь
    /// ничего не отрезает — в отличие от непустого запроса.
    ///
    /// Фасеты (списки авторов и жанров) считаются по книгам, подошедшим под
    /// **текстовый запрос**, без учёта самих фильтров, — иначе после выбора
    /// автора в выпадающем списке остался бы только он один.
    pub fn search(
        &self,
        query: &str,
        author: Option<&str>,
        genre: Option<&str>,
        show_short: bool,
        cover: CoverFilter,
        offset: usize,
        limit: usize,
    ) -> Result<SearchOutcome> {
        let query = query.trim();

        // Режим отбора страницы: непустой запрос — по релевантности (лучший
        // score первым), пустой — по алфавиту, как обычный просмотр.
        let mode = if query.is_empty() {
            SortMode::Alphabet
        } else {
            SortMode::Relevance
        };

        let keep = offset.saturating_add(limit);
        let mut acc = Facets::new(keep, show_short, cover, mode);
        // Фильтры собираются один раз и уходят в запрос — и для страницы, и
        // для пустого текста. В Rust их больше нет.
        let filter_clauses = self.filter_clauses(author, genre, cover);

        // Текстовый запрос **без** фильтров: нужен фасетам и счётчику
        // `matched`, которые обязаны видеть книги, не прошедшие фильтр.
        let text_query = if query.is_empty() {
            None
        } else {
            self.build_full_text_query(query)
                .with_context(|| format!("не удалось разобрать запрос `{query}`"))?
        };

        // Пометка «совпадения приблизительные»: ставится только каскадом
        // (см. ниже) и только если резервный запрос что-то нашёл. Пустой запрос
        // и обычный удачный поиск оставляют её ложной.
        let mut approximate = false;

        match text_query {
            None => {
                // Пустой текст: фильтр (если он есть) применяет Tantivy, но
                // страницу собираем обходом метаданных — Tantivy не даст
                // алфавитный порядок при равных score.
                let selected_ids = if filter_clauses.is_empty() {
                    None
                } else {
                    let mut clauses: Vec<(Occur, Box<dyn Query>)> =
                        Vec::with_capacity(filter_clauses.len() + 1);
                    // `AllQuery` в `MUST` — «вся библиотека», поверх неё
                    // обязательные клаузы фильтров.
                    clauses.push((Occur::Must, Box::new(AllQuery)));
                    clauses.extend(filter_clauses);
                    Some(self.matching_ids(&BooleanQuery::new(clauses))?)
                };
                for book in self.books.values() {
                    acc.count(book);
                    let selected = selected_ids
                        .as_ref()
                        .is_none_or(|ids| ids.binary_search(&book.id).is_ok());
                    // `filtered` считаем здесь же: обход идёт по всей
                    // библиотеке, поэтому счётчик точный, а не оценка.
                    if selected && acc.pick(book, 0.0) {
                        acc.filtered += 1;
                    }
                }
            }
            Some(text_query) => {
                let searcher = self.reader.searcher();

                // Строгий проход забирает запрос себе (`with_filters` его
                // оборачивает) — на горячем пути лишних клонов нет.
                let total =
                    self.collect_page(&mut acc, &searcher, text_query, filter_clauses, keep)?;

                // --- Каскад с резервным запросом ---------------------------
                //
                // Резервный запрос допускает **одну** опечатку в одном слове
                // названия и стоит заметно дороже строгого (fuzzy по словарю
                // названий — 0,6–3,9 мс на терм против десятков микросекунд
                // точного поиска), а из-за него в выдачу лезет мусор. Поэтому
                // он запускается ровно в одном случае: строгий запрос **после
                // фильтров** не нашёл ничего, а сам запрос непустой. Если
                // строгий что-то нашёл — резервный не строится и не
                // выполняется, поведение и скорость обычного поиска не меняются.
                if total == 0 {
                    // Строгий запрос нужен резервному как `SHOULD`-клауза для
                    // ранга; собираем его заново — это холодная ветка, и
                    // повторный разбор запроса здесь дешевле, чем клон запроса
                    // на каждом обычном поиске.
                    let strict = self.build_full_text_query(query)?;
                    if let Some(strict) = strict {
                        if let Some(fallback) = self.build_fuzzy_fallback_query(query, &*strict)? {
                            // Счётчик только в тестовой сборке: по нему тесты
                            // отличают «резервный запрос не запускался» от
                            // «запускался, но ничего не нашёл».
                            #[cfg(test)]
                            self.fallback_searches
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                            // Счётчики и фасеты обязаны прийти от резервного
                            // запроса, иначе `matched`/`filtered` были бы
                            // нулевыми при непустом списке книг. Накопитель
                            // начинаем заново, чтобы не смешать строгие нули с
                            // приблизительной выдачей.
                            acc = Facets::new(keep, show_short, cover, mode);
                            // Фильтры у резервного запроса те же самые.
                            let fallback_filters = self.filter_clauses(author, genre, cover);
                            let fallback_total = self.collect_page(
                                &mut acc,
                                &searcher,
                                fallback,
                                fallback_filters,
                                keep,
                            )?;
                            // Пометка «приблизительные» — только если резервный
                            // запрос действительно что-то показал.
                            approximate = fallback_total > 0;
                        }
                    }
                }
            }
        }

        // `into_sorted_vec` отдаёт кучу по возрастанию `Ord`: при пустом
        // запросе это алфавит, при непустом — убывание score, то есть лучшая
        // книга первой. Страница вырезается уже из готового порядка.
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
            approximate,
        })
    }

    /// Сколько раз реально выполнялся резервный (fuzzy) запрос.
    ///
    /// Есть только в тестовой сборке (см. поле `fallback_searches`): по нему
    /// тест каскада проверяет, что при удачном строгом поиске резервный запрос
    /// не выполнялся вовсе.
    #[cfg(test)]
    pub fn fallback_search_count(&self) -> u64 {
        self.fallback_searches
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Один проход выдачи: фасеты, `matched`, страница и честный `filtered`.
    ///
    /// Общий и для строгого, и для резервного запроса: счётчики приблизительной
    /// выдачи обязаны считаться ровно так же, как точной, иначе они разъедутся.
    /// Запрос забирается по значению (его оборачивает [`Self::with_filters`]),
    /// поэтому на горячем пути лишних клонов нет. Возвращает `filtered` — число
    /// книг, прошедших фильтры.
    fn collect_page(
        &self,
        acc: &mut Facets,
        searcher: &Searcher,
        text_query: Box<dyn Query>,
        filter_clauses: Vec<(Occur, Box<dyn Query>)>,
        keep: usize,
    ) -> Result<usize> {
        // Проход 1 — фасеты и `matched` по выдаче без фильтров. Он ограничен
        // [`SEARCH_FETCH_CAP`], как и раньше: фасетам не нужна вся библиотека,
        // список всё равно обрезается.
        let facet_hits = self.hit_refs(searcher, &*text_query, SEARCH_FETCH_CAP)?;
        for (id, _score, addr) in facet_hits {
            match self.books.get(&id) {
                Some(book) => acc.count(book),
                // Индекс и метаданные разошлись — книга всё равно должна
                // попасть в фасеты и счётчик.
                None => {
                    let book = self.book_from_index(searcher, addr, id)?;
                    acc.count(&book);
                }
            }
        }

        // Проход 2 — страница и честный `filtered`: фильтры уже в запросе,
        // поэтому Tantivy не может «не дотянуться» до редкой книги. `Count`
        // считает все совпадения в том же проходе, так что число страниц не
        // зависит от размера страницы.
        //
        // Оговорка: `Count` включает и спрятанные статьи/заметки — `hidden`
        // это пост-фильтр (он зависит от редактируемых
        // `keep_authors.txt`/`hide_authors.txt`, поэтому в индекс его не
        // вынести), и вычесть его из `Count`, не обойдя библиотеку, нельзя. Это
        // цена за необрезанный `filtered`; на пустом запросе счётчик точный,
        // потому что там обходятся все книги.
        let filtered_query = self.with_filters(text_query, filter_clauses);
        let (page_hits, total) = self.page_hits(searcher, &*filtered_query, keep)?;
        for (id, score, addr) in page_hits {
            if let Some(book) = self.books.get(&id) {
                acc.pick(book, score);
            } else {
                let book = self.book_from_index(searcher, addr, id)?;
                acc.pick(&book, score);
            }
        }
        acc.filtered = total;
        Ok(total)
    }

    /// Клаузы фильтров для запроса.
    ///
    /// В Tantivy нет `Occur::Filter` (в отличие от Lucene), поэтому каждая
    /// клауза — `MUST` поверх `ConstScoreQuery(_, 0.0)`: она обязана совпасть,
    /// но не должна влиять на score. Ноль дешевле `BoostQuery(_, 0.0)`: для
    /// `ConstScoreQuery` BM25-вес вообще не строится.
    ///
    /// Автор — только `author_key` и только через [`normalize_key`], иначе
    /// сравнение снова станет побайтовым. Жанр — multi-value `genre` с той же
    /// нормализацией. Обложка — `has_cover` (1/0). Пустой нормализованный
    /// ключ (автор из одних знаков) даёт терм, которого в индексе нет, — то
    /// есть фильтр честно не пропускает ничего.
    fn filter_clauses(
        &self,
        author: Option<&str>,
        genre: Option<&str>,
        cover: CoverFilter,
    ) -> Vec<(Occur, Box<dyn Query>)> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        if let Some(author) = author {
            clauses.push(filter_clause(Term::from_field_text(
                self.f_author_key,
                &normalize_key(author),
            )));
        }
        if let Some(genre) = genre {
            clauses.push(filter_clause(Term::from_field_text(
                self.f_genre,
                &normalize_key(genre),
            )));
        }
        match cover {
            CoverFilter::All => {}
            CoverFilter::WithCover => {
                clauses.push(filter_clause(Term::from_field_u64(self.f_has_cover, 1)));
            }
            CoverFilter::WithoutCover => {
                clauses.push(filter_clause(Term::from_field_u64(self.f_has_cover, 0)));
            }
        }
        clauses
    }

    /// Добавить клаузы фильтров к текстовому запросу.
    ///
    /// Без фильтров возвращает запрос как есть: лишний `BooleanQuery` вокруг
    /// него только замедлил бы поиск.
    fn with_filters(
        &self,
        text_query: Box<dyn Query>,
        filter_clauses: Vec<(Occur, Box<dyn Query>)>,
    ) -> Box<dyn Query> {
        if filter_clauses.is_empty() {
            return text_query;
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> =
            Vec::with_capacity(filter_clauses.len() + 1);
        clauses.push((Occur::Must, text_query));
        clauses.extend(filter_clauses);
        Box::new(BooleanQuery::new(clauses))
    }

    /// `id` документа по адресу в индексе.
    fn doc_id(&self, searcher: &Searcher, addr: DocAddress) -> Result<Option<u64>> {
        let doc: TantivyDocument = searcher.doc(addr)?;
        Ok(doc.get_first(self.f_id).and_then(|v| v.as_u64()))
    }

    /// Верхушка выдачи `(id, score, адрес)` в порядке релевантности.
    ///
    /// Книги здесь не клонируются: `id` берём из индекса, а метаданные — из
    /// памяти по `id`. Раньше на каждый запрос клонировались десятки тысяч
    /// книг вместе с аннотациями и оглавлениями.
    fn hit_refs(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        cap: usize,
    ) -> Result<Vec<(u64, Score, DocAddress)>> {
        let cap = cap.max(1);
        let hits = searcher
            .search(query, &TopDocs::with_limit(cap).order_by_score())
            .context("ошибка поиска")?;
        let mut out = Vec::with_capacity(hits.len());
        for (score, addr) in hits {
            if let Some(id) = self.doc_id(searcher, addr)? {
                out.push((id, score, addr));
            }
        }
        Ok(out)
    }

    /// Верхушка выдачи и **честное** число совпадений за один проход.
    ///
    /// `Count` в паре с `TopDocs` считает все документы, подошедшие под
    /// запрос, а не только попавшие на страницу: по этому числу листается
    /// выдача. Раньше оно считалось по обрезанной верхушке
    /// [`SEARCH_FETCH_CAP`] и дальше неё было неполным.
    fn page_hits(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        keep: usize,
    ) -> Result<(Vec<(u64, Score, DocAddress)>, usize)> {
        let keep = keep.max(1);
        let (hits, total) = searcher
            .search(query, &(TopDocs::with_limit(keep).order_by_score(), Count))
            .context("ошибка поиска")?;
        let mut out = Vec::with_capacity(hits.len());
        for (score, addr) in hits {
            if let Some(id) = self.doc_id(searcher, addr)? {
                out.push((id, score, addr));
            }
        }
        Ok((out, total))
    }

    /// `id` всех книг, подошедших под запрос (обычно это фильтр без текста).
    ///
    /// Нужно ветке пустого запроса: страница там собирается обходом
    /// метаданных (алфавит и точные счётчики), а сам отбор делает Tantivy.
    /// Возвращаем **отсортированный** вектор, а не `HashSet`: на фильтре «без
    /// обложки» под запрос подходит почти вся библиотека, и хеш-таблица на
    /// сотни тысяч `u64` заметно дороже отсортированного вектора, по которому
    /// мы ищем двоичным поиском. `id` для этого объявлен `FAST`.
    fn matching_ids(&self, query: &dyn Query) -> Result<Vec<u64>> {
        let searcher = self.reader.searcher();
        let total = searcher
            .search(query, &Count)
            .context("ошибка подсчёта по фильтру")?;
        if total == 0 {
            return Ok(Vec::new());
        }
        let schema = self.index.schema();
        let id_field = schema.get_field_name(self.f_id);
        let hits = searcher
            .search(
                query,
                &TopDocs::with_limit(total).order_by_u64_field(id_field, Order::Asc),
            )
            .context("ошибка отбора по фильтру")?;
        Ok(hits.into_iter().filter_map(|(id, _addr)| id).collect())
    }

    /// Собрать метаданные книги из индекса, если её нет в памяти.
    ///
    /// Индекс и `data/metadata` могут разойтись — метаданные правили руками
    /// или файл потерялся. Раньше такая книга всё равно находилась: из
    /// индекса берём название, автора, архив и путь, а жанры и подробности
    /// остаются пустыми.
    fn book_from_index(
        &self,
        searcher: &Searcher,
        addr: DocAddress,
        id: u64,
    ) -> Result<BookMetadata> {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let text = |f: Field| {
            doc.get_first(f)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        Ok(BookMetadata {
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
            // В индексе счёта слов нет: считаем такую книгу книгой.
            words: None,
            hidden: false,
        })
    }

    /// Разобрать запрос на термы тем же анализатором, что и поле `title`.
    ///
    /// Поле `title` индексируется анализатором `ru_stem` (разбивка на слова +
    /// lower-case + русский стеммер), поэтому термы запроса обязаны пройти
    /// через него же: тогда «преступления» превратится в ту же основу, что и
    /// «Преступление» в индексе, и терм найдётся. Брать анализатор по имени
    /// строкой нельзя — только [`Index::tokenizer_for_field`], иначе поле и
    /// запрос разъедутся при любой смене анализатора.
    ///
    /// Возвращаются пары [`QueryTerm`]: исходное слово (для стоп-списка) и его
    /// стем (для клауз). Стеммер не меняет число токенов, поэтому индексы в
    /// обеих половинах совпадают.
    fn query_terms(&self, query: &str) -> Result<Vec<QueryTerm>> {
        // Стемы — тем же анализатором, что и поле: только так они совпадут с
        // термами индекса.
        let mut analyzer = self
            .index
            .tokenizer_for_field(self.f_title)
            .context("не удалось получить анализатор поля title")?;
        let mut stream = analyzer.token_stream(query);
        let mut stems = Vec::new();
        // Лишние термы отбрасываем: см. [`MAX_QUERY_TERMS`].
        while let Some(token) = stream.next() {
            if stems.len() == MAX_QUERY_TERMS {
                break;
            }
            stems.push(token.text.clone());
        }

        // Исходные слова — тот же `SimpleTokenizer` + `LowerCaser`, но без
        // стеммера. Стеммер не добавляет и не удаляет токены, поэтому списки
        // выравниваются один к одному; стоп-слова проверяются по `raw`.
        let mut lower = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        let mut raw_stream = lower.token_stream(query);
        let mut raws = Vec::with_capacity(stems.len());
        while let Some(token) = raw_stream.next() {
            if raws.len() == stems.len() {
                break;
            }
            raws.push(token.text.clone());
        }
        debug_assert_eq!(raws.len(), stems.len(), "стеммер изменил число токенов");

        Ok(stems
            .into_iter()
            .enumerate()
            .map(|(index, stem)| QueryTerm {
                // Стеммер в норме не трогает число токенов; если всё-таки
                // разошлось, исходным словом считаем сам стем — терять терм
                // нельзя.
                raw: raws.get(index).cloned().unwrap_or_else(|| stem.clone()),
                stem,
            })
            .collect())
    }

    /// Собрать полнотекстовый запрос вручную.
    ///
    /// `QueryParser` здесь сознательно не используется: он по умолчанию
    /// объединяет термы через `OR`, из-за чего «мастер и маргарита» находило
    /// любую книгу со словом «и». Запрос собирается так:
    ///
    /// * на верхнем уровне — `MUST`-группа на **каждый** терм запроса, то
    ///   есть логическое `AND`: книге нужно содержать все термы;
    /// * внутри группы — `SHOULD` из точного терма по `title`, точного терма
    ///   по `author` и (для длинных термов) fuzzy-терма по `author`, то есть
    ///   `OR` между полями одного терма;
    /// * дополнительный `SHOULD` верхнего уровня — точная фраза из всех
    ///   термов в исходном порядке по `title` с повышенным весом;
    /// * ещё два `SHOULD` верхнего уровня — точное совпадение всего запроса с
    ///   `title_key` и `author_key`. Это `SHOULD`, а не `MUST`: точное
    ///   название поднимает книгу в ранге, но запрос «Мастер и маргарита
    ///   краткий разбор» не должен прятать сам роман, у которого ключ короче.
    ///
    /// Потермная `MUST`-структура принципиальна: плоский `SHOULD` вернул бы
    /// поведение `OR` и находил бы всё, где есть слово «и».
    ///
    /// Жанры (`f_genres`) в запросе не участвуют: жанр — это фильтр, а не
    /// текст, и он применяется отдельно по метаданным. `RegexQuery` и
    /// `WildcardQuery` не используются нигде. Fuzzy по `title` в этом, строгом,
    /// запросе тоже нет: он дорог и шумит, поэтому включается только в
    /// резервном каскаде (см. [`Self::build_fuzzy_fallback_query`]).
    ///
    /// Возвращает `None`, если в запросе не осталось ни одного терма
    /// (пустая строка или одни разделители).
    fn build_full_text_query(&self, query: &str) -> Result<Option<Box<dyn Query>>> {
        let terms = self.query_terms(query)?;
        if terms.is_empty() {
            return Ok(None);
        }

        // У `title` есть и частоты, и позиции (нужны фразе), у `author` —
        // только частоты, у ключей — вообще ничего. Запросить у поля больше,
        // чем в нём записано, Tantivy не даст: на построении веса он вернёт
        // ошибку, поэтому `IndexRecordOption` каждой клаузы обязан совпадать
        // с опциями поля в [`build_schema`].
        let key_record = IndexRecordOption::Basic;

        // Веса не заменяют BM25: Tantivy считает score сам и нормирует вклад
        // терма по длине поля (fieldnorms), так что короткое точное название
        // выигрывает у длинного уже без boost. Константы задают только
        // взаимный приоритет клауз: ключ названия (multi) > ключ автора >
        // фраза > ключ названия (single) > терм в title > терм в author >
        // fuzzy.
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(terms.len() + 3);

        // Строгий запрос: fuzzy по `title` не включаем — им занимается только
        // резервный каскад (см. [`Self::build_fuzzy_fallback_query`]).
        for term in &terms {
            clauses.push(self.per_term_query(&term.stem, false));
        }

        // Точная фраза целиком — только если термов хотя бы два: для одного
        // терма она совпала бы с обычным термом и лишь задвоила бы вклад.
        if terms.len() >= 2 {
            let phrase: Vec<Term> = terms
                .iter()
                .map(|term| Term::from_field_text(self.f_title, &term.stem))
                .collect();
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(PhraseQuery::new(phrase)),
                    BOOST_TITLE_PHRASE,
                )),
            ));
        }

        // Точные ключи: весь запрос целиком, нормализованный той же функцией,
        // что и при индексации. Вес `title_key` зависит от длины запроса — см.
        // [`BOOST_TITLE_KEY_SINGLE`].
        let query_key = normalize_key(query);
        if !query_key.is_empty() {
            let title_key_boost = if terms.len() >= 2 {
                BOOST_TITLE_KEY_MULTI
            } else {
                BOOST_TITLE_KEY_SINGLE
            };
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.f_title_key, &query_key),
                        key_record,
                    )),
                    title_key_boost,
                )),
            ));
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.f_author_key, &query_key),
                        key_record,
                    )),
                    BOOST_AUTHOR_KEY,
                )),
            ));
        }

        Ok(Some(Box::new(BooleanQuery::new(clauses))))
    }

    /// `MUST`-группа одного терма: `OR` точных совпадений по `title` и
    /// `author` плюс необязательные fuzzy-клаузы **внутри того же терма**.
    ///
    /// Группа обязана совпасть (`Occur::Must` у вызывающего), но внутри
    /// достаточно любой клаузы. Поэтому fuzzy не может «подменить» терм: он
    /// лишь даёт ещё один способ ему совпасть.
    ///
    /// `title_fuzzy` включает fuzzy по **стеммированному** `title` — это
    /// единственное отличие резервного запроса от строгого. Fuzzy по `author`
    /// (порог [`FUZZY_MIN_TERM_LEN`], расстояние [`FUZZY_DISTANCE`]) есть в
    /// обоих запросах и здесь не меняется.
    fn per_term_query(&self, stem: &str, title_fuzzy: bool) -> (Occur, Box<dyn Query>) {
        // У `title` есть и частоты, и позиции, у `author` — только частоты:
        // `IndexRecordOption` обязан совпадать с опциями поля.
        let mut per_term: Vec<(Occur, Box<dyn Query>)> =
            Vec::with_capacity(if title_fuzzy { 4 } else { 3 });
        per_term.push((
            Occur::Should,
            Box::new(BoostQuery::new(
                Box::new(TermQuery::new(
                    Term::from_field_text(self.f_title, stem),
                    IndexRecordOption::WithFreqsAndPositions,
                )),
                BOOST_TITLE_TERM,
            )),
        ));
        per_term.push((
            Occur::Should,
            Box::new(BoostQuery::new(
                Box::new(TermQuery::new(
                    Term::from_field_text(self.f_author, stem),
                    IndexRecordOption::WithFreqs,
                )),
                BOOST_AUTHOR_TERM,
            )),
        ));
        // Fuzzy по автору — прежнее правило, не трогаем.
        if stem.chars().count() >= FUZZY_MIN_TERM_LEN {
            per_term.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(FuzzyTermQuery::new(
                        Term::from_field_text(self.f_author, stem),
                        FUZZY_DISTANCE,
                        true,
                    )),
                    BOOST_AUTHOR_FUZZY,
                )),
            ));
        }
        // Fuzzy по названию — только резервный запрос и только по стему.
        if title_fuzzy {
            per_term.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(FuzzyTermQuery::new(
                        Term::from_field_text(self.f_title, stem),
                        fuzzy_title_distance(stem),
                        true,
                    )),
                    BOOST_TITLE_FUZZY,
                )),
            ));
        }
        (Occur::Must, Box::new(BooleanQuery::new(per_term)))
    }

    /// Собрать **резервный** запрос: тот же строгий поиск, но с одной
    /// разрешённой опечаткой в одном слове названия.
    ///
    /// Устройство (спецификация — §6 «SEARCH_STEMMED», каскад):
    ///   * на каждый терм, которому разрешён fuzzy по `title`
    ///     (см. [`title_fuzzy_terms`]), — свой вариант запроса; в варианте
    ///     **этот один** терм получает дополнительную fuzzy-клаузу, а
    ///     остальные обязаны совпасть строго, как в строгом запросе;
    ///   * все варианты — `SHOULD` в общем `BooleanQuery`: достаточно, чтобы
    ///     сработал любой (то есть одну опечатку исправляем в любом слове);
    ///   * туда же `SHOULD`-клаузой идёт сам строгий запрос с весом
    ///     [`BOOST_FALLBACK_STRICT`] — не для отбора, а для ранга: точные
    ///     совпадения обязаны быть выше приблизительных.
    ///
    /// Термам, которым fuzzy не разрешён (короткие стемы и стоп-слова),
    /// отдельного варианта не строится: без fuzzy-клаузы такой «вариант» был бы
    /// байт в байт равен строгому запросу, который и так уже в списке. В любом
    /// чужом варианте эти термы по-прежнему обязаны совпасть строго.
    ///
    /// Возвращает `None`, если ни одному терму fuzzy не разрешён (запрос из
    /// коротких слов или одних стоп-слов): тогда резервный запрос не нужен и
    /// не выполняется. `RegexQuery` и `WildcardQuery` здесь не используются —
    /// их в проекте нет вообще.
    fn build_fuzzy_fallback_query(
        &self,
        query: &str,
        strict: &dyn Query,
    ) -> Result<Option<Box<dyn Query>>> {
        let terms = self.query_terms(query)?;
        let fuzzy = title_fuzzy_terms(&terms);
        if fuzzy.is_empty() {
            return Ok(None);
        }

        let mut variants: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(fuzzy.len() + 1);
        for &fixed in &fuzzy {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(terms.len());
            for (index, term) in terms.iter().enumerate() {
                clauses.push(self.per_term_query(&term.stem, index == fixed));
            }
            variants.push((Occur::Should, Box::new(BooleanQuery::new(clauses))));
        }
        variants.push((
            Occur::Should,
            Box::new(BoostQuery::new(strict.box_clone(), BOOST_FALLBACK_STRICT)),
        ));
        Ok(Some(Box::new(BooleanQuery::new(variants))))
    }

    /// Полнотекстовый поиск по названию и автору (жанр в запросе не участвует).
    ///
    /// Термы объединяются через `AND`, а внутри терма поля — через `OR`
    /// (см. [`Self::build_full_text_query`]). Результат — в порядке
    /// релевантности, как его отдаёт Tantivy, не длиннее `cap`, вместе со
    /// score каждой книги.
    ///
    /// Оставлено для тестов и отладки: боевой [`Self::search`] читает книги из
    /// памяти по `id` и клонирует только страницу. Фильтры сюда не входят —
    /// их добавляет [`Self::with_filters`].
    #[allow(dead_code)]
    fn full_text_hits(&self, query: &str, cap: usize) -> Result<Vec<(BookMetadata, Score)>> {
        let searcher = self.reader.searcher();
        // Пустой запрос или запрос из одних разделителей не превращаем в
        // пустой `BooleanQuery`: результатов у него всё равно нет.
        let Some(parsed) = self
            .build_full_text_query(query)
            .with_context(|| format!("не удалось разобрать запрос `{query}`"))?
        else {
            return Ok(Vec::new());
        };
        let hits = self.hit_refs(&searcher, &*parsed, cap)?;
        let mut out = Vec::with_capacity(hits.len());
        for (id, score, addr) in hits {
            let book = match self.books.get(&id) {
                Some(book) => book.clone(),
                // Индекс и метаданные разошлись — собираем из индекса,
                // чтобы книга всё равно нашлась. Score берём из выдачи
                // Tantivy: он нужен сортировке, как и для книг из метаданных.
                None => self.book_from_index(&searcher, addr, id)?,
            };
            out.push((book, score));
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

    /// Русский стеммер: падежная форма в запросе находит книгу.
    ///
    /// Без стеммера «преступления» и «Преступление» — разные термы, и главный
    /// запрос этапа не находил бы ничего.
    #[test]
    fn russian_stemmer_finds_inflected_forms() -> Result<()> {
        let dir = scratch("stemming");
        let books = vec![BookMetadata {
            id: 600,
            title: "Преступление и наказание".into(),
            author: "Фёдор Достоевский".into(),
            ..sample_book(600)
        }];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("600-600", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;

        let out = lib.search(
            "преступления и наказания",
            None,
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(out.matched, 1, "падежные формы должны находить книгу");
        assert_eq!(out.books[0].id, 600);

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Временная библиотека с одной книгой «Преступление и наказание».
    ///
    /// Общая заготовка тестов каскада: у всех сценариев одна и та же книга, и
    /// различается только запрос. Каталог надо удалить в конце теста.
    fn dostoevsky(name: &str) -> Result<(PathBuf, Library)> {
        let dir = scratch(name);
        let books = vec![BookMetadata {
            id: 900,
            title: "Преступление и наказание".into(),
            author: "Фёдор Достоевский".into(),
            ..sample_book(900)
        }];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("900-900", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;
        Ok((dir, lib))
    }

    /// Сценарий заказчика: опечатка в названии — «приступление и наказание».
    ///
    /// Строгий запрос не находит ничего (терма «приступлен» в индексе нет),
    /// поэтому включается каскад, и резервный запрос допускает **одну**
    /// опечатку в одном слове названия. Книга обязана найтись, а признаки
    /// `matched`/`filtered` — прийти из резервного запроса, а не остаться
    /// нулями строгого (это отдельная ошибка, которую легко не заметить).
    #[test]
    fn typo_in_title_finds_book_through_fallback() -> Result<()> {
        let (dir, lib) = dostoevsky("typo_fallback")?;
        // Проверяем, что дело именно в каскаде: до запроса он не выполнялся.
        assert_eq!(lib.fallback_search_count(), 0);

        let out = lib.search(
            "приступление и наказание",
            None,
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;

        assert_eq!(out.filtered, 1, "опечатка не должна мешать найти книгу");
        assert_eq!(out.matched, 1, "`matched` обязан прийти из резервного запроса");
        assert_eq!(out.books.len(), 1);
        assert_eq!(out.books[0].id, 900, "нашлась именно «Преступление и наказание»");
        assert!(out.approximate, "выдача приблизительная: в запросе опечатка");
        assert_eq!(
            lib.fallback_search_count(),
            1,
            "резервный запрос обязан выполниться ровно один раз"
        );

        // «и» — стоп-слово: fuzzy-варианта для него нет, поэтому вариантов
        // ровно два — по одному на знаменательное слово, и сам терм «и» в
        // каждом варианте обязан совпасть строго.
        let terms = lib.query_terms("приступление и наказание")?;
        assert_eq!(
            title_fuzzy_terms(&terms),
            vec![0, 2],
            "«и» не получает fuzzy, хотя и входит в запрос"
        );

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Запрос без опечаток идёт прежним строгим путём: каскад не запускается.
    ///
    /// Это главная гарантия того, что скорость и поведение обычного поиска не
    /// изменились: резервный запрос не строится и не выполняется вовсе
    /// (счётчик остаётся нулевым), а `approximate` ложно.
    #[test]
    fn exact_query_keeps_strict_path_and_skips_fallback() -> Result<()> {
        let (dir, lib) = dostoevsky("strict_path")?;

        let out = lib.search(
            "преступление и наказание",
            None,
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;

        assert_eq!(out.filtered, 1, "точный запрос находит книгу строго");
        assert_eq!(out.books.len(), 1);
        assert_eq!(out.books[0].id, 900);
        assert!(!out.approximate, "точное совпадение — не приблизительное");
        assert_eq!(
            lib.fallback_search_count(),
            0,
            "при непустом строгом результате резервный запрос не выполняется"
        );

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Две опечатки в разных словах схема «одна опечатка на запрос» не лечит.
    ///
    /// Каждый вариант резервного запроса исправляет ровно **один** терм, а
    /// остальные обязаны совпасть строго (это по-прежнему AND). Поэтому
    /// «приступление и наказанме» не находит книгу: вариант для первого слова
    /// спотыкается о строгое «наказанм», вариант для второго — о строгое
    /// «приступлен». Сначала убеждаемся, что каждая опечатка **по
    /// отдельности** исправляется, иначе тест ничего не доказывал бы.
    ///
    /// Это ожидаемое поведение, а не регрессия: разрешить опечатку в каждом
    /// терме — значит потерять AND и снова найти мусор по любому слову.
    #[test]
    fn two_typos_in_different_words_stay_unfixed() -> Result<()> {
        let (dir, lib) = dostoevsky("two_typos")?;

        let one_typo_first = lib.search(
            "приступление наказание",
            None,
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(one_typo_first.filtered, 1, "опечатка в первом слове исправляется");
        assert!(one_typo_first.approximate);

        let one_typo_second = lib.search(
            "преступление наказанме",
            None,
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(one_typo_second.filtered, 1, "опечатка во втором слове исправляется");
        assert!(one_typo_second.approximate);

        let both = lib.search(
            "приступление наказанме",
            None,
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(
            both.filtered, 0,
            "схема исправляет только одну опечатку за запрос"
        );
        assert!(both.books.is_empty());
        assert!(!both.approximate, "приблизительных совпадений тоже нет");

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Мусорный запрос по-прежнему даёт ноль и не паникует.
    ///
    /// Терм короче [`FUZZY_TITLE_MIN_TERM_LEN`], поэтому fuzzy-варианта нет и
    /// резервный запрос не строится вовсе: `approximate` ложно, счётчик не
    /// растёт.
    #[test]
    fn garbage_query_is_empty_and_not_approximate() -> Result<()> {
        let (dir, lib) = dostoevsky("garbage")?;

        let out = lib.search("ъъъъ", None, None, false, CoverFilter::All, 0, 10)?;

        assert_eq!(out.filtered, 0);
        assert_eq!(out.matched, 0);
        assert!(out.books.is_empty());
        assert!(!out.approximate);
        assert_eq!(
            lib.fallback_search_count(),
            0,
            "короткий терм не даёт fuzzy-варианта — резервный запрос не выполняется"
        );

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Стоп-слово не получает fuzzy-клаузу — даже если длиннее порога.
    ///
    /// Список стоп-слов проверяется по **исходному** слову, а длина — по стему.
    /// В тесте стем намеренно равен исходному слову и длине 5, чтобы исключение
    /// объяснялось только стоп-списком, а не порогом длины.
    #[test]
    fn stop_word_is_never_fuzzy() {
        assert!(is_stop_word("и"), "«и» — стоп-слово");
        assert!(is_stop_word("чтобы"), "длинное служебное слово — тоже стоп-слово");

        let terms = vec![
            QueryTerm {
                raw: "чтобы".into(),
                stem: "чтобы".into(),
            },
            QueryTerm {
                raw: "приступление".into(),
                stem: "приступлен".into(),
            },
        ];
        assert_eq!(
            title_fuzzy_terms(&terms),
            vec![1],
            "fuzzy достаётся только знаменательному слову, не стоп-слову"
        );

        // Короткое стоп-слово отсекается независимо от остальных правил.
        let terms = vec![QueryTerm {
            raw: "и".into(),
            stem: "и".into(),
        }];
        assert!(title_fuzzy_terms(&terms).is_empty());
    }

    /// Замер резервного запроса на временном индексе.
    ///
    /// Это не проверка, а измерение, поэтому тест помечен `#[ignore]` (он ещё и
    /// строит сто тысяч книг). Запуск:
    ///
    /// ```text
    /// cargo test --release benchmark_fallback_query -- --ignored --nocapture
    /// ```
    ///
    /// Печатает цену строгого и резервного запроса по отдельности. Цифры для
    /// [`FUZZY_TITLE_DISTANCE`] = 1 попали в отчёт; если выставить
    /// [`FUZZY_TITLE_ALLOW_DISTANCE_2`] = true, тот же замер покажет цену
    /// расстояния 2.
    #[test]
    #[ignore = "замер производительности, запускать вручную"]
    fn benchmark_fallback_query() -> Result<()> {
        use std::time::Instant;

        /// Сколько книг в синтетическом индексе.
        const BOOKS: u64 = 100_000;
        /// Размер пачки для `index_books`: как [`INDEX_CHUNK`], только меньше —
        /// замеру хватает.
        const BATCH: usize = 10_000;
        /// Слоги для синтетических слов. Дают большой (десятки тысяч стемов)
        /// словарь названий — иначе fuzzy искать почти не по чему и замер
        /// получился бы нечестно быстрым.
        const SYLLABLES: &[&str] = &[
            "пре", "сту", "пле", "ние", "на", "ка", "за", "ма", "стер", "мар", "га", "рит",
            "до", "сто", "ев", "ский", "вой", "мир", "том", "дом", "сад", "лес", "ре", "мо",
            "го", "ра", "зем", "ля", "не", "бо", "солн", "це", "лу", "звез", "да", "ве", "тер",
            "дождь", "снег", "огонь", "вода", "ти", "ши", "кро", "ва", "тень", "свет", "путь",
        ];

        /// Детерминированное «слово» из трёх слогов: у разных `seed` — разные
        /// слова, словарь получается большим и стабильным между запусками.
        fn synthetic_word(mut seed: u64) -> String {
            let mut word = String::new();
            for _ in 0..3 {
                word.push_str(SYLLABLES[(seed % SYLLABLES.len() as u64) as usize]);
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
            }
            word
        }

        let dir = scratch("bench_fallback");
        // Слова книги-образца запоминаем при генерации: метаданные в этом
        // замере не пишутся, и взять название из `lib.books` нельзя.
        let mut probe: Option<(String, String, String)> = None;
        {
            let lib = Library::open(&dir)?;
            let mut writer = lib.writer()?;
            let mut batch = Vec::with_capacity(BATCH);
            for id in 0..BOOKS {
                let words = (
                    synthetic_word(id),
                    synthetic_word(id + 1_000_000),
                    synthetic_word(id + 2_000_000),
                );
                if id == 12_345 {
                    probe = Some(words.clone());
                }
                batch.push(BookMetadata {
                    id,
                    title: format!("{} {} {}", words.0, words.1, words.2),
                    author: format!("Автор {}", id % 10_000),
                    ..sample_book(id)
                });
                if batch.len() == BATCH {
                    lib.index_books(&mut writer, &batch)?;
                    batch.clear();
                }
            }
            if !batch.is_empty() {
                lib.index_books(&mut writer, &batch)?;
            }
        }
        let lib = Library::open(&dir)?;

        // Точный запрос по конкретной книге и он же с опечаткой в первом слове.
        let (word0, word1, word2) = probe.expect("книга-образец сгенерирована");
        let exact = format!("{word0} {word1} {word2}");
        let mut typo_word = word0.clone();
        typo_word.insert(2, 'щ');
        let typo = format!("{typo_word} {word1} {word2}");

        let searcher = lib.reader.searcher();
        let exact_query = lib.build_full_text_query(&exact)?.expect("запрос не пуст");
        let typo_strict = lib.build_full_text_query(&typo)?.expect("запрос не пуст");
        let fallback = lib
            .build_fuzzy_fallback_query(&typo, &*typo_strict)?
            .expect("в слове с опечаткой больше пяти букв — вариант есть");

        let exact_hits = searcher.search(&*exact_query, &Count)?;
        let typo_strict_hits = searcher.search(&*typo_strict, &Count)?;
        let fallback_hits = searcher.search(&*fallback, &Count)?;

        // Разогрев, чтобы не мерить первый прогон (ленивые структуры Tantivy).
        let _ = lib.hit_refs(&searcher, &*exact_query, 100)?;
        let _ = lib.hit_refs(&searcher, &*fallback, 100)?;

        const RUNS: usize = 20;
        let t = Instant::now();
        for _ in 0..RUNS {
            let _ = lib.hit_refs(&searcher, &*exact_query, 100)?;
        }
        let exact_us = t.elapsed().as_secs_f64() * 1e6 / RUNS as f64;

        let t = Instant::now();
        for _ in 0..RUNS {
            let _ = lib.hit_refs(&searcher, &*fallback, 100)?;
        }
        let fallback_ms = t.elapsed().as_secs_f64() * 1e3 / RUNS as f64;

        // Полный публичный поиск с каскадом — один раз, как его видит GUI.
        let t = Instant::now();
        let out = lib.search(&typo, None, None, false, CoverFilter::All, 0, 100)?;
        let cascade_ms = t.elapsed().as_secs_f64() * 1e3;

        println!("--- замер резервного запроса ---");
        println!("книг в индексе: {BOOKS}");
        println!("термов в запросе: 3 (fuzzy-вариантов: 3)");
        println!("строгий запрос:      {exact_us:.1} мкс, попаданий {exact_hits}");
        println!("строгий с опечаткой: попаданий {typo_strict_hits} (должно быть 0)");
        println!("резервный запрос:    {fallback_ms:.2} мс, попаданий {fallback_hits}");
        println!("полный поиск с каскадом: {cascade_ms:.2} мс, книг {}", out.books.len());
        assert!(out.approximate, "каскад обязан был сработать");
        assert_eq!(typo_strict_hits, 0, "опечатка не должна совпасть строго");

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// «ё» и «е» — один и тот же ключ, а регистр и пунктуация ключ не меняют.
    ///
    /// Пользователь печатает «Королев», а на обложке «Королёв»: если ключи
    /// разойдутся, точное совпадение молча перестанет срабатывать.
    #[test]
    fn key_normalization_folds_case_punctuation_and_yo() {
        assert_eq!(normalize_key("Королёв"), normalize_key("Королев"));
        assert_eq!(normalize_key("Королёв"), "королев");
        assert_eq!(normalize_key("Мастер и Маргарита"), "мастер и маргарита");
        assert_eq!(normalize_key("мастер и маргарита"), "мастер и маргарита");
        // Знаки препинания и лишние пробелы сворачиваются в один пробел.
        assert_eq!(
            normalize_key("  Мастер,  и -- Маргарита!  "),
            "мастер и маргарита"
        );
        // Одни разделители дают пустой ключ, а не строку из пробелов.
        assert_eq!(normalize_key("  ...  "), "");
    }

    /// «Королев» в запросе находит книгу «Королёв».
    ///
    /// Стеммер и `normalize_key` сворачивают «ё» к «е» по отдельности; этот
    /// тест проверяет, что совпадение доходит до публичной выдачи.
    #[test]
    fn yo_in_query_matches_yo_in_title() -> Result<()> {
        let dir = scratch("yo_fold");
        let books = vec![BookMetadata {
            id: 610,
            title: "Королёв".into(),
            author: "Иван Королёв".into(),
            ..sample_book(610)
        }];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("610-610", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;

        let out = lib.search("Королев", None, None, false, CoverFilter::All, 0, 10)?;
        assert_eq!(out.matched, 1, "«Королев» должно находить «Королёв»");
        assert_eq!(out.books[0].id, 610);

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Словарь авторов собирается из индекса целиком, а не из текущей выдачи.
    ///
    /// Источник — термы `author_key` и их `doc_freq`, поэтому проверяется
    /// именно проиндексированный временный каталог, а не метаданные: число
    /// книг должно прийти из индекса, а написание имени — из метаданных.
    #[test]
    fn author_dictionary_counts_all_authors_from_the_index() -> Result<()> {
        let dir = scratch("author_dictionary");
        let bulgakov = |id: u64| BookMetadata {
            id,
            title: "Мастер и Маргарита".into(),
            author: "Михаил Булгаков".into(),
            ..sample_book(id)
        };
        let books = vec![
            bulgakov(710),
            bulgakov(711),
            bulgakov(712),
            BookMetadata {
                id: 713,
                title: "Преступление и наказание".into(),
                author: "Фёдор Достоевский".into(),
                ..sample_book(713)
            },
            BookMetadata {
                id: 714,
                title: "Королёв".into(),
                author: "Иван Королёв".into(),
                ..sample_book(714)
            },
            BookMetadata {
                id: 715,
                title: "Королёв-2".into(),
                author: "Иван Королёв".into(),
                ..sample_book(715)
            },
        ];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("710-715", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;
        let dict = lib.author_dictionary()?;

        // Имя — в написании из метаданных, число книг — из `doc_freq`.
        let count = |name: &str| dict.iter().find(|(n, _)| n == name).map(|(_, c)| *c);
        assert_eq!(count("Михаил Булгаков"), Some(3));
        assert_eq!(count("Фёдор Достоевский"), Some(1));
        assert_eq!(count("Иван Королёв"), Some(2));
        assert_eq!(dict.len(), 3, "в словаре ровно авторы индекса: {dict:?}");

        // Список отсортирован по имени.
        let names: Vec<&str> = dict.iter().map(|(name, _)| name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "словарь отсортирован по имени");

        // Подсказки по словарю: фамилия, имя и «ё»/«е» в любом регистре.
        let dictionary = crate::author_suggest::AuthorDictionary::new(dict);
        let found = dictionary.suggestions("булг", crate::author_suggest::MAX_SUGGESTIONS);
        assert_eq!(found, vec![("Михаил Булгаков".to_string(), 3)]);
        let found = dictionary.suggestions("мих", crate::author_suggest::MAX_SUGGESTIONS);
        assert_eq!(found, vec![("Михаил Булгаков".to_string(), 3)]);
        let found = dictionary.suggestions("КОРОЛЕВ", crate::author_suggest::MAX_SUGGESTIONS);
        assert_eq!(found, vec![("Иван Королёв".to_string(), 2)]);

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Устаревшая схема индекса распознаётся, а индекс собирается заново.
    ///
    /// Индекс, собранный до появления `title_key`/`author_key`, открывать
    /// нельзя: полей, которые ждёт запрос, в нём нет, и поиск молча ничего бы
    /// не находил. `Library::open` обязан заметить расхождение, убрать старый
    /// каталог в `index.bak-*` (не удалять!) и наполнить новый из метаданных.
    #[test]
    fn stale_schema_is_rebuilt_from_scratch() -> Result<()> {
        let dir = scratch("stale_schema");
        fs::create_dir_all(dir.join("index"))?;

        // Индекс старой схемы: без стеммера и без полей-ключей.
        let mut builder = Schema::builder();
        let f_id = builder.add_u64_field("id", INDEXED | STORED);
        let f_title = builder.add_text_field("title", TEXT | STORED);
        let f_author = builder.add_text_field("author", TEXT | STORED);
        let f_genres = builder.add_text_field("genres", TEXT | STORED);
        let f_archive = builder.add_text_field("archive", STRING | STORED);
        let f_path = builder.add_text_field("internal_path", STRING | STORED);
        let old_schema = builder.build();
        {
            let old = Index::create_in_dir(dir.join("index"), old_schema)?;
            let mut writer = old.writer::<TantivyDocument>(INDEX_MEMORY_BUDGET)?;
            writer.add_document(doc!(
                f_id => 700u64,
                f_title => "Старое название",
                f_author => "Старый автор",
                f_genres => "",
                f_archive => "700-700.arx",
                f_path => "books/700.epub",
            ))?;
            writer.commit()?;
            writer.wait_merging_threads()?;
        }
        // Метаданные пишем вручную: `Library::open` здесь вызвать нельзя — он
        // бы сразу пересобрал индекс, и проверять было бы нечего.
        let meta_dir = dir.join("metadata");
        fs::create_dir_all(&meta_dir)?;
        let mut line = serde_json::to_string(&sample_book(700))?;
        line.push('\n');
        fs::write(meta_dir.join("700-700.jsonl"), line)?;

        let lib = Library::open(&dir)?;
        assert!(lib.schema_upgraded, "расхождение схемы должно быть замечено");
        assert!(
            lib.index.schema().get_field("title_key").is_ok(),
            "в новом индексе должен быть title_key"
        );
        // Старый каталог переехал, а не исчез.
        let backup = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with("index.bak-"));
        assert!(backup.is_some(), "старый индекс должен остаться в index.bak-*");
        // Индекс наполнен из метаданных: книга находится сразу после открытия.
        let out = lib.search("Книга 700", None, None, false, CoverFilter::All, 0, 10)?;
        assert_eq!(out.matched, 1, "пересобранный индекс должен находить книгу");
        assert_eq!(out.books[0].id, 700);
        drop(lib);

        // Повторное открытие уже ничего не пересобирает.
        let again = Library::open(&dir)?;
        assert!(!again.schema_upgraded, "схема уже актуальна");
        let backups = fs::read_dir(&dir)?
            .filter(|e| {
                e.as_ref()
                    .map(|e| e.file_name().to_string_lossy().starts_with("index.bak-"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(backups, 1, "второй пересборки быть не должно");

        let _ = fs::remove_dir_all(&dir);
        Ok(())
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
            size: 500_000,
            // Обычная книга: 50 000 слов и полмегабайта, прятать нечего.
            words: Some(50_000),
            hidden: false,
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
    /// Короткая запись прячется, а короткая вещь настоящего автора — нет.
    ///
    /// Признак — [`MIN_BOOK_WORDS`]: 6 000 слов это ≈24 страницы. Ниже порога
    /// остаются только стихи, малые формы и сочинения автора, у которого есть
    /// настоящая книга: его рассказы — не обрывки.
    #[test]
    fn short_items_are_hidden_from_search() -> Result<()> {
        let dir = scratch("short");
        // Три книги одного автора по 50 000 слов — он «настоящий».
        let mut books: Vec<BookMetadata> = (1..=3u64).map(sample_book).collect();
        // Заметка на 500 слов от автора без единой книги — прячется.
        books.push(BookMetadata {
            id: 100,
            author: "Никто".into(),
            words: Some(500),
            ..sample_book(100)
        });
        // Рассказ на 4 500 слов (16+ страниц) того же автора — остаётся.
        books.push(BookMetadata {
            id: 101,
            words: Some(4_500),
            ..sample_book(101)
        });
        // А его же вещь на 900 слов — уже брошюра, прячется.
        books.push(BookMetadata {
            id: 104,
            words: Some(900),
            ..sample_book(104)
        });
        // 10 000 слов (40 страниц) у автора без длинных книг — тоже мало:
        // книга начинается с [`MIN_BOOK_WORDS`].
        books.push(BookMetadata {
            id: 105,
            author: "Никто Второй".into(),
            words: Some(10_000),
            ..sample_book(105)
        });
        // Стихи короткие по природе — остаются в любом случае.
        books.push(BookMetadata {
            id: 102,
            author: "Поэт".into(),
            genre_codes: vec!["poetry".into()],
            words: Some(300),
            ..sample_book(102)
        });
        // Запись старого импорта без счёта слов — считаем книгой.
        books.push(BookMetadata {
            id: 103,
            author: "Старый".into(),
            words: None,
            ..sample_book(103)
        });
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("1-105", &books)?;
        }
        let lib = Library::open(&dir)?;

        let hidden = lib.search("", None, None, false, CoverFilter::All, 0, 50)?;
        assert_eq!(hidden.matched, 5, "из девяти записей спрятались четыре");
        assert!(!hidden.books.iter().any(|b| b.id == 100), "заметка спрятана");
        assert!(
            !hidden.books.iter().any(|b| b.id == 101),
            "18 страниц не спасает даже имя настоящего автора"
        );
        assert!(
            !hidden.books.iter().any(|b| b.id == 104),
            "брошюра настоящего автора тоже спрятана"
        );
        assert!(
            !hidden.books.iter().any(|b| b.id == 105),
            "40 страниц — ещё не книга"
        );
        for id in [102, 103] {
            assert!(
                hidden.books.iter().any(|b| b.id == id),
                "запись {id} должна остаться"
            );
        }

        let shown = lib.search("", None, None, true, CoverFilter::All, 0, 50)?;
        assert_eq!(shown.matched, 9, "с галочкой видно всё");
        assert!(shown.books.iter().any(|b| b.id == 100));
        assert!(shown.books.iter().any(|b| b.id == 101));
        assert!(shown.books.iter().any(|b| b.id == 104));
        assert!(shown.books.iter().any(|b| b.id == 105));
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Галочки по обложке делят выдачу на три части: всё, только с обложкой,
    /// только без обложки.
    ///
    /// Книг с обложкой в библиотеке примерно 89 %, и без этих галочек книги
    /// без обложки в выдаче тонут — а именно их и нужно видеть, когда ищешь,
    /// кому обложку ещё предстоит достать.
    ///
    /// Счётчик `matched` теперь не зависит от фильтров (обложка — такая же
    /// клауза запроса, как автор и жанр), поэтому галочки проверяются по
    /// `filtered`: именно по нему листается выдача.
    #[test]
    fn cover_filters_split_the_search() -> Result<()> {
        let dir = scratch("coverless");
        let mut books: Vec<BookMetadata> = [200u64, 201]
            .into_iter()
            .map(|id| BookMetadata {
                cover: Some(format!("1-2/{id}.avif")),
                ..sample_book(id)
            })
            .collect();
        // Третья книга без обложки — её галочка и должна показать.
        books.push(sample_book(202));
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("1-202", &books)?;
            // Фильтр по обложке теперь клауза запроса, поэтому книги должны
            // быть не только в метаданных, но и в индексе.
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;

        let all = lib.search("", None, None, false, CoverFilter::All, 0, 50)?;
        assert_eq!(all.matched, 3, "без галочек видно всё");
        assert_eq!(all.filtered, 3, "и в выдаче все три");

        let with = lib.search("", None, None, false, CoverFilter::WithCover, 0, 50)?;
        assert_eq!(with.matched, 3, "`matched` считается без фильтров");
        assert_eq!(with.filtered, 2, "«только с обложкой» прячет книги без обложки");
        assert!(with.books.iter().all(|b| b.cover.is_some()));

        let bare = lib.search("", None, None, false, CoverFilter::WithoutCover, 0, 50)?;
        assert_eq!(bare.matched, 3, "`matched` считается без фильтров");
        assert_eq!(bare.filtered, 1, "«без обложки» оставляет только книгу без обложки");
        assert_eq!(bare.books.len(), 1);
        assert_eq!(bare.books[0].id, 202);

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Точное название важнее частичного, а AND не даёт находить что попало.
    ///
    /// Обе книги содержат все термы запроса, но у первой название — ровно
    /// запрос: она должна стоять первой. Старый `QueryParser` с `OR` находил
    /// бы по такому запросу вообще всё, где есть слово «и».
    #[test]
    fn exact_title_ranks_above_partial_title() -> Result<()> {
        let dir = scratch("exact_title_rank");
        let books = vec![
            BookMetadata {
                id: 300,
                title: "Мастер и маргарита".into(),
                author: "Михаил Булгаков".into(),
                ..sample_book(300)
            },
            BookMetadata {
                id: 301,
                title: "Мастер и маргарита краткий разбор".into(),
                author: "Разборов".into(),
                ..sample_book(301)
            },
        ];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("1-301", &books)?;
            // Поиск идёт по индексу, поэтому книги мало записать в
            // метаданные — их нужно ещё и проиндексировать.
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;

        // (а) находятся обе книги: AND по термам «мастер», «и», «маргарита».
        let out = lib.search(
            "Мастер и маргарита",
            None,
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(out.matched, 2, "обе книги содержат все термы запроса");
        assert!(out.books.iter().any(|b| b.id == 300));
        assert!(out.books.iter().any(|b| b.id == 301));

        // (б) точное название — первое в порядке релевантности Tantivy.
        let hits = lib.full_text_hits("Мастер и маргарита", SEARCH_FETCH_CAP)?;
        let ranked: Vec<u64> = hits.iter().map(|(b, _score)| b.id).collect();
        assert_eq!(
            ranked,
            vec![300, 301],
            "релевантность: точное название должно быть выше частичного"
        );

        // Релевантность доходит и до публичной выдачи: при непустом запросе
        // `Facets` сортирует по score, а не по алфавиту.
        assert_eq!(out.books[0].id, 300, "точное название первым и в выдаче");

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// При непустом запросе порядок выдачи — по релевантности, а не по
    /// алфавиту, и это видно через публичный `search`.
    ///
    /// На примере заказчика («Мастер и маргарита» против «Мастер и маргарита
    /// краткий разбор») алфавит и релевантность совпадают, поэтому конкурент
    /// здесь специально начинается на «А»: по алфавиту он первый, а по score —
    /// второй, потому что в нём те же слова, но не фразой.
    #[test]
    fn relevance_beats_alphabet_for_nonempty_query() -> Result<()> {
        let dir = scratch("relevance_order");
        let books = vec![
            BookMetadata {
                id: 400,
                title: "Мастер и маргарита".into(),
                author: "Михаил Булгаков".into(),
                ..sample_book(400)
            },
            // Все термы запроса есть, но вразбивку: точной фразы нет, поэтому
            // score ниже, хотя по алфавиту книга левее.
            BookMetadata {
                id: 401,
                title: "Азбука: маргарита и мастер".into(),
                author: "Разборов".into(),
                ..sample_book(401)
            },
        ];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("400-401", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;

        let out = lib.search(
            "Мастер и маргарита",
            None,
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(out.matched, 2, "обе книги содержат все термы запроса");
        assert_eq!(
            out.books.iter().map(|b| b.id).collect::<Vec<_>>(),
            vec![400, 401],
            "при непустом запросе — по релевантности, лучшая первой"
        );

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Пустой запрос — это просмотр библиотеки, а не поиск: порядок остаётся
    /// алфавитным, даже если по score книги шли бы иначе.
    #[test]
    fn empty_query_stays_alphabetical() -> Result<()> {
        let dir = scratch("empty_alphabet");
        // Та же пара, что и в тесте выше: по алфавиту 510 идёт первой, по
        // релевантности — второй.
        let books = vec![
            BookMetadata {
                id: 510,
                title: "Азбука: маргарита и мастер".into(),
                author: "Разборов".into(),
                ..sample_book(510)
            },
            BookMetadata {
                id: 511,
                title: "Мастер и маргарита".into(),
                author: "Михаил Булгаков".into(),
                ..sample_book(511)
            },
        ];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("510-511", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;

        let out = lib.search("", None, None, false, CoverFilter::All, 0, 10)?;
        assert_eq!(out.matched, 2);
        assert_eq!(
            out.books.iter().map(|b| b.id).collect::<Vec<_>>(),
            vec![510, 511],
            "при пустом запросе — алфавит, а не релевантность"
        );

        // Для контраста: тот же набор с непустым запросом Tantivy оценивает
        // по score, и порядок переворачивается.
        let searched = lib.search(
            "Мастер и маргарита",
            None,
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(
            searched.books.iter().map(|b| b.id).collect::<Vec<_>>(),
            vec![511, 510],
            "с непустым запросом тот же набор идёт по релевантности"
        );

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Списки авторов: `keep_authors.txt` возвращает короткие вещи, а
    /// `hide_authors.txt` прячет их даже у настоящего автора.
    #[test]
    fn author_lists_change_what_is_hidden() -> Result<()> {
        let dir = scratch("lists");
        fs::create_dir_all(&dir)?;
        // Две книги «Автора» по 50 000 слов — он настоящий; плюс заметка
        // «Никто» на 500 слов и рассказ «Автора» на 4 500.
        let mut books: Vec<BookMetadata> = (1..=2u64).map(sample_book).collect();
        books.push(BookMetadata {
            id: 3,
            author: "Никто".into(),
            words: Some(500),
            ..sample_book(3)
        });
        books.push(BookMetadata {
            id: 4,
            words: Some(4_500),
            ..sample_book(4)
        });
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("1-4", &books)?;
        }

        // Так заметка спрятана, и 18-страничный рассказ «Автора» тоже:
        // короткую вещь имя автора не спасает.
        let lib = Library::open(&dir)?;
        assert_eq!(lib.search("", None, None, false, CoverFilter::All, 0, 10)?.matched, 2);

        // Вписали «Никто» в keep — заметка вернулась.
        fs::write(dir.join(KEEP_AUTHORS_FILE), "# никто\nНикто\n")?;
        let lib = Library::open(&dir)?;
        assert_eq!(lib.search("", None, None, false, CoverFilter::All, 0, 10)?.matched, 3);

        // А теперь спрятали самого Автора — ушли все его записи, даже книги
        // по 50 000 слов: список «прятать всегда» перебивает всё.
        fs::write(dir.join(HIDE_AUTHORS_FILE), "Автор\n")?;
        let lib = Library::open(&dir)?;
        assert_eq!(lib.search("", None, None, false, CoverFilter::All, 0, 10)?.matched, 1);
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Пустышку не спасает ни имя автора, ни жанр, а малую форму и большой
    /// файл — спасает.
    #[test]
    fn empty_books_are_hidden_without_exceptions() -> Result<()> {
        let dir = scratch("junk");
        fs::create_dir_all(&dir)?;
        // Книга настоящего автора на 50 000 слов — видна.
        let mut books = vec![sample_book(1)];
        // Его же запись на 5 слов: обложка и оглавление — прячется.
        books.push(BookMetadata {
            id: 2,
            words: Some(5),
            ..sample_book(2)
        });
        // Рассказ по жанру у автора без длинных книг — остаётся.
        books.push(BookMetadata {
            id: 3,
            author: "Сказочник".into(),
            genre_codes: vec!["short_story".into()],
            genres: vec!["Рассказы".into()],
            words: Some(500),
            ..sample_book(3)
        });
        // Такая же запись без жанра и без длинных книг — прячется.
        books.push(BookMetadata {
            id: 4,
            author: "Никто".into(),
            words: Some(500),
            ..sample_book(4)
        });
        // Сказка в тонком файле на 20 КБ — всё равно брошюра: размер
        // строже жанра.
        books.push(BookMetadata {
            id: 5,
            author: "Сказочник Второй".into(),
            genre_codes: vec!["child_tale".into()],
            words: Some(500),
            size: 20_000,
            ..sample_book(5)
        });
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("1-5", &books)?;
        }

        let lib = Library::open(&dir)?;
        // Видны только книга на 50 000 слов и рассказ по жанру: пустышка,
        // заметка и тонкая сказка спрятаны.
        assert_eq!(lib.search("", None, None, false, CoverFilter::All, 0, 10)?.matched, 2);
        // Галочка «статьи и заметки» показывает всё, включая пустышки.
        assert_eq!(lib.search("", None, None, true, CoverFilter::All, 0, 10)?.matched, 5);
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
            let out = lib.search("", None, None, false, CoverFilter::All, page * 3, 3)?;
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
        let all = lib.search("", None, None, false, CoverFilter::All, 0, 25)?;
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
        let tail = lib.search("", None, None, false, CoverFilter::All, 100, 10)?;
        assert!(tail.books.is_empty());
        assert_eq!(tail.matched, 25);

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Редкий автор находится, даже если текстовая выдача забита другими
    /// книгами. Это главная проверка задачи.
    ///
    /// Книг с общим словом в названии здесь **больше** [`SEARCH_FETCH_CAP`],
    /// поэтому прежнее поведение (фильтр в Rust поверх верхушки выдачи) книгу
    /// редкого автора не находило бы: у неё длинное название и самый низкий
    /// score, в первые 20 000 она не попадает. Теперь фильтр по `author_key`
    /// стоит в запросе, и Tantivy отбирает по нему **до** обрезки выдачи.
    #[test]
    fn rare_author_is_found_below_the_fetch_cap() -> Result<()> {
        let dir = scratch("rare_author");
        // На одну книгу больше предела: ровно столько, чтобы верхушка выдачи
        // без фильтра заведомо до редкой книги не дотянулась.
        let common = SEARCH_FETCH_CAP + 1;
        let rare_id = 10_000_000u64;
        let mut books: Vec<BookMetadata> = (0..common as u64)
            .map(|n| {
                let id = 1_000_000 + n;
                BookMetadata {
                    id,
                    // Короткое название — высокий score: в поле мало слов, и
                    // BM25 нормирует вклад терма по длине поля.
                    title: format!("Общее слово {n}"),
                    author: "Плодовитый Автор".into(),
                    ..sample_book(id)
                }
            })
            .collect();
        books.push(BookMetadata {
            id: rare_id,
            // Длинное название с тем же словом — score заведомо ниже, чем у
            // коротких, поэтому в верхушку без фильтра книга не попадает.
            title: "Общее слово и ещё очень много других слов в длинном названии".into(),
            author: "Редкий Автор".into(),
            ..sample_book(rare_id)
        });
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("rare", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;

        // Без фильтра верхушка выдачи до редкой книги не дотягивается.
        let top = lib.full_text_hits("общее слово", SEARCH_FETCH_CAP)?;
        assert_eq!(top.len(), SEARCH_FETCH_CAP, "верхушка заполнена другими книгами");
        assert!(
            !top.iter().any(|(b, _)| b.id == rare_id),
            "редкая книга не должна попадать в верхушку по score"
        );

        // С фильтром по редкому автору книга обязана найтись — и именно она.
        let out = lib.search(
            "общее слово",
            Some("Редкий Автор"),
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(out.filtered, 1, "под фильтр подходит ровно одна книга");
        assert_eq!(out.books.len(), 1);
        assert_eq!(out.books[0].id, rare_id);
        assert_eq!(out.books[0].author, "Редкий Автор");

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Фильтр по автору сравнивает нормализованные ключи, а не байты.
    ///
    /// [`normalize_key`] сворачивает регистр, пунктуацию, лишние пробелы и
    /// «ё»/«е», поэтому «Булгаков М.А.» находит книгу «Булгаков М. А.», а
    /// «МИХАИЛ БУЛГАКОВ» — «Михаил Булгаков». Порядок слов она **не** меняет,
    /// поэтому «Булгаков М. А.» и «Михаил Булгаков» остаются разными ключами:
    /// это известное ограничение, а не регрессия (раньше сравнение было ещё и
    /// регистрозависимым).
    #[test]
    fn author_filter_uses_normalized_key() -> Result<()> {
        let dir = scratch("author_key_filter");
        let books = vec![
            BookMetadata {
                id: 800,
                title: "Мастер и маргарита".into(),
                author: "Булгаков М. А.".into(),
                ..sample_book(800)
            },
            BookMetadata {
                id: 801,
                title: "Белая гвардия".into(),
                author: "Михаил Булгаков".into(),
                ..sample_book(801)
            },
        ];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("800-801", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;

        // Пунктуация и пробелы значения не имеют.
        let punct = lib.search("", Some("Булгаков М.А."), None, false, CoverFilter::All, 0, 10)?;
        assert_eq!(punct.filtered, 1, "«Булгаков М.А.» находит «Булгаков М. А.»");
        assert_eq!(punct.books[0].id, 800);

        // Регистр — тоже.
        let upper = lib.search(
            "",
            Some("МИХАИЛ БУЛГАКОВ"),
            None,
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(upper.filtered, 1, "регистр не важен");
        assert_eq!(upper.books[0].id, 801);

        // А разный порядок слов ключи не сводит — это две разные строки.
        assert_ne!(
            normalize_key("Булгаков М. А."),
            normalize_key("Михаил Булгаков")
        );

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Жанровый фильтр работает и при пустом запросе, и при непустом, а поле
    /// `genre` — multi-value: книга с двумя жанрами находится по любому из них.
    #[test]
    fn genre_filter_works_with_and_without_text() -> Result<()> {
        let dir = scratch("genre_filter");
        let make = |id: u64, title: &str, codes: &[&str]| {
            let codes: Vec<String> = codes.iter().map(|c| (*c).to_string()).collect();
            BookMetadata {
                id,
                title: title.into(),
                genre_codes: codes.clone(),
                // Названия считаются из кодов при загрузке; здесь то же самое,
                // чтобы индекс и память не разошлись.
                genres: crate::genres::names(&codes),
                ..sample_book(id)
            }
        };
        let books = vec![
            // Оба жанра у одной книги — проверка multi-value.
            make(810, "Звёздный десант", &["sf", "detective"]),
            make(811, "Звёздный путь", &["sf"]),
            make(812, "Тайна старого дома", &["detective"]),
        ];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("810-812", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;

        // Пустой запрос: фильтр по жанру идёт по всей библиотеке.
        let by_genre = lib.search("", None, Some("Детективы"), false, CoverFilter::All, 0, 10)?;
        assert_eq!(by_genre.matched, 3, "`matched` — вся библиотека без фильтров");
        assert_eq!(by_genre.filtered, 2, "детективы — 810 и 812");
        let ids: Vec<u64> = by_genre.books.iter().map(|b| b.id).collect();
        assert!(ids.contains(&810) && ids.contains(&812) && !ids.contains(&811));

        // Непустой запрос: фильтр сужает текстовую выдачу.
        let both = lib.search(
            "Звёздный",
            None,
            Some("Детективы"),
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(both.filtered, 1, "«Звёздный» + детективы — только 810");
        assert_eq!(both.books[0].id, 810);

        // Второй жанр той же книги тоже находится: поле multi-value.
        let sf = lib.search(
            "",
            None,
            Some("Научная фантастика"),
            false,
            CoverFilter::All,
            0,
            10,
        )?;
        assert_eq!(sf.filtered, 2, "фантастика — 810 и 811");

        // Регистр фильтра нормализуется так же, как при индексации.
        let lower = lib.search("", None, Some("детективы"), false, CoverFilter::All, 0, 10)?;
        assert_eq!(lower.filtered, 2, "регистр в фильтре по жанру не важен");

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Год разбирается в число, а «пусто» и мусор дают 0 — «год неизвестен».
    ///
    /// Фильтра по году в публичном API пока нет (это следующий этап), поэтому
    /// проверяем и разбор строки, и то, что в индекс уходит именно число.
    #[test]
    fn year_is_parsed_and_indexed_as_number() -> Result<()> {
        let details = |year: &str, date: &str| Details {
            year: year.into(),
            date: date.into(),
            ..Details::default()
        };
        // Ведущие цифры: «1967 г.» и «1967-1968» дают 1967.
        assert_eq!(book_year(&details("1967 г.", "")), 1967);
        assert_eq!(book_year(&details("1967-1968", "")), 1967);
        // Пустой «год» берём из даты.
        assert_eq!(book_year(&details("", "2005-01-01")), 2005);
        // «Неизвестен» — это 0: пусто, тире и мусор.
        assert_eq!(book_year(&details("", "")), 0);
        assert_eq!(book_year(&details("—", "")), 0);
        assert_eq!(book_year(&details("без года", "")), 0);

        // Значение действительно уходит в индекс.
        let dir = scratch("year_index");
        let mut dated = sample_book(900);
        dated.details.year = "1967 г.".into();
        let mut undated = sample_book(901);
        undated.details.date = "—".into();
        let books = vec![dated, undated];
        {
            let lib = Library::open(&dir)?;
            lib.append_metadata("900-901", &books)?;
            let mut writer = lib.writer()?;
            lib.index_books(&mut writer, &books)?;
        }
        let lib = Library::open(&dir)?;
        let searcher = lib.reader.searcher();
        let hits = searcher.search(
            &AllQuery,
            &TopDocs::with_limit(10).order_by_u64_field("id", Order::Asc),
        )?;
        let mut years: Vec<(u64, i64)> = Vec::new();
        for (_key, addr) in hits {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let id = doc
                .get_first(lib.f_id)
                .and_then(|v| v.as_u64())
                .expect("id в индексе есть всегда");
            let year = doc
                .get_first(lib.f_year)
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            years.push((id, year));
        }
        assert_eq!(years, vec![(900, 1967), (901, 0)]);

        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
}
