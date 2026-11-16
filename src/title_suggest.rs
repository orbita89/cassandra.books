//! Подсказки названий книг при наборе в строке поиска.
//!
//! Словарь строится один раз из метаданных (см.
//! [`crate::db::Library::title_dictionary`]) и дальше только читается:
//! подсказка — двоичный поиск по отсортированным ключам, без Tantivy.
//!
//! Чем отличается от [`crate::author_suggest::AuthorDictionary`]:
//!   * ключ — **хвост** названия, начиная с каждого слова («мастер и
//!     маргарита», «маргарита»), а не «слово + разделитель + остальное».
//!     Поэтому работает и многословный ввод: «мастер и м» находит роман;
//!   * ключи не копируют текст: это пары «номер названия, смещение слова» в
//!     общий буфер нормализованных названий. На 417 тысячах названий копии
//!     хвостов стоили бы ~100 МБ, ссылки — 8 байт на слово.
//!
//! Правила отбора:
//!   * меньше [`crate::author_suggest::MIN_QUERY_CHARS`] символов — пусто;
//!   * совпадение с начала названия выше совпадения с середины;
//!   * дальше — короткое (1–2 буквы) последнее слово совпало с **целым**
//!     словом: на «война и» «Война и мир» выше «Война Измерений». Для слов
//!     длиннее правило не действует: «при» — скорее недописанное «Призрак»,
//!     чем предлог из «При свечах»;
//!   * дальше — меньше лишних слов после введённого текста: на «мастер и м»
//!     сам роман выше «"Мастер и Маргарита": гимн демонизму?…», даже если
//!     статья есть в двух изданиях;
//!   * дальше — больше книг с таким названием, затем по алфавиту;
//!   * одно название — одна строка.

use std::collections::HashSet;

use crate::author_suggest::MIN_QUERY_CHARS;
use crate::db::normalize_key;

/// Сколько ключей с подходящим префиксом просматривать за раз. Короткий
/// префикс («при») совпадает с десятками тысяч слов; подсказок нужно
/// несколько штук, а просмотр всего диапазона на каждую паузу ввода — лишний
/// CPU.
const MAX_SCANNED_KEYS: usize = 20_000;

/// Название в словаре.
struct Title {
    /// Как показать (исходное написание одной из книг).
    name: String,
    /// Сколько книг с таким (нормализованным) названием.
    books: u32,
    /// Нормализованное название — диапазон в [`TitleDictionary::norm`].
    start: u32,
    end: u32,
}

/// Ключ: хвост названия `title`, начиная с байта `at` буфера.
#[derive(Clone, Copy)]
struct Key {
    title: u32,
    at: u32,
}

/// Одно совпадение при отборе подсказок.
#[derive(Clone, Copy)]
struct Hit {
    /// Совпало с начала названия, а не с середины.
    at_start: bool,
    /// Последнее введённое слово совпало с целым словом названия.
    whole_word: bool,
    /// Сколько слов названия осталось после введённого текста.
    extra: usize,
    title: u32,
}

impl Hit {
    /// Ключ ранга без числа книг: меньше — лучше.
    fn rank(&self) -> (bool, bool, usize) {
        (!self.at_start, !self.whole_word, self.extra)
    }
}

/// Словарь подсказок названий.
pub struct TitleDictionary {
    titles: Vec<Title>,
    /// Все нормализованные названия подряд.
    norm: String,
    /// Ключи, отсортированные по тексту хвоста.
    keys: Vec<Key>,
}

impl TitleDictionary {
    /// Пустой словарь — до первой сборки.
    pub fn empty() -> Self {
        Self {
            titles: Vec::new(),
            norm: String::new(),
            keys: Vec::new(),
        }
    }

    /// Построить словарь по `(название, число книг)`.
    ///
    /// Названия с одинаковой нормализацией ([`normalize_key`]) склеиваются,
    /// число книг складывается, а показывается алфавитно первое написание —
    /// так результат не зависит от порядка входа. Ключ даёт каждое слово не
    /// короче [`MIN_QUERY_CHARS`]: короче запрос всё равно не бывает.
    pub fn new(titles: impl IntoIterator<Item = (String, usize)>) -> Self {
        let mut merged: std::collections::HashMap<String, (String, usize)> =
            std::collections::HashMap::new();
        for (name, books) in titles {
            let key = normalize_key(&name);
            if key.is_empty() {
                continue;
            }
            merged
                .entry(key)
                .and_modify(|(shown, count)| {
                    *count += books;
                    if name < *shown {
                        *shown = name.clone();
                    }
                })
                .or_insert((name, books));
        }
        let mut merged: Vec<(String, (String, usize))> = merged.into_iter().collect();
        merged.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let mut norm = String::with_capacity(merged.iter().map(|(k, _)| k.len()).sum());
        let mut entries = Vec::with_capacity(merged.len());
        let mut keys = Vec::new();
        for (index, (key, (name, books))) in merged.into_iter().enumerate() {
            let start = norm.len();
            norm.push_str(&key);
            let mut word_start = 0;
            for word in key.split(' ') {
                if word.chars().count() >= MIN_QUERY_CHARS {
                    keys.push(Key {
                        title: index as u32,
                        at: (start + word_start) as u32,
                    });
                }
                word_start += word.len() + 1;
            }
            entries.push(Title {
                name,
                books: books.min(u32::MAX as usize) as u32,
                start: start as u32,
                end: norm.len() as u32,
            });
        }

        let mut dict = Self {
            titles: entries,
            norm,
            keys: Vec::new(),
        };
        keys.sort_unstable_by(|a, b| dict.tail(*a).cmp(dict.tail(*b)));
        dict.keys = keys;
        dict
    }

    /// Сколько разных названий в словаре.
    pub fn len(&self) -> usize {
        self.titles.len()
    }

    /// Текст ключа: хвост названия от слова до конца.
    fn tail(&self, key: Key) -> &str {
        let end = self.titles[key.title as usize].end as usize;
        &self.norm[key.at as usize..end]
    }

    /// Подсказки по введённому тексту: `(название, число книг)`.
    pub fn suggestions(&self, query: &str, limit: usize) -> Vec<(String, usize)> {
        let needle = normalize_key(query);
        if needle.chars().count() < MIN_QUERY_CHARS || limit == 0 {
            return Vec::new();
        }
        let first = self
            .keys
            .partition_point(|&key| self.tail(key) < needle.as_str());

        let needle_words = needle.split(' ').count();
        let short_last = needle.rsplit(' ').next().is_some_and(|w| w.chars().count() < MIN_QUERY_CHARS);
        let mut seen: HashSet<u32> = HashSet::new();
        let mut hits: Vec<Hit> = Vec::new();
        for &key in self.keys[first..].iter().take(MAX_SCANNED_KEYS) {
            if !self.tail(key).starts_with(needle.as_str()) {
                break;
            }
            let title = &self.titles[key.title as usize];
            let at_start = key.at == title.start;
            let tail = self.tail(key);
            let hit = Hit {
                at_start,
                whole_word: short_last
                    && (tail[needle.len()..].is_empty() || tail[needle.len()..].starts_with(' ')),
                extra: tail.split(' ').count().saturating_sub(needle_words),
                title: key.title,
            };
            if seen.insert(key.title) {
                hits.push(hit);
            } else if let Some(old) = hits.iter_mut().find(|h| h.title == key.title) {
                // То же название совпало ещё раз — берём лучшее из совпадений.
                if hit.rank() < old.rank() {
                    *old = hit;
                }
            }
        }

        hits.sort_by(|a, b| {
            let ta = &self.titles[a.title as usize];
            let tb = &self.titles[b.title as usize];
            a.rank()
                .cmp(&b.rank())
                .then_with(|| tb.books.cmp(&ta.books))
                .then_with(|| ta.name.cmp(&tb.name))
        });
        hits.truncate(limit);
        hits.into_iter()
            .map(|hit| {
                let title = &self.titles[hit.title as usize];
                (title.name.clone(), title.books as usize)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict() -> TitleDictionary {
        TitleDictionary::new(
            [
                ("Мастер и Маргарита", 3),
                ("мастер и маргарита", 2),
                ("Маргарита Наваррская", 1),
                ("Преступление и наказание", 5),
                ("Ёжик в тумане", 1),
                ("Мастера и подмастерья", 1),
            ]
            .map(|(name, n)| (name.to_string(), n)),
        )
    }

    fn names(out: Vec<(String, usize)>) -> Vec<String> {
        out.into_iter().map(|(name, _)| name).collect()
    }

    #[test]
    fn short_query_gives_nothing() {
        assert!(dict().suggestions("ма", 8).is_empty());
    }

    #[test]
    fn same_title_is_merged_and_counted() {
        let d = dict();
        assert_eq!(d.len(), 5);
        assert_eq!(
            d.suggestions("мастер и", 8),
            vec![("Мастер и Маргарита".to_string(), 5)]
        );
    }

    #[test]
    fn any_word_and_multi_word_prefix_match() {
        let d = dict();
        assert_eq!(names(d.suggestions("наказ", 8)), ["Преступление и наказание"]);
        assert_eq!(names(d.suggestions("мастер и м", 8)), ["Мастер и Маргарита"]);
        assert_eq!(names(d.suggestions("ежик", 8)), ["Ёжик в тумане"], "ё = е");
    }

    #[test]
    fn match_at_title_start_ranks_first() {
        // «Маргарита Наваррская» начинается с «марг», роман — нет, хотя книг
        // у романа больше.
        assert_eq!(
            names(dict().suggestions("марг", 8)),
            ["Маргарита Наваррская", "Мастер и Маргарита"]
        );
    }

    /// Полное совпадение выше длинного названия с тем же началом, даже если
    /// длинного больше изданий.
    #[test]
    fn fewer_extra_words_beat_more_copies() {
        let d = TitleDictionary::new(
            [
                ("\"Мастер и Маргарита\": гимн демонизму?", 2),
                ("Мастер и Маргарита", 1),
            ]
            .map(|(name, n)| (name.to_string(), n)),
        );
        assert_eq!(
            names(d.suggestions("мастер и м", 8)),
            ["Мастер и Маргарита", "\"Мастер и Маргарита\": гимн демонизму?"]
        );
    }

    /// Целое слово выше начала слова: «и» — союз, а не «Измерений».
    #[test]
    fn whole_last_word_beats_word_prefix() {
        let d = TitleDictionary::new(
            [("Война Измерений", 3), ("Война и мир", 1)].map(|(name, n)| (name.to_string(), n)),
        );
        assert_eq!(names(d.suggestions("война и", 8)), ["Война и мир", "Война Измерений"]);
    }

    /// Слово из трёх букв и больше — скорее недописанное, чем предлог.
    #[test]
    fn longer_last_word_is_treated_as_unfinished() {
        let d = TitleDictionary::new(
            [("При свечах", 1), ("Призрак", 4)].map(|(name, n)| (name.to_string(), n)),
        );
        assert_eq!(names(d.suggestions("при", 8)), ["Призрак", "При свечах"]);
    }

    #[test]
    fn limit_is_respected() {
        assert_eq!(dict().suggestions("мас", 1).len(), 1);
    }
}
