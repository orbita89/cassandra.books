//! Вынос дубликатов из библиотеки — кнопка «Убрать дубликаты».
//!
//! Решение о том, что считать дубликатом, принимает `dedup.py` и записывает
//! в `dedup_out/plan.tsv`: одна строка — одну книгу убрать, рядом `keeper_id`
//! (какая копия остаётся) и `reason` (почему так решили). Здесь план только
//! исполняется, и исполняется так, чтобы ничего не потерялось:
//!
//!   1. обложки уезжают в `dublicate/covers/` (перенос, не удаление);
//!   2. каждый затронутый `.arx` распаковывается во временный каталог, книги
//!      из плана уезжают в `dublicate/archives/`, остальное пакуется обратно
//!      и архив подменяется целиком — только после проверки, что новый
//!      архив открывается и содержит ровно то, что должен;
//!   3. из `metadata/*.jsonl` убираются строки этих книг, а сами строки
//!      дописываются в `dublicate/metadata/` — то есть запись тоже не
//!      теряется, а переезжает;
//!   4. старый `index/` уезжает в `dublicate/index/`, индекс собирается заново
//!      из оставшихся метаданных.
//!
//! Прерывание на любом шаге не портит данные: переносы идемпотентны, а
//! архив подменяется атомарным `rename` последним действием. Повторный
//! запуск просто доделает остаток — книги, которых в архиве уже нет,
//! пропускаются.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use crate::db::Library;
use crate::human_size;

/// Куда переезжают дубликаты. Каталог рядом с `data/`, а не внутри него:
/// `data/` — это библиотека, `dublicate/` — то, что из неё вынесли.
pub const DUBLICATE_DIR: &str = "dublicate";
/// Имя плана внутри каталога отчётов.
pub const PLAN_NAME: &str = "plan.tsv";
/// Каталог отчётов: `dedup_out` рядом с `data/`.
pub const REPORT_DIR: &str = "dedup_out";

/// Сжатие пересобранных архивов — ровно то же, чем архивы собирались при
/// импорте, без «почти такого же» уровня: пересобранный архив должен
/// отличаться от исходного только вынесенными книгами, и ничем больше.
/// Константа общая с импортом, чтобы уровни не разъехались.
pub const ARCHIVE_COMPRESSION: &str = crate::worker::ARX_COMPRESSION;

/// Сколько архивов разрешено собирать одновременно.
///
/// Память считает не распаковка, а `arx create`: он держит словари zstd и
/// просит 1.4 ГБ на уровне 19 (9.7 ГБ на 22, см.
/// [`crate::worker::ARX_COMPRESSION`]). Потоков у выноса до двенадцати, и без
/// этого ограничения они собирали бы двенадцать архивов разом — именно так
/// процесс и убивало OOM-killer'ом. Два — потолок с запасом: даже на уровне
/// 22 это 19 ГБ, а распаковки идут своим чередом, пока идёт сборка.
pub const CREATE_LIMIT: usize = 2;

/// Одна книга из плана — её надо вынести.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanItem {
    pub id: u64,
    pub archive: String,
    pub internal_path: String,
    pub cover: Option<String>,
    pub group_id: String,
    pub title: String,
    pub author: String,
    pub year: String,
    pub size: u64,
    /// Какая копия остаётся в библиотеке.
    pub keeper_id: u64,
    /// Почему решили, что это дубликат: id, hash, isbn, size, cover…
    pub reason: String,
}

/// План выноса целиком.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub path: PathBuf,
    pub items: Vec<PlanItem>,
}

impl Plan {
    /// Прочитать `plan.tsv`. Заголовок обязателен: лучше отказаться, чем
    /// вынести книги по чужому файлу.
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать план {}", path.display()))?;
        let mut lines = text.lines();
        let header: Vec<&str> = lines
            .next()
            .context("пустой план: нет строки заголовка")?
            .split('\t')
            .collect();
        // Колонки ищем по именам, а не по номерам: план может обрасти
        // новыми полями, и это не должно ломать кнопку.
        let column = |name: &str| -> Result<usize> {
            header.iter().position(|h| *h == name).with_context(|| {
                format!("в плане нет колонки `{name}` — пересоберите его: python3 dedup.py --phash")
            })
        };
        let c_id = column("id")?;
        let c_archive = column("archive")?;
        let c_path = column("internal_path")?;
        let c_cover = column("cover")?;
        let c_group = column("group_id")?;
        let c_title = column("title")?;
        let c_author = column("author")?;
        let c_year = column("year")?;
        let c_size = column("size")?;
        let c_keeper = column("keeper_id")?;
        let c_reason = column("reason")?;

        let mut items = Vec::new();
        for (number, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            let widest = [
                c_id, c_archive, c_path, c_cover, c_group, c_title, c_author, c_year, c_size,
                c_keeper, c_reason,
            ]
            .into_iter()
            .max()
            .unwrap_or(0);
            if fields.len() <= widest {
                bail!("{}: строка {} короче заголовка", path.display(), number + 2);
            }
            let number_at = |index: usize| -> Result<u64> {
                fields[index].parse::<u64>().with_context(|| {
                    format!(
                        "{}: строка {}: `{}` не число",
                        path.display(),
                        number + 2,
                        fields[index]
                    )
                })
            };
            let cover = fields[c_cover].trim();
            items.push(PlanItem {
                id: number_at(c_id)?,
                archive: fields[c_archive].to_string(),
                internal_path: fields[c_path].to_string(),
                cover: (!cover.is_empty()).then(|| cover.to_string()),
                group_id: fields[c_group].to_string(),
                title: fields[c_title].to_string(),
                author: fields[c_author].to_string(),
                year: fields[c_year].to_string(),
                size: number_at(c_size)?,
                keeper_id: number_at(c_keeper)?,
                reason: fields[c_reason].to_string(),
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            items,
        })
    }

    /// Сколько книг, архивов и байт в плане.
    pub fn summary(&self) -> (usize, usize, u64) {
        let archives: BTreeSet<&str> = self.items.iter().map(|i| i.archive.as_str()).collect();
        let bytes = self.items.iter().map(|i| i.size).sum();
        (self.items.len(), archives.len(), bytes)
    }

    /// Книги плана по архивам: один архив за один проход.
    fn by_archive(&self) -> BTreeMap<&str, Vec<&PlanItem>> {
        let mut out: BTreeMap<&str, Vec<&PlanItem>> = BTreeMap::new();
        for item in &self.items {
            out.entry(item.archive.as_str()).or_default().push(item);
        }
        out
    }
}

/// Где искать план, если пользователь не задал путь: `dedup_out` рядом с
/// `data/`, а если библиотека лежит отдельно — то внутри неё.
pub fn find_plan(data_dir: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(parent) = data_dir.parent() {
        candidates.push(parent.join(REPORT_DIR).join(PLAN_NAME));
    }
    candidates.push(data_dir.join(REPORT_DIR).join(PLAN_NAME));
    candidates.into_iter().find(|path| path.is_file())
}

/// Сколько архивов пересобирать одновременно, если пользователь не выбрал.
///
/// Каждый поток держит распакованный архив во временном каталоге (это гигабайты
/// на диск) и грузит одно ядро zstd, поэтому «сколько ядер» здесь не ответ:
/// 12 потоков — это уже 200 МБ/с на входе и под 50 ГБ во временных файлах.
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().min(12))
        .unwrap_or(4)
}

/// Каталог, куда переезжают дубликаты.
pub fn dublicate_dir(data_dir: &Path) -> PathBuf {
    data_dir
        .parent()
        .unwrap_or(Path::new("."))
        .join(DUBLICATE_DIR)
}

// ---------------------------------------------------------------------------
// Прогресс
// ---------------------------------------------------------------------------

/// На каком шаге вынос.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Covers,
    Metadata,
    Index,
    /// Пересборка архивов идёт последней: она самая долгая, а библиотека к
    /// этому моменту уже чистая и рабочая.
    Archives,
    Done,
}

impl Step {
    pub fn title(self) -> &'static str {
        match self {
            Step::Covers => "обложки",
            Step::Archives => "архивы",
            Step::Metadata => "метаданные",
            Step::Index => "индекс",
            Step::Done => "готово",
        }
    }
}

/// Очередная порция прогресса.
#[derive(Clone, Debug)]
pub struct Progress {
    pub step: Step,
    pub done: usize,
    pub total: usize,
    pub status: String,
}

/// Что получилось.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub books: usize,
    pub archives_rebuilt: usize,
    pub archives_skipped: usize,
    pub covers_moved: usize,
    pub metadata_dropped: usize,
    /// Сколько книг плана в метаданных уже не было (вынесли в прошлый раз).
    pub already_gone: usize,
    pub bytes_moved: u64,
    pub seconds: f64,
    /// Архивы, которые не удалось пересобрать: их книги остались в
    /// библиотеке, чтобы не было битых ссылок.
    pub errors: Vec<String>,
}

impl Outcome {
    pub fn report(&self) -> String {
        let mut out = format!(
            "книг убрано из библиотеки: {}, архивов пересобрано: {}, \
             архивов уже чистых: {}, обложек перенесено: {}, строк убрано из \
             метаданных: {}, объём: {}, время: {:.0} с",
            self.books,
            self.archives_rebuilt,
            self.archives_skipped,
            self.covers_moved,
            self.metadata_dropped,
            human_size(self.bytes_moved),
            self.seconds,
        );
        if self.already_gone > 0 {
            out.push_str(&format!(
                "\nуже были вынесены раньше (в метаданных их не было): {}",
                self.already_gone
            ));
        }
        if !self.errors.is_empty() {
            out.push_str(&format!(
                "\n[!] не пересобрано архивов: {} — их книги оставлены в библиотеке:\n  {}",
                self.errors.len(),
                self.errors.join("\n  ")
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Исполнение плана
// ---------------------------------------------------------------------------

/// Вынести дубликаты. `report` зовётся из нескольких потоков, поэтому
/// принимается как `Sync`-замыкание: обычно это отправка в канал UI.
pub fn execute(
    plan: &Plan,
    data_dir: &Path,
    jobs: usize,
    report: &(dyn Fn(Progress) + Sync),
) -> Result<Outcome> {
    let started = std::time::Instant::now();
    let mut outcome = Outcome::default();
    if plan.items.is_empty() {
        outcome.seconds = started.elapsed().as_secs_f64();
        return Ok(outcome);
    }
    let dest = dublicate_dir(data_dir);
    // Сначала чиним то, что могло остаться от оборванного запуска.
    let restored = recover_archives(&data_dir.join("archives"))?;
    if restored > 0 {
        report(Progress {
            step: Step::Covers,
            done: 0,
            total: plan.items.len(),
            status: format!("вернули на место архивов после обрыва: {restored}"),
        });
    }
    for sub in ["archives", "covers", "metadata"] {
        fs::create_dir_all(dest.join(sub))
            .with_context(|| format!("не удалось создать {}", dest.join(sub).display()))?;
    }

    // --- 1. обложки --------------------------------------------------------
    report(Progress {
        step: Step::Covers,
        done: 0,
        total: plan.items.len(),
        status: "переносим обложки".into(),
    });
    let mut covers = 0usize;
    for (number, item) in plan.items.iter().enumerate() {
        if let Some(cover) = &item.cover {
            let from = data_dir.join("covers").join(cover);
            if from.is_file() {
                let to = dest.join("covers").join(cover);
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)?;
                }
                move_file(&from, &to)?;
                covers += 1;
            }
        }
        if number % 2000 == 0 {
            report(Progress {
                step: Step::Covers,
                done: number,
                total: plan.items.len(),
                status: "переносим обложки".into(),
            });
        }
    }
    outcome.covers_moved = covers;

    // --- 2. что вообще можно убирать ---------------------------------------
    // Книгу убираем из библиотеки, если её EPUB либо ещё лежит в архиве,
    // либо уже переехал в `dublicate/`. Если ни там ни там — оставляем:
    // ссылок в никуда в библиотеке быть не должно.
    //
    // Признак — файл, а не id: в библиотеке встречаются книги с одинаковым id
    // в разных архивах, и по id под вынос попала бы и та копия, которую мы
    // оставляем (так уже терялись книги). `\u{1}` в ключе — чтобы `a/b` и
    // `a` + `b/c` не склеились в одно.
    let grouped = plan.by_archive();
    let archives_dir = data_dir.join("archives");
    let mut safe: HashSet<String> = HashSet::new();
    for (archive, items) in &grouped {
        let stem = archive.trim_end_matches(".arx");
        let present = list_archive(&archives_dir.join(archive)).unwrap_or_default();
        for item in items.iter() {
            let in_archive = present.iter().any(|entry| entry == &item.internal_path);
            let in_dest = dest
                .join("archives")
                .join(stem)
                .join(&item.internal_path)
                .is_file();
            if in_archive || in_dest {
                safe.insert(file_key(&item.archive, &item.internal_path));
            }
        }
    }

    // --- 3. метаданные -----------------------------------------------------
    // Строки вынесенных книг переезжают в `dublicate/metadata/<шард>`, а не
    // пропадают: по ним всегда можно понять, что именно убрали и откуда.
    let planned: HashSet<u64> = plan.items.iter().map(|item| item.id).collect();
    let mut still_there: HashSet<u64> = HashSet::new();

    let metadata_dir = data_dir.join("metadata");
    let shards = metadata_shards(&metadata_dir)?;
    let mut dropped = 0usize;
    let mut rewritten: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for shard in &shards {
        let text = fs::read_to_string(shard)
            .with_context(|| format!("не удалось прочитать {}", shard.display()))?;
        let mut kept_lines: Vec<&str> = Vec::new();
        let mut gone_lines: Vec<&str> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let removable = line_file(line).is_some_and(|key| safe.contains(&key));
            if removable {
                gone_lines.push(line);
                dropped += 1;
                continue;
            }
            if let Some(id) = line_id(line) {
                if planned.contains(&id) {
                    still_there.insert(id);
                }
            }
            kept_lines.push(line);
        }
        if gone_lines.is_empty() {
            continue;
        }
        let name = shard
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("metadata.jsonl"));
        write_lines(&dest.join("metadata").join(name), &gone_lines, true)?;
        rewritten.insert(
            shard.clone(),
            kept_lines.iter().map(|l| l.to_string()).collect(),
        );
    }
    outcome.metadata_dropped = dropped;
    outcome.books = dropped;
    // Книги, которых в метаданных уже не было, — это повторный запуск после
    // успешного выноса. Это не ошибка, так и напишем. Вычитание с насыщением:
    // строк с одним id бывает больше, чем строк плана.
    outcome.already_gone = planned
        .len()
        .saturating_sub(still_there.len() + dropped);
    if !still_there.is_empty() {
        outcome.errors.push(format!(
            "{} книг оставлены в библиотеке: их файла нет ни в архиве, ни в dublicate/. \
             Перезапустите вынос — он доделает остаток",
            still_there.len()
        ));
    }
    report(Progress {
        step: Step::Metadata,
        done: shards.len(),
        total: shards.len(),
        status: format!("вынесено {dropped} записей"),
    });
    for (shard, lines) in rewritten {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        replace_lines(&shard, &refs)?;
    }

    // --- 4. индекс ---------------------------------------------------------
    // Старый индекс переезжает в `dublicate/index/<дата>`: если новый
    // почему-то не соберётся, старый останется рядом и его можно вернуть.
    report(Progress {
        step: Step::Index,
        done: 0,
        total: 1,
        status: "пересобираем индекс".into(),
    });
    let index_dir = data_dir.join("index");
    if index_dir.exists() {
        let stamp = format!("index-{}", now_stamp());
        let target = dest.join(&stamp);
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        if fs::rename(&index_dir, &target).is_err() {
            // Другой раздел — тогда копируем.
            copy_dir(&index_dir, &target)?;
            fs::remove_dir_all(&index_dir)
                .with_context(|| format!("не удалось убрать {}", index_dir.display()))?;
        }
    }
    let mut library = Library::open(data_dir).context("не удалось открыть библиотеку заново")?;
    library.rebuild_index().context("не удалось пересобрать индекс")?;
    let books_left = library.books.len();

    // --- 5. архивы ---------------------------------------------------------
    // Самая долгая часть: `arx` не умеет удалять записи, поэтому каждый архив
    // распаковывается целиком, дубликаты переезжают в `dublicate/archives/`, а
    // остаток собирается заново и подменяет исходный файл. Библиотека к этому
    // моменту уже чистая, так что прерывание здесь ничего не портит.
    let total_archives = grouped.len();
    let counter = AtomicUsize::new(0);
    let work_root = data_dir.join("tmp").join("dedup_work");
    fs::create_dir_all(&work_root)?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()
        .context("не удалось создать пул потоков")?;

    report(Progress {
        step: Step::Archives,
        done: 0,
        total: total_archives,
        status: "пересобираем архивы".into(),
    });
    // Распаковок одновременно вчетверо меньше, чем потоков: см. [`Gate`].
    let extract_gate = Gate::new(jobs / 4);
    // А сборок архива — не больше [`CREATE_LIMIT`]: память ест именно
    // `arx create`, и без этого ограничения потоки выноса собирали бы
    // архивов столько же, сколько их самих.
    let create_gate = Gate::new(CREATE_LIMIT);
    let jobs_list: Vec<(&str, &Vec<&PlanItem>)> = grouped.iter().map(|(k, v)| (*k, v)).collect();
    let results: Vec<(&str, Result<Rebuild>)> = pool.install(|| {
        jobs_list
            .par_iter()
            .map(|(archive, items)| {
                let result = rebuild_archive(
                    archive,
                    items,
                    &archives_dir,
                    &dest,
                    &work_root,
                    &extract_gate,
                    &create_gate,
                );
                let done = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let status = match &result {
                    Ok(Rebuild::Rebuilt { moved, .. }) => {
                        format!("{archive}: вынесено {moved}")
                    }
                    Ok(Rebuild::Nothing) => format!("{archive}: уже чисто"),
                    Err(err) => format!("{archive}: ошибка — {err}"),
                };
                report(Progress {
                    step: Step::Archives,
                    done,
                    total: total_archives,
                    status,
                });
                (*archive, result)
            })
            .collect()
    });
    for (archive, result) in results {
        match result {
            Ok(Rebuild::Rebuilt { moved, bytes, ids }) => {
                outcome.archives_rebuilt += 1;
                outcome.bytes_moved += bytes;
                debug_assert_eq!(moved, ids.len());
            }
            Ok(Rebuild::Nothing) => outcome.archives_skipped += 1,
            Err(err) => outcome.errors.push(format!("{archive}: {err}")),
        }
    }
    let _ = fs::remove_dir_all(&work_root);
    if outcome.errors.len() > 1 || outcome.archives_rebuilt + outcome.archives_skipped < total_archives
    {
        outcome.errors.push(format!(
            "{} архивов не пересобрано: дубликаты остались внутри .arx. \
             Библиотека уже чистая — повторный запуск доделает остаток",
            total_archives - outcome.archives_rebuilt - outcome.archives_skipped
        ));
    }

    outcome.seconds = started.elapsed().as_secs_f64();
    report(Progress {
        step: Step::Done,
        done: books_left,
        total: books_left,
        status: format!("книг осталось: {books_left}"),
    });
    Ok(outcome)
}

/// Ограничитель одновременных распаковок.
///
/// `arx extract` внутри себя многопоточный (около шести потоков), а
/// `arx create` — однопоточный. Если запустить распаковку во всех потоках
/// сразу, получается 12 × 6 потоков на 16 ядрах: суммарная скорость падает
/// в десять раз (проверено: 61 МБ/с против 630 МБ/с у одиночного запуска).
/// Поэтому распаковок одновременно не больше `jobs / 4`, а собираются архивы
/// параллельно и во все потоки.
struct Gate {
    free: std::sync::Mutex<usize>,
    wake: std::sync::Condvar,
}

impl Gate {
    fn new(limit: usize) -> Self {
        Self {
            free: std::sync::Mutex::new(limit.max(1)),
            wake: std::sync::Condvar::new(),
        }
    }

    /// Зайти (ждём, если все места заняты) и выйти по [`Gate::leave`].
    fn enter(&self) -> GateGuard<'_> {
        let mut free = self.free.lock().unwrap_or_else(|err| err.into_inner());
        while *free == 0 {
            free = self.wake.wait(free).unwrap_or_else(|err| err.into_inner());
        }
        *free -= 1;
        GateGuard { gate: self }
    }
}

/// Пропуск, который возвращает место обратно при разрушении.
struct GateGuard<'a> {
    gate: &'a Gate,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        let mut free = self.gate.free.lock().unwrap_or_else(|err| err.into_inner());
        *free += 1;
        self.gate.wake.notify_one();
    }
}

/// Что сделали с одним архивом.
enum Rebuild {
    /// Архив пересобран: сколько книг вынесли, сколько байт и какие id.
    Rebuilt {
        moved: usize,
        bytes: u64,
        ids: Vec<u64>,
    },
    /// В архиве уже нет книг из плана — пересобирать нечего.
    Nothing,
}

/// Пересобрать один `.arx` без книг из плана.
///
/// Порядок важен: сначала распаковали, потом вынесли лишнее, потом собрали
/// новый архив и проверили его, и только потом подменили старый. Пока
/// подмены не было, исходный архив цел, что бы ни случилось.
fn rebuild_archive(
    archive: &str,
    items: &[&PlanItem],
    archives_dir: &Path,
    dest: &Path,
    work_root: &Path,
    extract_gate: &Gate,
    create_gate: &Gate,
) -> Result<Rebuild> {
    let source = archives_dir.join(archive);
    if !source.is_file() {
        bail!("архив не найден");
    }
    let present: HashSet<String> = list_archive(&source)?.into_iter().collect();
    let wanted: Vec<&&PlanItem> = items
        .iter()
        .filter(|item| present.contains(&item.internal_path))
        .collect();
    if wanted.is_empty() {
        return Ok(Rebuild::Nothing);
    }

    let stem = archive.trim_end_matches(".arx");
    let work = work_root.join(stem);
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let result = (|| -> Result<Rebuild> {
        {
            let _slot = extract_gate.enter();
            run_arx(&["extract", &source.to_string_lossy(), "-C", &work.to_string_lossy()])?;
        }

        let mut bytes = 0u64;
        let mut ids = Vec::new();
        for item in &wanted {
            let from = work.join(&item.internal_path);
            if !from.is_file() {
                bail!("после распаковки нет {}", item.internal_path);
            }
            let to = dest.join("archives").join(stem).join(&item.internal_path);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            move_file(&from, &to)?;
            bytes += item.size;
            ids.push(item.id);
        }

        // Собираем новый архив рядом и проверяем, что он читается и содержит
        // ровно оставшиеся файлы.
        let fresh = work_root.join(format!("{stem}.new.arx"));
        let _ = fs::remove_file(&fresh);
        {
            // Сборка — самая прожорливая по памяти часть: см. [`CREATE_LIMIT`].
            let _slot = create_gate.enter();
            run_arx(&[
                "create",
                "-f",
                "-c",
                ARCHIVE_COMPRESSION,
                "--dir-as-root",
                "-o",
                &fresh.to_string_lossy(),
                &work.to_string_lossy(),
            ])?;
        }
        if !fresh.is_file() || fs::metadata(&fresh)?.len() == 0 {
            bail!("arx create не создал архив");
        }
        let mut expected: Vec<String> = fs::read_dir(&work)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        expected.sort();
        let mut got = list_archive(&fresh)?;
        got.sort();
        if got != expected {
            bail!(
                "новый архив не совпал с содержимым каталога: ожидалось {} файлов, в архиве {}",
                expected.len(),
                got.len()
            );
        }

        // Подмена: старый уводим в сторону, новый ставим на его место.
        // Копию кладём рядом с архивом (а не во временный каталог): если
        // процесс убьют ровно между двумя `rename`, её не снесёт уборка
        // `tmp/`, и следующий запуск вернёт архив на место.
        let backup = archives_dir.join(format!("{stem}.arx.old"));
        let _ = fs::remove_file(&backup);
        fs::rename(&source, &backup).context("не удалось отложить старый архив")?;
        if let Err(err) = fs::rename(&fresh, &source) {
            // Возвращаем как было — библиотека не должна остаться без архива.
            let _ = fs::rename(&backup, &source);
            return Err(err).context("не удалось поставить новый архив на место");
        }
        let _ = fs::remove_file(&backup);
        // Если в архиве не осталось ни одной книги, ему нечего делать в
        // `data/archives/`: уводим и его — пустой, без единой записи.
        if expected.is_empty() {
            let empty = dest.join("archives").join(format!("{stem}.arx"));
            let _ = fs::remove_file(&empty);
            if fs::rename(&source, &empty).is_err() {
                let _ = fs::remove_file(&source);
            }
        }
        Ok(Rebuild::Rebuilt {
            moved: ids.len(),
            bytes,
            ids,
        })
    })();
    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_file(work_root.join(format!("{stem}.new.arx")));
    let _ = fs::remove_file(archives_dir.join(format!("{stem}.arx.old")));
    result
}

/// Вернуть на место архивы, оставшиеся «отложенными» после обрыва.
///
/// Файл `X.arx.old` рядом с `X.arx` означает, что подмена не доехала до
/// конца: либо архив ещё не поставлен на место (тогда возвращаем), либо всё
/// уже хорошо (тогда просто убираем копию).
fn recover_archives(archives_dir: &Path) -> Result<usize> {
    let mut restored = 0usize;
    for entry in fs::read_dir(archives_dir)
        .with_context(|| format!("не удалось прочитать {}", archives_dir.display()))?
    {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".arx.old") else {
            continue;
        };
        let target = archives_dir.join(format!("{stem}.arx"));
        if target.exists() {
            let _ = fs::remove_file(&path);
        } else {
            fs::rename(&path, &target)
                .with_context(|| format!("не удалось вернуть {}", target.display()))?;
            restored += 1;
        }
    }
    Ok(restored)
}

/// Записи архива (`arx list` печатает по одной на строку).
fn list_archive(archive: &Path) -> Result<Vec<String>> {
    let output = Command::new("arx")
        .arg("list")
        .arg(archive)
        .output()
        .with_context(|| format!("не удалось запустить `arx list {}`", archive.display()))?;
    if !output.status.success() {
        bail!(
            "arx list {} завершился с ошибкой: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Запустить `arx` и упасть, если он вернул ошибку.
fn run_arx(args: &[&str]) -> Result<()> {
    let output = Command::new("arx")
        .args(args)
        .output()
        .with_context(|| format!("не удалось запустить `arx {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`arx {}` завершился с ошибкой: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Перенести файл, не падая, если он уже на месте (повторный запуск).
fn move_file(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) if to.is_file() => {
            // Уже перенесли в прошлый раз: исходник просто убираем.
            fs::remove_file(from).ok();
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            fs::copy(from, to)?;
            fs::remove_file(from)?;
            Ok(())
        }
        Err(err) => Err(err).with_context(|| format!("не удалось перенести {}", from.display())),
    }
}

/// Шарды метаданных по алфавиту: `metadata*.jsonl`.
fn metadata_shards(metadata_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = fs::read_dir(metadata_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    out.sort();
    Ok(out)
}

/// `id` из строки метаданных. Полный разбор JSON тут не нужен: строка
/// начинается с `{"id":…`, а книгам с неожиданным форматом ничего не будет —
/// их просто оставят в библиотеке.
fn line_id(line: &str) -> Option<u64> {
    let rest = line.trim_start().strip_prefix('{')?.trim_start();
    let rest = rest.strip_prefix("\"id\"")?.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Ключ файла книги — `archive` + `internal_path`.
fn file_key(archive: &str, internal: &str) -> String {
    format!("{archive}\u{1}{internal}")
}

/// `archive` + `internal_path` из строки метаданных (в том же виде, что
/// [`file_key`]).
///
/// Нужен потому, что id в библиотеке не уникален: одна и та же книга лежит в
/// двух архивах с одинаковым id, и «вынести по id» унесло бы вместе с
/// дубликатом и ту копию, которую решили оставить. Поля `archive` и
/// `internal_path` идут в строке до `details`, поэтому берём первое вхождение
/// и не рискуем поймать их внутри аннотации.
fn line_file(line: &str) -> Option<String> {
    let archive = json_string(line, "\"archive\":\"")?;
    let internal = json_string(line, "\"internal_path\":\"")?;
    Some(file_key(&archive, &internal))
}

/// Значение строкового поля JSON по его началу (кавычки экранировать не нужно:
/// в путях архива их не бывает).
fn json_string(line: &str, key: &str) -> Option<String> {
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Дописать строки в файл (`append`) или создать его.
fn write_lines(path: &Path, lines: &[&str], append: bool) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .with_context(|| format!("не удалось открыть {}", path.display()))?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// Заменить файл новым содержимым (атомарно: сначала временный, потом rename).
fn replace_lines(path: &Path, lines: &[&str]) -> Result<()> {
    let tmp = path.with_extension("jsonl.new");
    write_lines(&tmp, lines, false)?;
    fs::rename(&tmp, path)
        .with_context(|| format!("не удалось заменить {}", path.display()))?;
    Ok(())
}

/// Копия каталога — нужна только если `data/` и `dublicate/` на разных дисках.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Метка времени для имени старого индекса.
fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{seconds}")
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_reads_header_and_rows() {
        let dir = std::env::temp_dir().join("library_gui_test_plan");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plan.tsv");
        fs::write(
            &path,
            "id\tarchive\tinternal_path\tcover\tgroup_id\ttitle\tauthor\tyear\tsize\tkeeper_id\treason\n\
             7\t1-9.arx\t7.epub\t1-9/7.avif\tg000001\tКнига\tАвтор\t2001\t123\t9\tisbn\n\
             8\t1-9.arx\t8.epub\t\tg000001\tКнига\tАвтор\t\t456\t9\tcover\n",
        )
        .unwrap();
        let plan = Plan::load(&path).unwrap();
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.items[0].id, 7);
        assert_eq!(plan.items[0].cover.as_deref(), Some("1-9/7.avif"));
        assert_eq!(plan.items[0].size, 123);
        assert_eq!(plan.items[1].cover, None);
        assert_eq!(plan.summary(), (2, 1, 579));
        assert_eq!(plan.by_archive()["1-9.arx"].len(), 2);
    }

    #[test]
    fn plan_rejects_foreign_file() {
        let dir = std::env::temp_dir().join("library_gui_test_plan_bad");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plan.tsv");
        fs::write(&path, "id,archive\n1,2\n").unwrap();
        let err = Plan::load(&path).unwrap_err().to_string();
        assert!(err.contains("колонки"), "неожиданная ошибка: {err}");
    }

    #[test]
    fn line_id_survives_odd_lines() {
        assert_eq!(line_id(r#"{"id":42,"title":"x"}"#), Some(42));
        assert_eq!(line_id(r#"  { "id" : 7 }"#), Some(7));
        assert_eq!(line_id("не json"), None);
        assert_eq!(line_id(r#"{"title":"без id"}"#), None);
    }

    #[test]
    fn line_file_reads_top_level_fields_only() {
        let line = r#"{"id":1,"title":"a","archive":"10-19.arx","internal_path":"1.epub","details":{"annotation":"тут \"archive\":\"подделка\""}}"#;
        assert_eq!(line_file(line).as_deref(), Some("10-19.arx\u{1}1.epub"));
        assert_eq!(line_file(r#"{"id":1}"#), None);
    }

    #[test]
    fn metadata_round_trip_keeps_only_wanted() {
        let dir = std::env::temp_dir().join("library_gui_test_meta");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("metadata")).unwrap();
        let shard = dir.join("metadata").join("1-9.jsonl");
        fs::write(
            &shard,
            "{\"id\":1,\"title\":\"a\"}\n{\"id\":2,\"title\":\"b\"}\n{\"id\":3,\"title\":\"c\"}\n",
        )
        .unwrap();

        let remove: HashSet<u64> = [2u64].into_iter().collect();
        let text = fs::read_to_string(&shard).unwrap();
        let kept: Vec<&str> = text
            .lines()
            .filter(|line| !line_id(line).is_some_and(|id| remove.contains(&id)))
            .collect();
        replace_lines(&shard, &kept).unwrap();

        let after = fs::read_to_string(&shard).unwrap();
        assert!(after.contains("\"id\":1") && after.contains("\"id\":3"));
        assert!(!after.contains("\"id\":2"));
        assert_eq!(metadata_shards(&dir.join("metadata")).unwrap().len(), 1);
    }
}

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::db::CoverFilter;

    /// Есть ли `arx` в системе: без него пересобирать архивы нечем.
    fn have_arx() -> bool {
        Command::new("arx")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// Библиотека из трёх книг в одном архиве — минимальный стенд.
    fn make_library(root: &Path) -> PathBuf {
        let data = root.join("data");
        for sub in [
            "archives",
            "metadata",
            "covers/1-9",
            "index",
            "tmp",
            "cache",
        ] {
            fs::create_dir_all(data.join(sub)).unwrap();
        }
        let stage = root.join("stage");
        fs::create_dir_all(&stage).unwrap();
        for id in [1u64, 2, 3] {
            fs::write(stage.join(format!("{id}.epub")), format!("book-{id}")).unwrap();
        }
        run_arx(&[
            "create",
            "-f",
            "-c",
            "zstd=1",
            "--dir-as-root",
            "-o",
            &data.join("archives").join("1-9.arx").to_string_lossy(),
            &stage.to_string_lossy(),
        ])
        .unwrap();

        let mut shard = String::new();
        for id in [1u64, 2, 3] {
            shard.push_str(&format!(
                "{{\"id\":{id},\"title\":\"Книга {id}\",\"author\":\"Автор\",\
                 \"genre_codes\":[],\"archive\":\"1-9.arx\",\"internal_path\":\"{id}.epub\",\
                 \"cover\":{},\"details\":{{}},\"size\":7}}\n",
                if id == 2 {
                    "\"1-9/2.avif\"".to_string()
                } else {
                    "null".to_string()
                }
            ));
        }
        fs::write(data.join("metadata").join("1-9.jsonl"), shard).unwrap();
        fs::write(data.join("covers").join("1-9").join("2.avif"), b"cover").unwrap();
        data
    }

    fn plan_removing_two(root: &Path) -> Plan {
        let path = root.join("dedup_out").join("plan.tsv");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "id\tarchive\tinternal_path\tcover\tgroup_id\ttitle\tauthor\tyear\tsize\tkeeper_id\treason\n\
             2\t1-9.arx\t2.epub\t1-9/2.avif\tg000001\tКнига 2\tАвтор\t2020\t7\t1\tisbn\n",
        )
        .unwrap();
        Plan::load(&path).unwrap()
    }

    /// Одинаковый id у двух книг не должен уносить обе: вынос решается по
    /// файлу, иначе keeper с тем же id пропадал из библиотеки.
    #[test]
    fn same_id_in_two_archives_keeps_the_keeper() {
        if !have_arx() {
            eprintln!("arx не найден — тест пропущен");
            return;
        }
        let root = std::env::temp_dir().join("library_gui_test_same_id");
        let _ = fs::remove_dir_all(&root);
        let data = root.join("data");
        for sub in ["archives", "metadata", "covers", "index", "tmp", "cache"] {
            fs::create_dir_all(data.join(sub)).unwrap();
        }
        for (stem, books) in [("1-9", vec!["5.epub"]), ("10-19", vec!["5.epub", "7.epub"])] {
            let stage = root.join(format!("stage-{stem}"));
            fs::create_dir_all(&stage).unwrap();
            for book in books {
                fs::write(stage.join(book), format!("book-{book}")).unwrap();
            }
            run_arx(&[
                "create",
                "-f",
                "-c",
                "zstd=1",
                "--dir-as-root",
                "-o",
                &data.join("archives").join(format!("{stem}.arx")).to_string_lossy(),
                &stage.to_string_lossy(),
            ])
            .unwrap();
        }
        fs::write(
            data.join("metadata").join("1-19.jsonl"),
            concat!(
                "{\"id\":5,\"title\":\"Книга\",\"archive\":\"1-9.arx\",\"internal_path\":\"5.epub\",\"details\":{},\"size\":8}\n",
                "{\"id\":5,\"title\":\"Книга\",\"archive\":\"10-19.arx\",\"internal_path\":\"5.epub\",\"details\":{},\"size\":8}\n",
                "{\"id\":7,\"title\":\"Книга\",\"archive\":\"10-19.arx\",\"internal_path\":\"7.epub\",\"details\":{},\"size\":8}\n",
            ),
        )
        .unwrap();
        let plan_path = root.join("dedup_out").join("plan.tsv");
        fs::create_dir_all(plan_path.parent().unwrap()).unwrap();
        fs::write(
            &plan_path,
            "id\tarchive\tinternal_path\tcover\tgroup_id\ttitle\tauthor\tyear\tsize\tkeeper_id\treason\n\
             5\t1-9.arx\t5.epub\t\tg000001\tКнига\t\t\t8\t5\tid\n\
             7\t10-19.arx\t7.epub\t\tg000001\tКнига\t\t\t8\t5\tid\n",
        )
        .unwrap();
        let plan = Plan::load(&plan_path).unwrap();

        let outcome = execute(&plan, &data, 2, &|_| {}).unwrap();
        assert_eq!(outcome.metadata_dropped, 2);
        let left = fs::read_to_string(data.join("metadata").join("1-19.jsonl")).unwrap();
        let lines: Vec<&str> = left.lines().filter(|line| !line.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "keeper должен остаться: {left}");
        assert!(lines[0].contains("\"archive\":\"10-19.arx\""), "{left}");
    }

    /// Полный проход: книга уезжает из архива, обложек, метаданных; индекс
    /// пересобирается и находит только оставшееся.
    #[test]
    fn execute_takes_duplicates_out_of_the_whole_library() {        if !have_arx() {
            eprintln!("arx не найден — тест пропущен");
            return;
        }
        let root = std::env::temp_dir().join("library_gui_test_dedup_e2e");
        let _ = fs::remove_dir_all(&root);
        let data = make_library(&root);
        let plan = plan_removing_two(&root);
        assert_eq!(plan.summary(), (1, 1, 7));

        let seen = std::sync::Mutex::new(Vec::new());
        let report = |progress: Progress| seen.lock().unwrap().push(progress.step);
        let outcome = execute(&plan, &data, 2, &report).unwrap();

        assert_eq!(outcome.books, 1);
        assert_eq!(outcome.covers_moved, 1);
        assert_eq!(outcome.metadata_dropped, 1);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(seen.lock().unwrap().contains(&Step::Index));

        // Книга уехала из архива, но не потерялась: она в dublicate/.
        let dest = dublicate_dir(&data);
        assert!(dest.join("archives/1-9/2.epub").is_file());
        assert!(dest.join("covers/1-9/2.avif").is_file());
        let gone = fs::read_to_string(dest.join("metadata/1-9.jsonl")).unwrap();
        assert!(gone.contains("\"id\":2"), "строка книги должна переехать: {gone}");

        let left = list_archive(&data.join("archives").join("1-9.arx")).unwrap();
        let mut left = left;
        left.sort();
        assert_eq!(left, vec!["1.epub".to_string(), "3.epub".to_string()]);

        // В библиотеке книга исчезла вместе с обложкой и из индекса тоже.
        let library = Library::open(&data).unwrap();
        assert_eq!(library.books.len(), 2);
        assert!(!library.books.contains_key(&2));
        assert!(!data.join("covers").join("1-9").join("2.avif").exists());
        let found = library.search("Книга", None, None, false, CoverFilter::All, 0, 10).unwrap();
        let ids: Vec<u64> = found.books.iter().map(|b| b.id).collect();
        assert!(!ids.contains(&2), "вынесенная книга всё ещё ищется: {ids:?}");
    }

    /// Повторный запуск ничего не портит: книги уже вынесены, план устарел.
    #[test]
    fn second_run_is_a_no_op() {
        if !have_arx() {
            return;
        }
        let root = std::env::temp_dir().join("library_gui_test_dedup_again");
        let _ = fs::remove_dir_all(&root);
        let data = make_library(&root);
        let plan = plan_removing_two(&root);
        let quiet = |_: Progress| {};
        execute(&plan, &data, 2, &quiet).unwrap();

        let again = execute(&plan, &data, 2, &quiet).unwrap();
        // Книга уже вынесена раньше: из библиотеки в этот прогон не убрано
        // ничего, но файл на месте — в dublicate/.
        assert_eq!(again.books, 0);
        assert_eq!(again.metadata_dropped, 0);
        assert_eq!(again.already_gone, 1);
        assert!(again.errors.is_empty(), "{:?}", again.errors);
        assert!(dublicate_dir(&data).join("archives/1-9/2.epub").is_file());
    }

    /// Обрыв между двумя `rename` не должен стоить архива.
    #[test]
    fn interrupted_swap_is_healed() {
        let root = std::env::temp_dir().join("library_gui_test_dedup_heal");
        let _ = fs::remove_dir_all(&root);
        let archives = root.join("archives");
        fs::create_dir_all(&archives).unwrap();

        // Подмена не доехала: есть только отложенная копия.
        fs::write(archives.join("1-9.arx.old"), b"original").unwrap();
        assert_eq!(recover_archives(&archives).unwrap(), 1);
        assert_eq!(
            fs::read(archives.join("1-9.arx")).unwrap(),
            b"original",
            "архив должен вернуться на место"
        );
        assert!(!archives.join("1-9.arx.old").exists());

        // Подмена доехала: копия просто убирается, архив не трогаем.
        fs::write(archives.join("1-9.arx"), b"new").unwrap();
        fs::write(archives.join("1-9.arx.old"), b"original").unwrap();
        assert_eq!(recover_archives(&archives).unwrap(), 0);
        assert_eq!(fs::read(archives.join("1-9.arx")).unwrap(), b"new");
        assert!(!archives.join("1-9.arx.old").exists());

        let _ = fs::remove_dir_all(&root);
    }

    /// Все книги архива вынесены — пустой `.arx` не остаётся в библиотеке.
    #[test]
    fn empty_archive_leaves_the_library() {
        if !have_arx() {
            return;
        }
        let root = std::env::temp_dir().join("library_gui_test_dedup_empty");
        let _ = fs::remove_dir_all(&root);
        let data = make_library(&root);
        let path = root.join("dedup_out").join("plan.tsv");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "id\tarchive\tinternal_path\tcover\tgroup_id\ttitle\tauthor\tyear\tsize\tkeeper_id\treason\n\
             1\t1-9.arx\t1.epub\t\tg000001\tК\tА\t\t7\t1\tid\n\
             2\t1-9.arx\t2.epub\t\tg000001\tК\tА\t\t7\t1\tid\n\
             3\t1-9.arx\t3.epub\t\tg000001\tК\tА\t\t7\t1\tid\n",
        )
        .unwrap();
        let plan = Plan::load(&path).unwrap();
        let quiet = |_: Progress| {};
        let outcome = execute(&plan, &data, 1, &quiet).unwrap();
        assert_eq!(outcome.books, 3);
        assert!(!data.join("archives").join("1-9.arx").exists());
        assert!(dublicate_dir(&data).join("archives/1-9.arx").exists());
        let library = Library::open(&data).unwrap();
        assert!(library.books.is_empty());
    }
}
