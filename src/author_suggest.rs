//! Подсказки автора: словарь в памяти, собранный из индекса.
//!
//! Раньше подсказки отбирались линейно из `App::authors` — фасетов текущей
//! выдачи, то есть из ~500 авторов. Автор за их пределами не появлялся в
//! подсказках никогда. Теперь источник — [`AuthorDictionary`]: один раз при
//! старте [`crate::db::Library::author_dictionary`] читает словарь термов поля
//! `author_key` (в нём на каждую книгу ровно один нормализованный автор) и
//! число книг (`doc_freq`), а [`AuthorDictionary::new`] строит по нему ключи
//! для поиска по началу **любого** слова. Tantivy при наборе текста не
//! трогается вообще.
//!
//! Старая функция [`author_suggestions`] больше не участвует в подсказках, но
//! оставлена: на неё опираются её собственные тесты, и она служит эталоном
//! линейного отбора «по вхождению» — в отличие от префиксного поиска по
//! словарю. Удалять её не стали, чтобы не терять проверенное поведение.
//!
//! Правила отбора подсказок:
//!   * 1–2 символа — подсказок нет (заказчик не хочет поиск на каждую букву);
//!   * совпадение — префикс любого слова имени, а не только начала строки;
//!   * один автор — одна строка, даже если подходит по нескольким словам;
//!   * внутри выдачи — по убыванию числа книг, при равенстве по имени;
//!   * не больше [`MAX_SUGGESTIONS`] штук.

use std::collections::HashSet;

use crate::db::normalize_key;

/// Сколько символов нужно ввести, чтобы подсказки вообще появились.
pub const MIN_QUERY_CHARS: usize = 3;

/// Больше подсказок не показываем, даже если совпадений много.
pub const MAX_SUGGESTIONS: usize = 8;

/// Разделитель «слово-ключ» и «остаток имени» внутри ключа поиска.
///
/// `\u{1}` не встречается в тексте: [`normalize_key`] оставляет только буквы,
/// цифры и пробелы. Разделитель не даёт `starts_with` склеивать разные слова:
/// ключ всегда начинается ровно с одного слова, и префикс не может «переехать»
/// через границу слова.
const KEY_SEP: char = '\u{1}';

/// Отобрать подсказки автора из готового списка `(имя, число книг)`.
///
/// **Эталонная реализация**: линейный перебор с поиском вхождения подстроки.
/// В интерфейсе больше не используется — подсказки даёт [`AuthorDictionary`],
/// — но оставлена вместе с тестами: она показывает, как вёлся отбор раньше, и
/// на ней удобно сверять новые правила (префикс по любому слову, дедупликация).
///
/// `limit` — желаемое число подсказок; сверху он дополнительно ограничен
/// [`MAX_SUGGESTIONS`], чтобы правило «не больше 8» нельзя было обойти
/// случайным вызовом. Возвращаются собственные строки: их удобно рисовать
/// в замыканиях egui, не держа ссылку на весь список.
#[allow(dead_code)]
pub fn author_suggestions(
    authors: &[(String, usize)],
    query: &str,
    limit: usize,
) -> Vec<(String, usize)> {
    let needle = query.trim().to_lowercase();
    // Длину считаем в символах, а не в байтах: «До» — это два символа,
    // хотя в UTF-8 они занимают четыре байта.
    if needle.chars().count() < MIN_QUERY_CHARS {
        return Vec::new();
    }
    let limit = limit.min(MAX_SUGGESTIONS);
    if limit == 0 {
        return Vec::new();
    }

    // Ключ сортировки считаем один раз на автора, а не на каждое сравнение.
    // `starts_with` — приоритет: совпадение с начала строки идёт раньше.
    let mut hits: Vec<(bool, &(String, usize))> = authors
        .iter()
        .filter_map(|author| {
            let name = author.0.to_lowercase();
            if name.contains(&needle) {
                Some((name.starts_with(&needle), author))
            } else {
                None
            }
        })
        .collect();

    // `true` (совпадение с начала) — первым; дальше по числу книг вниз;
    // при равном числе — по имени, чтобы порядок был предсказуемым.
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1 .1.cmp(&a.1 .1))
            .then_with(|| a.1 .0.cmp(&b.1 .0))
    });

    hits.into_iter()
        .take(limit)
        .map(|(_, (name, count))| (name.clone(), *count))
        .collect()
}

/// Автор в словаре: имя для показа и число книг.
struct Author {
    name: String,
    books: usize,
}

/// Один ключ поиска: срез в общем буфере ключей и номер автора.
///
/// Ключи хранятся одним плоским `String`, а не `Vec<String>`: на 117 тыс.
/// авторов ключей около 280 тыс., и заголовок `String` (24 байта) плюс
/// накладные расходы аллокатора на каждый ключ — это лишние ~10 МБ. Срез из
/// двенадцати байт дешевле.
struct KeyRef {
    start: u32,
    end: u32,
    author: u32,
}

/// Полный словарь авторов: имена с числом книг и отсортированные ключи.
///
/// Неизменяемый после сборки: подсказки — это только чтение и бинарный поиск,
/// поэтому структура спокойно живёт в `App` и не требует блокировок.
pub struct AuthorDictionary {
    /// Авторы в порядке, по которому на них ссылаются ключи.
    authors: Vec<Author>,
    /// Ключи, отсортированные по тексту ключа (байтовый порядок).
    keys: Vec<KeyRef>,
    /// Общий буфер, в котором лежат тексты всех ключей.
    buffer: String,
}

impl AuthorDictionary {
    /// Пустой словарь — состояние до первой сборки и запасной вариант, если
    /// индекс прочитать не удалось.
    pub fn empty() -> Self {
        Self {
            authors: Vec::new(),
            keys: Vec::new(),
            buffer: String::new(),
        }
    }

    /// Построить словарь по списку `(имя автора, число книг)`.
    ///
    /// Имена сортируются здесь, а не у вызывающего: порядок авторов — часть
    /// контракта структуры, и он не должен зависеть от того, как пришёл вход.
    ///
    /// Для каждого автора берётся **нормализованное** имя ([`normalize_key`],
    /// та же функция, что при индексации) и по одному ключу на каждое слово:
    /// `слово` + [`KEY_SEP`] + остальные слова через пробел. Так «булг»
    /// находит «Михаил Булгаков» по фамилии, а «мих» — по имени. Слова короче
    /// [`MIN_QUERY_CHARS`] ключей не дают: трёхсимвольный запрос всё равно не
    /// может с них начинаться.
    pub fn new(mut authors: Vec<(String, usize)>) -> Self {
        authors.sort_by(|a, b| a.0.cmp(&b.0));

        let mut entries: Vec<Author> = Vec::with_capacity(authors.len());
        // Сначала копим ключи отдельными строками: их надо отсортировать до
        // того, как разложить по общему буферу. После сборки вектор
        // освобождается, а в словаре остаются только срезы.
        let mut raw_keys: Vec<(String, u32)> = Vec::with_capacity(authors.len() * 2);
        for (index, (name, books)) in authors.into_iter().enumerate() {
            let author = index as u32;
            let key_name = normalize_key(&name);
            let words: Vec<&str> = key_name.split(' ').filter(|w| !w.is_empty()).collect();
            for (i, word) in words.iter().enumerate() {
                if word.chars().count() < MIN_QUERY_CHARS {
                    continue;
                }
                let mut key = String::with_capacity(key_name.len() + 1);
                key.push_str(word);
                key.push(KEY_SEP);
                let mut first = true;
                for (j, other) in words.iter().enumerate() {
                    if j == i {
                        continue;
                    }
                    if !first {
                        key.push(' ');
                    }
                    key.push_str(other);
                    first = false;
                }
                raw_keys.push((key, author));
            }
            entries.push(Author { name, books });
        }

        raw_keys.sort_by(|a, b| a.0.cmp(&b.0));
        // Повтор слова в имени даёт два одинаковых ключа одного автора.
        // Оставляем по одному.
        raw_keys.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

        let mut buffer = String::with_capacity(raw_keys.iter().map(|(key, _)| key.len()).sum());
        let mut keys: Vec<KeyRef> = Vec::with_capacity(raw_keys.len());
        for (key, author) in raw_keys {
            let start = buffer.len() as u32;
            buffer.push_str(&key);
            let end = buffer.len() as u32;
            keys.push(KeyRef { start, end, author });
        }

        Self {
            authors: entries,
            keys,
            buffer,
        }
    }

    /// Сколько авторов в словаре (для подсказки в интерфейсе).
    pub fn len(&self) -> usize {
        self.authors.len()
    }

    /// Текст ключа по ссылке.
    fn key_text(&self, key: &KeyRef) -> &str {
        &self.buffer[key.start as usize..key.end as usize]
    }

    /// Подсказки по введённому префиксу.
    ///
    /// Чистая функция над уже подготовленной структурой: ни Tantivy, ни
    /// изменяемого состояния. Ввод нормализуется той же [`normalize_key`], что
    /// и ключи, поэтому регистр, пунктуация и «ё»/«е» не важны.
    ///
    /// Ключи отсортированы, поэтому диапазон совпадений находится
    /// `partition_point` за O(log n), а не перебором. Дальше автор
    /// дедуплицируется (в имени может совпасть несколько слов), выдача
    /// сортируется по числу книг и имени и обрезается до `limit` /
    /// [`MAX_SUGGESTIONS`].
    pub fn suggestions(&self, query: &str, limit: usize) -> Vec<(String, usize)> {
        let needle = normalize_key(query);
        if needle.chars().count() < MIN_QUERY_CHARS {
            return Vec::new();
        }
        let limit = limit.min(MAX_SUGGESTIONS);
        if limit == 0 || self.keys.is_empty() {
            return Vec::new();
        }

        // Первый ключ, который не меньше префикса: всё, что начинается с
        // префикса, лежит подряд, начиная с этой позиции.
        let first = self
            .keys
            .partition_point(|key| self.key_text(key) < needle.as_str());

        let mut seen: HashSet<u32> = HashSet::new();
        let mut hits: Vec<u32> = Vec::new();
        for key in &self.keys[first..] {
            if !self.key_text(key).starts_with(needle.as_str()) {
                break;
            }
            // Один автор — одна строка, даже если совпало несколько его слов.
            if seen.insert(key.author) {
                hits.push(key.author);
            }
        }

        hits.sort_by(|&a, &b| {
            let a = &self.authors[a as usize];
            let b = &self.authors[b as usize];
            b.books.cmp(&a.books).then_with(|| a.name.cmp(&b.name))
        });
        hits.truncate(limit);
        hits.into_iter()
            .map(|index| {
                let author = &self.authors[index as usize];
                (author.name.clone(), author.books)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Небольшой список в том виде, в каком его отдаёт `App::authors`.
    fn authors() -> Vec<(String, usize)> {
        vec![
            ("Достоевский".to_string(), 12),
            ("Достоевская".to_string(), 3),
            ("Толстой".to_string(), 40),
            ("Алексей Толстой".to_string(), 25),
        ]
    }

    /// Словарь из пар «имя, число книг» — так же, как его строит `App`.
    fn dictionary(authors: &[(&str, usize)]) -> AuthorDictionary {
        AuthorDictionary::new(
            authors
                .iter()
                .map(|(name, count)| (name.to_string(), *count))
                .collect(),
        )
    }

    /// 1–2 символа — пусто: поиск не дёргаем на каждую букву.
    #[test]
    fn short_queries_return_nothing() {
        let authors = authors();
        assert!(author_suggestions(&authors, "", MAX_SUGGESTIONS).is_empty());
        assert!(author_suggestions(&authors, "Д", MAX_SUGGESTIONS).is_empty());
        assert!(author_suggestions(&authors, "До", MAX_SUGGESTIONS).is_empty());
        assert!(author_suggestions(&authors, "то", MAX_SUGGESTIONS).is_empty());

        let dict = dictionary(&[("Михаил Булгаков", 300)]);
        assert!(dict.suggestions("", MAX_SUGGESTIONS).is_empty());
        assert!(dict.suggestions("б", MAX_SUGGESTIONS).is_empty());
        assert!(dict.suggestions("бу", MAX_SUGGESTIONS).is_empty());
    }

    /// С трёх символов подсказки уже находятся.
    #[test]
    fn three_chars_find_authors() {
        let authors = authors();
        let found = author_suggestions(&authors, "Дос", MAX_SUGGESTIONS);
        assert_eq!(found.len(), 2, "оба Достоевских: {found:?}");
        assert!(found.iter().any(|(name, _)| name == "Достоевский"));
        assert!(found.iter().any(|(name, _)| name == "Достоевская"));
    }

    /// Подсказок не больше лимита и не больше жёсткого предела в 8.
    #[test]
    fn respects_the_limit() {
        let authors = authors();
        let found = author_suggestions(&authors, "Тол", 2);
        assert_eq!(found.len(), 2, "лимит 2 соблюдён: {found:?}");

        // Больше восьми не отдаём даже при большом лимите.
        let many: Vec<(String, usize)> =
            (0..20).map(|i| (format!("Толстой {i}"), 20 - i)).collect();
        let found = author_suggestions(&many, "Тол", 100);
        assert_eq!(found.len(), MAX_SUGGESTIONS);
    }

    /// Регистр не важен: «дос», «ДОС» и «Дос» дают одно и то же.
    #[test]
    fn case_does_not_matter() {
        let authors = authors();
        let lower = author_suggestions(&authors, "дос", MAX_SUGGESTIONS);
        let upper = author_suggestions(&authors, "ДОС", MAX_SUGGESTIONS);
        let mixed = author_suggestions(&authors, "Дос", MAX_SUGGESTIONS);
        assert_eq!(lower, upper);
        assert_eq!(lower, mixed);
        assert!(!lower.is_empty());
    }

    /// Совпадение с начала строки идёт раньше совпадения в середине,
    /// даже если у «серединного» автора книг больше.
    #[test]
    fn prefix_matches_come_first() {
        let authors = authors();
        let found = author_suggestions(&authors, "Тол", MAX_SUGGESTIONS);
        assert_eq!(
            found
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["Толстой", "Алексей Толстой"],
            "сначала начало строки, потом вхождение в середине"
        );
        // Внутри группы — по убыванию числа книг.
        assert_eq!(found[0].1, 40);
        assert_eq!(found[1].1, 25);
    }

    // --- Словарь: поиск по началу любого слова -----------------------------

    /// (а) и (б): фамилия и имя находят автора одинаково — ключ есть на
    /// каждое слово, а не только на начало строки.
    #[test]
    fn dictionary_finds_by_surname_and_by_first_name() {
        let dict = dictionary(&[("Михаил Булгаков", 300), ("Фёдор Достоевский", 120)]);
        assert_eq!(
            dict.suggestions("булг", MAX_SUGGESTIONS),
            vec![("Михаил Булгаков".to_string(), 300)],
            "фамилия должна находиться"
        );
        assert_eq!(
            dict.suggestions("мих", MAX_SUGGESTIONS),
            vec![("Михаил Булгаков".to_string(), 300)],
            "имя тоже должно находиться"
        );
        assert_eq!(
            dict.suggestions("булгаков", MAX_SUGGESTIONS),
            vec![("Михаил Булгаков".to_string(), 300)]
        );
    }

    /// (в): 1–2 символа не ищутся; (г): больше [`MAX_SUGGESTIONS`] не отдаём.
    #[test]
    fn dictionary_short_queries_and_limit() {
        let mut many: Vec<(String, usize)> = (0..20)
            .map(|i| (format!("Толстой {i}"), 20 - i))
            .collect();
        many.push(("Алексей Толстой".to_string(), 1));
        let dict = AuthorDictionary::new(many);

        assert!(dict.suggestions("т", MAX_SUGGESTIONS).is_empty());
        assert!(dict.suggestions("то", MAX_SUGGESTIONS).is_empty());
        assert_eq!(
            dict.suggestions("тол", MAX_SUGGESTIONS).len(),
            MAX_SUGGESTIONS
        );
        // Явно меньший лимит тоже соблюдается.
        assert_eq!(dict.suggestions("тол", 3).len(), 3);
    }

    /// (д): регистр и «ё»/«е» при вводе не важны.
    #[test]
    fn dictionary_folds_case_and_yo() {
        let dict = dictionary(&[("Фёдор Достоевский", 120)]);
        let lower = dict.suggestions("фед", MAX_SUGGESTIONS);
        let upper = dict.suggestions("ФЁД", MAX_SUGGESTIONS);
        let mixed = dict.suggestions("Фед", MAX_SUGGESTIONS);
        assert!(!lower.is_empty());
        assert_eq!(lower, upper, "«Ё» и «Е» — один ключ");
        assert_eq!(lower, mixed, "регистр не важен");
        assert_eq!(dict.suggestions("дос", MAX_SUGGESTIONS).len(), 1);
    }

    /// (е): автор подходит по двум словам, но показывается один раз.
    #[test]
    fn dictionary_deduplicates_an_author() {
        let dict = dictionary(&[("Иван Иванович", 7)]);
        // «ива» — префикс и имени, и отчества: два ключа, одна строка.
        let found = dict.suggestions("ива", MAX_SUGGESTIONS);
        assert_eq!(found, vec![("Иван Иванович".to_string(), 7)]);
        // И у того же автора ключи «иван…» и «иванович…» — тоже одна строка.
        assert_eq!(dict.suggestions("иванов", MAX_SUGGESTIONS).len(), 1);
    }

    /// Внутри выдачи — по убыванию числа книг, при равенстве по имени.
    #[test]
    fn dictionary_sorts_by_books_then_name() {
        let dict = dictionary(&[
            ("Толстой Алексей", 5),
            ("Толстой Лев", 40),
            ("Толстой Борис", 5),
        ]);
        let found = dict.suggestions("тол", MAX_SUGGESTIONS);
        assert_eq!(
            found
                .iter()
                .map(|(name, count)| (name.as_str(), *count))
                .collect::<Vec<_>>(),
            vec![
                ("Толстой Лев", 40),
                ("Толстой Алексей", 5),
                ("Толстой Борис", 5),
            ],
            "сначала больше книг, при равенстве — по имени"
        );
    }

    /// Пустой словарь (до сборки или если индекс не прочитался) безопасен.
    #[test]
    fn empty_dictionary_returns_nothing() {
        let dict = AuthorDictionary::empty();
        assert_eq!(dict.len(), 0);
        assert!(dict.suggestions("булг", MAX_SUGGESTIONS).is_empty());
    }
}
