#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""dedup.py — поиск дубликатов книг в локальной библиотеке library_gui.

Порядок работы:
  1) читает все metadata/*.jsonl;
  2) нормализует название, отдельно вытаскивает номер тома/части;
  3) группирует записи по (нормализованное название + том);
  4) классифицирует группу по названию, автору и году:
       generic   — «пустое» название («Рассказы», «Стихи», unknown): это разные
                   книги с одинаковым названием, дубликатами их считать нельзя
       exact     — один и тот же id или совпали название+автор+год+размер
       confirmed — совпали название+автор+год (или общий ISBN)
       probable  — совпали название+автор (год пуст или расходится)
       possible  — совпало только название
     Отдельным проходом (--phash) статус поднимается до exact_cover, если у
     копий совпали обложки; такие группы уже точно одна и та же книга.
  5) выбирает keeper в каждой группе;
  6) пишет dedup_out/groups.csv, dedup_out/records.csv и печатает сводку.

Только метаданные: архивы и обложки не читаются, ничего не считается по
содержимому файлов. Полный прогон — около минуты.

Данные (metadata/, archives/, covers/) только читаются. Пишем исключительно
в dedup_out/. Ничего не удаляется.

Запуск:
    python3 dedup.py                      # анализ всей библиотеки
    python3 dedup.py --limit 20000        # быстрый прогон на части библиотеки
    python3 dedup.py --phash              # дополнительно сравнить обложки (Pillow)
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import io
import json
import math
import multiprocessing as mp
import os
import re
import subprocess
import sys
import time
import unicodedata
import zipfile
from collections import Counter, defaultdict
from pathlib import Path

# --------------------------------------------------------------------------
# Имена полей в metadata/*.jsonl — меняются здесь, если схема поменяется
# --------------------------------------------------------------------------
F_ID = "id"
F_TITLE = "title"
F_AUTHOR = "author"
F_ARCHIVE = "archive"
F_INTERNAL = "internal_path"
F_COVER = "cover"
F_SIZE = "size"
F_DETAILS = "details"

D_AUTHORS = "authors"
D_YEAR = "year"
D_DATE = "date"
D_ISBN = "isbn"
D_SERIES = "series"
D_SERIES_NUMBER = "series_number"
D_DOC_ID = "doc_id"

# --------------------------------------------------------------------------
# Настройки по умолчанию
# --------------------------------------------------------------------------
DATA_DIR = Path("/home/anton/Downloads/library_gui/data")
OUT_DIR = Path("dedup_out")
METADATA_SUBDIR = "metadata"
ARCHIVES_SUBDIR = "archives"
COVERS_SUBDIR = "covers"

HASH_MODE = "none"          # none | docid | content | auto (content читает архивы!)
HASH_CACHE_NAME = "hashes.tsv"
PHASH_CACHE_NAME = "phash.tsv"
PHASH_MAX_DISTANCE = 5      # ≤5 бит И тот же автор → обложки считаем одинаковыми
PHASH_EXACT_DISTANCE = 0    # бит-в-бит одинаковая обложка — доказательство и без автора
PHASH_GENERIC_TITLES = 20   # один дизайн у стольких названий — не доказательство
ONLY_DUPLICATE_GROUPS = True  # писать в CSV только группы с копиями (copies ≥ 2)
USE_SERIES_NUMBER = True      # учитывать details.series_number как номер тома
JOBS = min(12, os.cpu_count() or 4)

# Группа считается «пустым названием» (рассказы, стихи, unknown…), если у неё
# столько копий, или если название короткое и копий тоже много. Такие группы
# остаются в отчёте, но не попадают в топы и в примеры на ручную проверку.
GENERIC_COPIES = 20          # столько копий — наверняка «пустое» название
GENERIC_SHORT_WORDS = 2      # …или столько слов в названии
GENERIC_SHORT_COPIES = 5     # при таком числе копий
GENERIC_TITLES = {"unknown", "без названия", "неизвестно", "неизвестная книга", "нет"}

STATUSES = ["generic", "possible", "probable", "confirmed", "exact", "exact_cover"]
STATUS_RANK = {s: i for i, s in enumerate(STATUSES)}

# --------------------------------------------------------------------------
# План выноса дубликатов (его читает кнопка «Убрать дубликаты» в приложении)
# --------------------------------------------------------------------------
PLAN_NAME = "plan.tsv"
# Насколько разными могут быть написания одного автора, чтобы копии с
# одинаковой обложкой всё ещё считались одной книгой. «Анджей Сапковский» и
# «Анджей Спаковский» — 2 правки на 16 символов (0.125), проходит; «Иван
# Петров» и «Сергей Смирнов» — 1.0, не проходит.
AUTHOR_CLOSE_RATIO = 0.18

# Откуда взялось «это одна и та же книга». Порядок = сила доказательства:
# первое сработавшее и попадает в plan.tsv как reason.
# «text» стоит сразу после id и хеша: это отпечаток самого текста книги
# (см. textdup.py), он сильнее любых метаданных и работает даже тогда, когда
# у двух копий разные название, автор, год и размер.
PLAN_REASONS = ["id", "hash", "text", "isbn", "size", "cover", "cover_near",
                "cover_missing", "edition", "author"]
PLAN_REASON_RANK = {r: i for i, r in enumerate(PLAN_REASONS)}

# --------------------------------------------------------------------------
# Отпечатки текста (dedup_out/text.tsv, считается скриптом textdup.py)
# --------------------------------------------------------------------------
TEXT_NAME = "text.tsv"
#: Жаккар по шинглам текста. Замер на 32 парах «одно название + один автор»
#: дал 0.949–1.000 у настоящих дубликатов (разные издания одной книги
#: расходятся на опечатки и вставки), у разных книг — единицы процентов.
TEXT_SAME = 0.8
#: Короче этого текст не доказательство: у двух разных брошюр вполне может
#: совпасть страница выходных данных, а вот 300 слов прозы — уже нет.
TEXT_MIN_WORDS = 300
#: Размеры копий одной книги отличаются на проценты; втрое — это уже сборник.
TEXT_SIZE_RATIO = 3.0
#: Больше этого числа книг у одного автора — это «Коллектив авторов» и прочие
#: сборники: сравнивать их все попарно бессмысленно, такие ловятся по названию.
TEXT_AUTHOR_CAP = 300

# --------------------------------------------------------------------------
# Нормализация
# --------------------------------------------------------------------------
_PUNCT = re.compile(r"[\W_]+", re.UNICODE)
_VOLUME_RE = re.compile(
    r"\b(?:том|volume|vol|часть|part|книга|book|выпуск|вып)\w{0,2}\.?\s*[№#]?\s*"
    r"(\d{1,3}|[ivxlcdm]{1,6})\b",
    re.IGNORECASE,
)
_PURE_NUMBER_RE = re.compile(r"^\s*[№#]?\s*(\d{1,3}|[ivxlcdm]{1,6})\s*\.?\s*$", re.IGNORECASE)
_ROMAN = {"i": 1, "v": 5, "x": 10, "l": 50, "c": 100, "d": 500, "m": 1000}


def fold(text: str) -> str:
    """Нижний регистр, ё→е, снятие диакритики, пунктуация → пробел."""
    s = (text or "").lower().replace("ё", "е")
    s = unicodedata.normalize("NFKD", s)
    s = "".join(ch for ch in s if not unicodedata.combining(ch))
    return " ".join(_PUNCT.sub(" ", s).split())


def roman_to_int(token: str) -> int:
    """'xii' → 12; 0, если это не римское число."""
    total, prev = 0, 0
    for ch in reversed(token.lower()):
        val = _ROMAN.get(ch)
        if val is None:
            return 0
        total += -val if val < prev else val
        prev = max(prev, val)
    return total


def volume_of(text: str) -> str:
    """Номер тома/части из строки: 'Том III' → '3', 'часть 2' → '2', иначе ''."""
    match = _VOLUME_RE.search(text or "")
    if not match:
        return ""
    token = match.group(1)
    if token.isdigit():
        return str(int(token))
    value = roman_to_int(token)
    return str(value) if 0 < value <= 200 else ""


def series_volume(series_number: str) -> str:
    """Номер тома из details.series_number, но только если это чистый номер.

    Значения вида «1996 11-12» или «Часть I. Подводные лодки» номером тома не
    считаем: у копий одной книги series_number часто пуст или мусорный, и
    лишнее дробление ключа прячет настоящие дубликаты.
    """
    pure = _PURE_NUMBER_RE.match(series_number or "")
    if not pure:
        return ""
    token = pure.group(1)
    if token.isdigit():
        return str(int(token))
    value = roman_to_int(token)
    return str(value) if 0 < value <= 200 else ""


def normalize_author(text: str) -> str:
    return fold(text)


def _isbn10_to_13(digits: str) -> str:
    """ISBN-10 → ISBN-13 (префикс 978), если контрольная цифра верна."""
    body = "978" + digits[:9]
    total = sum((1 if i % 2 == 0 else 3) * int(ch) for i, ch in enumerate(body))
    check = (10 - total % 10) % 10
    return body + str(check)


def _isbn10_ok(digits: str) -> bool:
    total = sum((10 - i) * (10 if ch == "X" else int(ch)) for i, ch in enumerate(digits))
    return total % 11 == 0


def _isbn13_ok(digits: str) -> bool:
    total = sum((1 if i % 2 == 0 else 3) * int(ch) for i, ch in enumerate(digits[:12]))
    return (10 - total % 10) % 10 == int(digits[12])


def parse_isbns(raw: str) -> set[str]:
    """Все ISBN из поля: бывает «5-88196-810-7; 5-7390-0336-9», «ISBN …», ISSN.

    Возвращаем множество из 10- и 13-значных форм; ISBN-10 дополнительно
    приводится к ISBN-13, чтобы «5-699-23805-7» и «978-5-699-23805-2» сошлись.
    Заглушки вида «X-XXX-XXXXX-X» (встречаются в шаблонах FB2) отбрасываем.
    """
    out: set[str] = set()
    for token in re.findall(r"[0-9Xx][0-9Xx\-\s]{7,}[0-9Xx]", raw or ""):
        digits = re.sub(r"[^0-9Xx]", "", token).upper()
        numbers = re.sub(r"[^0-9]", "", digits)
        if len(digits) == 10 and len(numbers) >= 9 and digits[:9].isdigit():
            out.add(digits)
            if _isbn10_ok(digits):
                out.add(_isbn10_to_13(digits))
        elif len(digits) == 13 and len(numbers) == 13:
            out.add(digits)
    return out


_YEAR_RE = re.compile(r"^(1\d{3}|20\d{2}|21\d{2})$")
_YEAR_IN_DATE_RE = re.compile(r"\b(1\d{3}|20\d{2}|21\d{2})\b")


def normalize_year(year: str, date: str) -> str:
    """Год: строго 4 цифры из year, иначе первая разумная дата из date."""
    year = (year or "").strip()
    if _YEAR_RE.match(year):
        return year
    match = _YEAR_IN_DATE_RE.search(date or "")
    return match.group(1) if match else ""


# --------------------------------------------------------------------------
# Запись о книге
# --------------------------------------------------------------------------
class Rec:
    """Одна строка metadata. Атрибуты короткие: записей полмиллиона."""

    __slots__ = (
        "id", "title", "title_norm", "volume", "volume_title", "volume_series",
        "author", "author_norm", "year", "isbns", "hash", "size", "cover",
        "archive", "internal", "src_file", "src_line", "generic", "keeper", "phash",
    )

    def __init__(self, raw, src_file, src_line, hash_value):
        details = raw.get(F_DETAILS) or {}
        self.id = raw.get(F_ID)
        self.title = (raw.get(F_TITLE) or "").strip()
        self.title_norm = fold(self.title)
        self.volume_title = volume_of(self.title)
        self.volume_series = series_volume(details.get(D_SERIES_NUMBER))
        self.volume = self.volume_title
        self.author = (raw.get(F_AUTHOR) or "").strip()
        self.author_norm = normalize_author(self.author)
        self.year = normalize_year(details.get(D_YEAR), details.get(D_DATE))
        self.isbns = parse_isbns(details.get(D_ISBN))
        self.hash = hash_value
        self.size = int(raw.get(F_SIZE) or 0)
        self.cover = raw.get(F_COVER)
        self.archive = raw.get(F_ARCHIVE) or ""
        self.internal = raw.get(F_INTERNAL) or ""
        self.src_file = src_file
        self.src_line = src_line
        self.generic = False
        self.keeper = False
        self.phash = None

    @property
    def key(self) -> tuple[str, str]:
        return (self.title_norm, self.volume)

    @property
    def file(self) -> str:
        return f"{self.archive}/{self.internal}"

    @property
    def has_cover(self) -> bool:
        return bool(self.cover)


# --------------------------------------------------------------------------
# Чтение metadata и хеши
# --------------------------------------------------------------------------
def metadata_files(data_dir: Path) -> list[Path]:
    return sorted((data_dir / METADATA_SUBDIR).glob("*.jsonl"))


def iter_raw_records(data_dir: Path):
    """Поток (имя_шарда, номер_строки, объект) по всем metadata/*.jsonl."""
    for path in metadata_files(data_dir):
        with open(path, encoding="utf-8") as handle:
            for number, line in enumerate(handle, 1):
                line = line.strip()
                if line:
                    yield path.name, number, json.loads(line)


def load_hash_cache(path: Path) -> dict[str, str]:
    """Кэш content_hash: «архив/внутренний_путь» → sha256."""
    cache: dict[str, str] = {}
    if not path.exists():
        return cache
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) == 3 and parts[2]:
                cache[f"{parts[0]}/{parts[1]}"] = parts[2]
    return cache


def content_hash(blob: bytes) -> str:
    """Хеш содержимого EPUB без случайных полей.

    В content.opf при каждой сборке заново генерируются urn:uuid и
    dcterms:modified, поэтому побайтовый sha256 у копий одной книги разный.
    Маскируем их — тогда хеш описывает именно содержимое книги.
    """
    digest = hashlib.sha256()
    with zipfile.ZipFile(io.BytesIO(blob)) as archive:
        for name in sorted(archive.namelist()):
            data = archive.read(name)
            if name == "content.opf":
                text = data.decode("utf-8", "replace")
                text = re.sub(r"urn:uuid:[0-9a-fA-F-]{36}", "urn:uuid:BOOK", text)
                text = re.sub(r'(dcterms:modified">)[^<]*', r"\1MOD", text)
                data = text.encode()
            digest.update(name.encode())
            digest.update(b"\0")
            digest.update(hashlib.sha256(data).digest())
    return digest.hexdigest()


def _hash_job(job: tuple[str, str]) -> tuple[str, str, str]:
    """Одна книга: arx dump → content_hash. Выполняется в дочернем процессе."""
    archive_path, entry = job
    try:
        done = subprocess.run(
            ["arx", "dump", archive_path, entry],
            capture_output=True,
            check=False,
        )
        if done.returncode != 0 or not done.stdout:
            return archive_path, entry, ""
        return archive_path, entry, content_hash(done.stdout)
    except Exception:
        return archive_path, entry, ""


def collect_entries(data_dir: Path, limit: int | None) -> list[tuple[str, str]]:
    """Список (путь_архива, запись) для всех книг библиотеки, без повторов."""
    seen: set[tuple[str, str]] = set()
    jobs: list[tuple[str, str]] = []
    for _, _, raw in iter_raw_records(data_dir):
        archive = raw.get(F_ARCHIVE) or ""
        internal = raw.get(F_INTERNAL) or ""
        if not archive or not internal:
            continue
        key = (archive, internal)
        if key in seen:
            continue
        seen.add(key)
        jobs.append((str(data_dir / ARCHIVES_SUBDIR / archive), internal))
        if limit and len(jobs) >= limit:
            break
    return jobs


def build_hash_cache(data_dir: Path, cache_path: Path, limit: int | None, jobs: int) -> int:
    """Считает content_hash всех книг и дописывает их в кэш (можно прерывать)."""
    cache_path.parent.mkdir(parents=True, exist_ok=True)
    known = load_hash_cache(cache_path)
    todo = [
        job for job in collect_entries(data_dir, limit)
        if f"{Path(job[0]).name}/{job[1]}" not in known
    ]
    print(f"всего в кэше: {len(known)}, осталось посчитать: {len(todo)}", flush=True)
    if not todo:
        return len(known)

    started = time.time()
    done_count = 0
    failed = 0
    with open(cache_path, "a", encoding="utf-8") as out, mp.Pool(jobs) as pool:
        for archive_path, entry, digest in pool.imap_unordered(_hash_job, todo, chunksize=32):
            out.write(f"{Path(archive_path).name}\t{entry}\t{digest}\n")
            done_count += 1
            if not digest:
                failed += 1
            if done_count % 2000 == 0:
                out.flush()
                rate = done_count / max(time.time() - started, 1e-6)
                left = (len(todo) - done_count) / max(rate, 1e-6)
                print(
                    f"  {done_count}/{len(todo)}  {rate:.0f} кн/с  осталось ~{left/60:.1f} мин",
                    flush=True,
                )
    print(f"готово: {done_count}, не удалось прочитать: {failed}", flush=True)
    return len(known) + done_count


# --------------------------------------------------------------------------
# Группировка и классификация
# --------------------------------------------------------------------------
def load_groups(data_dir: Path, hash_mode: str, limit: int | None):
    """Читает metadata и раскладывает записи по ключу (название + том).

    Сначала группируем по названию, потом решаем про том. Номер из названия
    («Война и мир. Том 2») всегда разводит книги по разным группам, а
    series_number — только если он заполнен у всех копий этого названия.
    Иначе пустой series_number у одной копии разрезал бы пару дубликатов.
    """
    cache = load_hash_cache(OUT_DIR / HASH_CACHE_NAME) if hash_mode in ("auto", "content") else {}
    if hash_mode == "content" and not cache:
        print("кэш хешей пуст — считаю content_hash (это долго, см. --build-hashes)", flush=True)
        build_hash_cache(data_dir, OUT_DIR / HASH_CACHE_NAME, None, JOBS)
        cache = load_hash_cache(OUT_DIR / HASH_CACHE_NAME)

    by_title: dict[str, list[Rec]] = defaultdict(list)
    total = 0
    for src_file, src_line, raw in iter_raw_records(data_dir):
        details = raw.get(F_DETAILS) or {}
        if hash_mode == "docid":
            value = (details.get(D_DOC_ID) or "").strip()
        elif hash_mode in ("content", "auto") and cache:
            value = cache.get(f"{raw.get(F_ARCHIVE)}/{raw.get(F_INTERNAL)}", "")
        else:
            value = ""
        rec = Rec(raw, src_file, src_line, value)
        by_title[rec.title_norm].append(rec)
        total += 1
        if limit and total >= limit:
            break

    groups: dict[tuple[str, str], list[Rec]] = defaultdict(list)
    split_by_series = 0
    for title_norm, records in by_title.items():
        use_series = (
            USE_SERIES_NUMBER
            and not records[0].volume_title
            and all(rec.volume_series for rec in records)
        )
        before = len(groups)
        for rec in records:
            rec.volume = rec.volume_series if use_series else rec.volume_title
            groups[rec.key].append(rec)
        if use_series and len(groups) - before > 1:
            split_by_series += 1
    print(f"названий: {len(by_title)}, разведено по series_number: {split_by_series}")
    return groups, total, bool(cache)


def classify(records: list[Rec], has_hash: bool) -> str:
    """Статус группы = самый сильный признак, который нашёлся хотя бы у пары.

    Внутри группы название и том уже совпадают, поэтому сравниваем только
    автора, год, размер и ISBN. Считаем не пары, а «есть ли вообще два
    совпадающих значения» — результат тот же, но линейно, а не n².
    """
    if has_hash:
        counts = Counter(rec.hash for rec in records if rec.hash)
        if any(count > 1 for count in counts.values()):
            return "exact"
    counts = Counter(rec.id for rec in records)
    if any(count > 1 for count in counts.values()):
        return "exact"
    # название + автор + год + размер — практически один и тот же файл
    counts = Counter((rec.author_norm, rec.year, rec.size) for rec in records if rec.author_norm)
    if any(count > 1 for count in counts.values()):
        return "exact"
    # название + автор + год (год должен быть известен)
    counts = Counter((rec.author_norm, rec.year) for rec in records
                     if rec.author_norm and rec.year)
    if any(count > 1 for count in counts.values()):
        return "confirmed"
    # общий ISBN подтверждает даже при иначе записанном авторе
    isbns = Counter(isbn for rec in records for isbn in rec.isbns)
    if any(count > 1 for count in isbns.values()):
        return "confirmed"
    # название + автор
    counts = Counter(rec.author_norm for rec in records if rec.author_norm)
    if any(count > 1 for count in counts.values()):
        return "probable"
    return "possible"


def is_generic(title_norm: str, copies: int) -> bool:
    """«Пустое» название: одно-два слова у многих книг («рассказы», «стихи»)."""
    if copies >= GENERIC_COPIES or title_norm in GENERIC_TITLES:
        return True
    return copies >= GENERIC_SHORT_COPIES and len(title_norm.split()) <= GENERIC_SHORT_WORDS


def keeper_rank(rec: Rec) -> tuple:
    """Приоритет keeper: обложка > хеш > ISBN > автор > больший размер."""
    return (
        rec.has_cover,
        bool(rec.hash),
        bool(rec.isbns),
        bool(rec.author_norm),
        rec.size,
        -(rec.id or 0),
    )


# --------------------------------------------------------------------------
# Отпечатки текста (dedup_out/text.tsv, считается скриптом textdup.py)
# --------------------------------------------------------------------------
def load_text_cache(path: Path) -> dict:
    """`text.tsv` → {id: (слов, хеши шинглов)}. Файла может и не быть."""
    cache: dict = {}
    if not path.exists():
        return cache
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) != 3:
                continue
            try:
                book_id = int(parts[0])
                words = int(parts[1])
                hashes = frozenset(int(value, 16) for value in parts[2].split(",") if value)
            except ValueError:
                continue
            if hashes:
                cache[book_id] = (words, hashes)
    return cache


def same_text(text: dict, one, other) -> bool:
    """Один и тот же текст у двух записей (с точностью до опечаток).

    Жаккар по bottom-k отпечаткам: |пересечение| / |объединение|. Именно
    Жаккар, а не вложенность: рассказ целиком входит в сборник, но выносить
    сборник за это нельзя, а у настоящих копий совпадает почти всё.
    """
    first = text.get(one)
    second = text.get(other)
    if first is None or second is None:
        return False
    if first[0] < TEXT_MIN_WORDS or second[0] < TEXT_MIN_WORDS:
        return False
    shared = len(first[1] & second[1])
    return shared / max(1, len(first[1] | second[1])) >= TEXT_SAME


def _union_by_cover(flat: list, union) -> int:
    """Копия без обложки при наличии копии с обложкой — тот же дубликат.

    В библиотеке полно пар «одно название + один автор», где у одной копии
    обложка есть, а у другой нет. Это один и тот же файл, приехавший из разных
    источников: у одного из них обложку вытащили, у другого нет. Текстовый слой
    такие пары не берёт, если текст разошёлся (другое издание, распознанный
    скан, другой перевод), а в списке библиотеки это две одинаковые строки.

    Поэтому: если в группе «название + автор» есть хоть одна копия с обложкой,
    все копии без обложки считаются дубликатами. Хранитель выбирается как
    обычно — с обложкой, так что книга из библиотеки не исчезает.
    """
    buckets: dict[tuple[str, str], list[int]] = defaultdict(list)
    for index, rec in enumerate(flat):
        if rec.author_norm:
            buckets[(rec.title_norm, rec.author_norm)].append(index)

    pairs = 0
    for bucket in buckets.values():
        if len(bucket) < 2 or len(bucket) > TEXT_AUTHOR_CAP:
            continue
        covered = [index for index in bucket if flat[index].cover]
        if not covered:
            continue
        anchor = covered[0]
        for index in bucket:
            if flat[index].cover:
                continue
            union(anchor, index, "cover_missing")
            pairs += 1
    return pairs


def _union_by_text(flat: list, text: dict, union) -> int:
    """Скрепить книги с одинаковым текстом, даже если метаданные разные.

    Все полмиллиона записей попарно не сравнить, поэтому кандидаты берутся
    по метаданным: книги одного названия (там бывает и разный автор) и книги
    одного автора (там ловятся разные названия одной книги — «Тайна замка
    Горсорп-Грэйндж» и «Привидения в замке Горсторп-Грэйндж»). Внутри автора
    записи сортируются по размеру и сравниваются только с теми, что не втрое
    больше: у копий размеры близкие, а у авторов-сборников («Коллектив
    авторов») книг столько, что попарно их сравнивать нельзя.
    """
    by_title: dict[str, list[int]] = defaultdict(list)
    by_author: dict[str, list[int]] = defaultdict(list)
    for index, rec in enumerate(flat):
        by_title[rec.title_norm].append(index)
        if rec.author_norm:
            by_author[rec.author_norm].append(index)

    pairs = 0
    # Одно название: записей мало, сравниваем все пары — размер тут не указ,
    # у одной и той же книги он отличается и в десять раз (сборник против
    # рассказа), и решает именно текст.
    for bucket in by_title.values():
        if len(bucket) < 2:
            continue
        for position, one in enumerate(bucket):
            for other in bucket[position + 1:]:
                if same_text(text, flat[one].id, flat[other].id):
                    union(one, other, "text")
                    pairs += 1
    # Один автор, разные названия: тут пары экономим по размеру.
    for bucket in by_author.values():
        if len(bucket) < 2 or len(bucket) > TEXT_AUTHOR_CAP:
            continue
        bucket.sort(key=lambda index: flat[index].size)
        for position, one in enumerate(bucket):
            limit = (flat[one].size or 1) * TEXT_SIZE_RATIO
            for other in bucket[position + 1:]:
                if flat[other].size > limit:
                    break
                if same_text(text, flat[one].id, flat[other].id):
                    union(one, other, "text")
                    pairs += 1
    return pairs


# --------------------------------------------------------------------------
# pHash по обложкам
# --------------------------------------------------------------------------
_N = 32
_DCT_COS = [[math.cos(math.pi * (2 * x + 1) * u / (2 * _N)) for x in range(_N)] for u in range(8)]


def phash_file(path: str) -> tuple[str, str]:
    """pHash обложки: 32×32 серый → DCT → 8×8 без DC → медиана → 64 бита."""
    try:
        from PIL import Image  # опциональная зависимость

        with Image.open(path) as image:
            image = image.convert("L").resize((_N, _N), Image.LANCZOS)
            pixels = list(image.getdata())
    except Exception:
        return path, ""
    values = []
    for u in range(8):
        cos_u = _DCT_COS[u]
        for v in range(8):
            cos_v = _DCT_COS[v]
            total = 0.0
            for y in range(_N):
                weight = cos_v[y]
                if weight == 0.0:
                    continue
                row = pixels[y * _N:(y + 1) * _N]
                acc = 0.0
                for x in range(_N):
                    acc += row[x] * cos_u[x]
                total += acc * weight
            values.append(total)
    average = sum(values[1:]) / (len(values) - 1)
    bits = 0
    for index, value in enumerate(values):
        if index and value > average:
            bits |= 1 << index
    return path, format(bits, "016x")


def load_phash_cache(path: Path) -> dict[str, int]:
    cache: dict[str, int] = {}
    if not path.exists():
        return cache
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) == 2 and parts[1]:
                cache[parts[0]] = int(parts[1], 16)
    return cache


def phash_pass(data_dir: Path, groups: dict, jobs: int) -> dict:
    """Считает pHash обложек в группах-кандидатах.

    Возвращает {ключ_группы: 'exact_cover'} для групп, где нашлась пара
    обложек с hamming ≤ PHASH_MAX_DISTANCE.
    """
    cache_path = OUT_DIR / PHASH_CACHE_NAME
    cache = load_phash_cache(cache_path)
    candidates: list[tuple[str, str]] = []  # (путь к обложке, ключ кэша)
    for records in groups.values():
        if len(records) < 2:
            continue
        covered = [rec for rec in records if rec.cover]
        if len(covered) < 2:
            continue
        for rec in covered:
            rel = rec.cover
            if rel not in cache:
                candidates.append((str(data_dir / COVERS_SUBDIR / rel), rel))

    candidates = list(dict.fromkeys(candidates))
    if candidates:
        print(f"pHash: считаю {len(candidates)} обложек на {jobs} потоках", flush=True)
        started = time.time()
        with open(cache_path, "a", encoding="utf-8") as out, mp.Pool(jobs) as pool:
            for index, (rel, digest) in enumerate(
                pool.imap_unordered(_phash_job, candidates, chunksize=16), 1
            ):
                if digest:
                    cache[rel] = int(digest, 16)
                    out.write(f"{rel}\t{digest}\n")
                if index % 2000 == 0:
                    out.flush()
                    rate = index / max(time.time() - started, 1e-6)
                    print(f"  {index}/{len(candidates)}  {rate:.0f} шт/с", flush=True)

    # Одинаковый дизайн обложки у многих разных названий — это шаблон серии
    # («рамка издательства»), а не признак одной книги. Такие хеши не считаем.
    titles_per_hash: dict[int, set[str]] = defaultdict(set)
    for records in groups.values():
        for rec in records:
            if rec.cover in cache:
                titles_per_hash[cache[rec.cover]].add(rec.title_norm)
    generic_hashes = {value for value, titles in titles_per_hash.items()
                      if len(titles) >= PHASH_GENERIC_TITLES}

    overrides: dict = {}
    skipped = 0
    for key, records in groups.items():
        if len(records) < 2 or records[0].generic:
            continue
        for rec in records:
            if rec.cover and rec.cover in cache:
                rec.phash = cache[rec.cover]
        # (хеш обложки, автор): разные книги одной серии рисуют по одному
        # шаблону, поэтому «похожая» обложка сама по себе ничего не доказывает —
        # при distance 1..5 требуем ещё и совпадения автора.
        items = [(rec.phash, rec.author_norm) for rec in records
                 if rec.phash is not None and rec.phash not in generic_hashes]
        if len(items) < 2:
            if any(rec.phash is not None for rec in records):
                skipped += 1
            continue
        found = False
        for i in range(len(items)):
            for j in range(i + 1, len(items)):
                distance = (items[i][0] ^ items[j][0]).bit_count()
                same_author = bool(items[i][1]) and items[i][1] == items[j][1]
                if distance <= PHASH_EXACT_DISTANCE or (
                    distance <= PHASH_MAX_DISTANCE and same_author
                ):
                    found = True
                    break
            if found:
                break
        if found:
            overrides[key] = "exact_cover"
    if skipped:
        print(f"  не сравнивались из-за типовых обложек серий: {skipped} групп")
    return overrides


def _phash_job(job: tuple[str, str]) -> tuple[str, str]:
    """(полный_путь, ключ_кэша) → (ключ_кэша, хеш). Ключ — путь из поля cover."""
    path, rel = job
    _, digest = phash_file(path)
    return rel, digest


# --------------------------------------------------------------------------
# Отчёты
# --------------------------------------------------------------------------
def write_csvs(rows, groups, statuses, pair_status, keeper_of):
    """groups.csv и records.csv. Никаких записей вне dedup_out/."""
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    with open(OUT_DIR / "groups.csv", "w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow([
            "group_id", "title_norm", "status", "copies", "keeper_id", "keeper_file",
            "volume", "generic", "title", "keeper_has_cover",
            "pair_status", "distinct_authors",
        ])
        for key, records, group_id in rows:
            keeper = keeper_of[key]
            authors = len({rec.author_norm for rec in records if rec.author_norm})
            writer.writerow([
                group_id, key[0], statuses[key], len(records), keeper.id, keeper.file,
                key[1], int(records[0].generic), records[0].title, int(keeper.has_cover),
                pair_status[key], authors,
            ])
    with open(OUT_DIR / "records.csv", "w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow([
            "group_id", "title_norm", "status", "is_keeper", "id", "title", "author",
            "year", "isbn", "hash", "size", "cover", "src_file", "src_line",
            "volume", "generic",
        ])
        for key, records, group_id in rows:
            keeper = keeper_of[key]
            for rec in records:
                writer.writerow([
                    group_id, key[0], statuses[key], int(rec is keeper), rec.id, rec.title,
                    rec.author, rec.year, "; ".join(sorted(rec.isbns)), rec.hash, rec.size,
                    rec.cover or "", rec.src_file, rec.src_line, key[1], int(rec.generic),
                ])


def print_summary(groups, total, statuses, has_hash, hash_mode):
    unique = len(groups)
    dup_groups = [key for key, records in groups.items() if len(records) > 1]
    extra = sum(len(groups[key]) - 1 for key in dup_groups)
    print("\n================= СВОДКА =================")
    print(f"всего записей:            {total}")
    print(f"уникальных названий+том:  {unique}")
    print(f"групп с дублями:          {len(dup_groups)}")
    print(f"лишних копий:             {extra}")
    print(f"режим хеша:               {hash_mode}{'' if has_hash else ' (хеша нет)'}")
    print("разбивка по статусам:")
    for status in reversed(STATUSES):
        keys = [key for key in dup_groups if statuses[key] == status]
        copies = sum(len(groups[key]) for key in keys)
        print(f"  {status:12s} групп {len(keys):6d}   записей {copies:7d}")
    print("  «пустые» названия (status=generic) — это РАЗНЫЕ книги с одинаковым\n"
          "  названием, дубликатами они не являются: удалять их нельзя.")


def _levenshtein(left: str, right: str) -> int:
    """Расстояние редактирования: нужно только для сравнения написаний автора."""
    if left == right:
        return 0
    if not left or not right:
        return max(len(left), len(right))
    previous = list(range(len(right) + 1))
    for i, a in enumerate(left, 1):
        current = [i]
        for j, b in enumerate(right, 1):
            current.append(min(previous[j] + 1, current[j - 1] + 1,
                               previous[j - 1] + (a != b)))
        previous = current
    return previous[-1]


def authors_close(one: str, other: str) -> bool:
    """Похожи ли подписи автора — «Никита Шарипов» и «Никита Эдуардович Шарипов».

    Нужно только для пар с одинаковой обложкой: там опечатка в фамилии ещё не
    значит, что это разные книги, а вот «Иван Петров» и «Сергей Смирнов» —
    уже значит.
    """
    if not one or not other:
        return False
    if one == other:
        return True
    first, second = set(one.split()), set(other.split())
    if first and second and (first <= second or second <= first):
        return True
    longest = max(len(one), len(other))
    return _levenshtein(one, other) / longest <= AUTHOR_CLOSE_RATIO


def plan_clusters(records: list[Rec], phash: dict, aggressive: bool,
                  use_covers: bool = True):
    """Разбить группу на кучки «это точно одна и та же книга».

    Статус группы — это про самую сильную пару в ней, а удалять нужно
    поштучно: в группе «Игра без правил» лежат 15 разных авторов, из которых
    совпадают только две записи. Поэтому здесь union-find по признакам
    тождества, и из каждой кучки остаётся ровно одна книга.

    Безопасный режим: id, хеш, ISBN, (автор+год+размер) и одинаковая обложка
    при похожем авторе. Агрессивный добавляет «тот же автор+год» и «тот же
    автор» — это уже разные издания одной книги.

    `use_covers=False` — для «пустых» названий («Рассказы», «Стихи»): там
    одинаковых обложек у разных книг сколько угодно, а вот совпавший id,
    ISBN или размер с автором и годом по-прежнему означают одну и ту же книгу.
    """
    size = len(records)
    parent = list(range(size))
    reason: dict[int, str] = {}

    def find(index: int) -> int:
        while parent[index] != index:
            parent[index] = parent[parent[index]]
            index = parent[index]
        return index

    def union(one: int, other: int, why: str) -> None:
        roots = {find(one), find(other)}
        # У корня кучки остаётся самое сильное доказательство из всех рёбер.
        for root in roots:
            old = reason.get(root)
            if old is None or PLAN_REASON_RANK[why] < PLAN_REASON_RANK[old]:
                reason[root] = why
        if len(roots) == 1:
            return
        first, second = sorted(roots)
        parent[second] = first

    buckets: dict[tuple, list[int]] = defaultdict(list)
    for index, rec in enumerate(records):
        buckets[("id", rec.id)].append(index)
        if rec.hash:
            buckets[("hash", rec.hash)].append(index)
        for isbn in rec.isbns:
            buckets[("isbn", isbn)].append(index)
        if rec.author_norm:
            buckets[("size", rec.author_norm, rec.year, rec.size)].append(index)
            if aggressive:
                buckets[("edition", rec.author_norm, rec.year)].append(index)
                buckets[("author", rec.author_norm)].append(index)
    for key, indices in buckets.items():
        why = key[0]
        for index in indices[1:]:
            union(indices[0], index, why)

    # Обложки сравниваем поштучно: одинаковый хеш — сильное доказательство,
    # но у издательских серий обложки похожи, поэтому «бит-в-бит» без
    # похожего автора не берём, а «почти такое же» — только при том же авторе.
    if use_covers:
        for i in range(size):
            cover_one = records[i].cover
            if not cover_one or cover_one not in phash:
                continue
            for j in range(i + 1, size):
                cover_two = records[j].cover
                if not cover_two or cover_two not in phash:
                    continue
                distance = (phash[cover_one] ^ phash[cover_two]).bit_count()
                if distance == 0 and authors_close(records[i].author_norm,
                                                   records[j].author_norm):
                    union(i, j, "cover")
                elif distance <= PHASH_MAX_DISTANCE and records[i].author_norm \
                        and records[i].author_norm == records[j].author_norm:
                    union(i, j, "cover_near")

    clusters: dict[int, list[int]] = defaultdict(list)
    for index in range(size):
        clusters[find(index)].append(index)
    return [(indices, reason.get(root, "size")) for root, indices in clusters.items()]


def _one_line(text: str) -> str:
    """Название и автор в plan.tsv: табы и переводы строк ломают TSV."""
    return " ".join(str(text or "").split())


def write_plan(rows, phash, aggressive: bool, text: dict | None = None) -> tuple[int, int, int]:
    """dedup_out/plan.tsv — что именно вынести из библиотеки.

    Одна строка = одну книгу убрать; `keeper_id` — та копия, которая
    остаётся, `reason` — почему решили, что это дубликат. Файл читает
    приложение (кнопка «Убрать дубликаты») и он же остаётся протоколом:
    ничего не удаляется без строки в этом файле.

    Сюда передаются ВСЕ записи библиотеки, включая одиночные группы.
    Кучки строятся глобально, а не внутри одного названия: одна и та же
    книга лежит под разными названиями («Тайна замка Горсорп-Грэйндж» и
    «Привидения в замке Горсторп-Грэйндж»), и такие склеивает отпечаток
    текста. Внутри названия работают прежние правила — id, хеш, ISBN,
    размер, обложка, автор.
    """
    OUT_DIR.mkdir(parents=True, exist_ok=True)

    flat: list = []
    group_of: list[str] = []
    starts: list[tuple[int, list]] = []
    for key, records, group_id in rows:
        start = len(flat)
        flat.extend(records)
        group_of.extend([group_id] * len(records))
        starts.append((start, records))

    parent = list(range(len(flat)))
    reason: dict[int, str] = {}

    def find(index: int) -> int:
        while parent[index] != index:
            parent[index] = parent[parent[index]]
            index = parent[index]
        return index

    def union(one: int, other: int, why: str) -> None:
        roots = {find(one), find(other)}
        # У корня кучки остаётся самое сильное доказательство из всех рёбер.
        for root in roots:
            old = reason.get(root)
            if old is None or PLAN_REASON_RANK[why] < PLAN_REASON_RANK[old]:
                reason[root] = why
        if len(roots) == 1:
            return
        first, second = sorted(roots)
        parent[second] = first

    # Один и тот же id, но в разных названиях — тоже дубликат. Записи без id
    # пропускаем: без этой проверки все они склеились бы в одну кучку.
    by_id: dict = defaultdict(list)
    for index, rec in enumerate(flat):
        if rec.id is not None:
            by_id[rec.id].append(index)
    for indices in by_id.values():
        for index in indices[1:]:
            union(indices[0], index, "id")

    for start, records in starts:
        generic = records[0].generic
        clusters = plan_clusters(records, phash, aggressive and not generic,
                                 use_covers=not generic)
        for indices, why in clusters:
            for index in indices[1:]:
                union(start + indices[0], start + index, why)

    text_pairs = 0
    if text:
        text_pairs = _union_by_text(flat, text, union)
    cover_pairs = _union_by_cover(flat, union)

    clusters: dict[int, list[int]] = defaultdict(list)
    for index in range(len(flat)):
        clusters[find(index)].append(index)

    kept = 0
    removed = 0
    bytes_out = 0
    with open(OUT_DIR / PLAN_NAME, "w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
        writer.writerow(["id", "archive", "internal_path", "cover", "group_id",
                         "title", "author", "year", "size", "keeper_id", "reason"])
        for root, indices in clusters.items():
            if len(indices) < 2:
                continue
            # Оставляем лучшую копию по тем же правилам, что и keeper группы.
            order = sorted(indices, key=lambda i: keeper_rank(flat[i]))
            best = order[-1]
            kept += 1
            for index in reversed(order[:-1]):
                rec = flat[index]
                removed += 1
                bytes_out += rec.size
                writer.writerow([
                    rec.id, rec.archive, rec.internal, rec.cover or "",
                    group_of[best], _one_line(rec.title), _one_line(rec.author),
                    rec.year, rec.size, flat[best].id, reason.get(root, "size"),
                ])
    print(f"  склеено по отпечатку текста: {text_pairs} пар")
    print(f"  склеено как «копия без обложки»: {cover_pairs} пар")
    return kept, removed, bytes_out


def current_files(data_dir: Path) -> set:
    """Файлы, которые сейчас есть в библиотеке: (архив, путь внутри архива)."""
    out: set = set()
    for shard in sorted((data_dir / METADATA_SUBDIR).glob("*.jsonl")):
        with open(shard, encoding="utf-8") as handle:
            for line in handle:
                if not line.strip():
                    continue
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if row.get(F_ARCHIVE) and row.get(F_INTERNAL):
                    out.add((row[F_ARCHIVE], row[F_INTERNAL]))
    return out


def merge_plan(old_path: Path, new_path: Path, data_dir: Path) -> tuple[int, int]:
    """Дописать в новый план строки прошлого прогона.

    Прошлый прогон уже убрал свои дубликаты из metadata, но файлы в архивах
    остались (пересборка не дошла до конца), поэтому выносить их всё ещё надо.

    Строки, чей файл **есть в библиотеке сейчас**, пропускаются: судьбу такой
    книги решает новый план, а старый для неё устарел. Без этого правила
    восставленные вручную книги пропадали бы снова — старый план всё ещё
    помнит, что их файлы надо вынести.
    """
    present = current_files(data_dir)
    with open(new_path, encoding="utf-8", newline="") as handle:
        rows = list(csv.reader(handle, delimiter="\t"))
    header, body = rows[0], rows[1:]
    removed = {row[0] for row in body}
    keepers = {row[9] for row in body}
    added = 0
    skipped = 0
    if old_path.exists():
        with open(old_path, encoding="utf-8", newline="") as handle:
            old = list(csv.reader(handle, delimiter="\t"))
        for row in old[1:]:
            if len(row) != len(header) or not row[0]:
                continue
            if row[0] in removed or row[0] in keepers or (row[1], row[2]) in present:
                skipped += 1
                continue
            body.append(row)
            removed.add(row[0])
            added += 1
        with open(new_path, "w", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
            writer.writerow(header)
            writer.writerows(body)
    return added, skipped


def protect_last_copy(data_dir: Path, plan_path: Path) -> int:
    """Не дать плану унести последнюю строку книги.

    Строка метаданных убирается по файлу, поэтому если в план попали файлы
    **всех** записей с одним id, книга исчезнет из библиотеки — файл в архиве
    останется, но найти его будет нечем. Бывает это там, где id не уникален
    (одна книга в двух перекрывающихся архивах) и план собрали из двух
    прогонов: строку книги вернули, а старый план всё ещё хочет вынести её файл.
    В таком случае одну строку плана выбрасываем — файл остаётся на месте.

    Возвращает число выброшенных строк.
    """
    lines: dict = defaultdict(list)
    for shard in sorted((data_dir / METADATA_SUBDIR).glob("*.jsonl")):
        with open(shard, encoding="utf-8") as handle:
            for line in handle:
                if not line.strip():
                    continue
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if row.get(F_ID) is not None and row.get(F_ARCHIVE) and row.get(F_INTERNAL):
                    lines[row[F_ID]].append((row[F_ARCHIVE], row[F_INTERNAL]))

    with open(plan_path, encoding="utf-8", newline="") as handle:
        rows = list(csv.reader(handle, delimiter="\t"))
    header, body = rows[0], rows[1:]
    removed = {(row[1], row[2]) for row in body if len(row) > 2}
    drop = {files[0] for files in lines.values()
            if len(files) > 1 and all(item in removed for item in files)}
    if not drop:
        return 0
    body = [row for row in body if (row[1], row[2]) not in drop]
    with open(plan_path, "w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
        writer.writerow(header)
        writer.writerows(body)
    return len(drop)


def matching_bucket(records: list[Rec], status: str):
    """Записи той пары/кучки, из-за которой группа получила статус.

    Для probable это кучка с одинаковым автором, для confirmed — с одинаковым
    автором и годом, для possible общего признака нет — показываем первые.
    """
    if status == "probable":
        counter = Counter(rec.author_norm for rec in records if rec.author_norm)
        best = max(counter, key=lambda value: counter[value], default="")
        return f"автор {best!r}", [rec for rec in records if rec.author_norm == best]
    if status == "confirmed":
        counter = Counter((rec.author_norm, rec.year) for rec in records
                          if rec.author_norm and rec.year)
        if counter:
            best = max(counter, key=lambda value: counter[value])
            return (f"автор+год {best[0]!r} / {best[1]}",
                    [rec for rec in records if (rec.author_norm, rec.year) == best])
        isbns = Counter(isbn for rec in records for isbn in rec.isbns)
        best = max(isbns, key=lambda value: isbns[value], default="")
        return f"ISBN {best}", [rec for rec in records if best in rec.isbns]
    return "общего признака нет", records[:3]


def print_extras(rows, groups, statuses, keeper_of, top=20):
    """Топ групп по копиям, примеры probable/possible и keeper без обложки."""
    print("\n=========== ТОП-20 ГРУПП ПО ЧИСЛУ КОПИЙ ===========")
    for key, records, group_id in sorted(rows, key=lambda row: -len(row[1]))[:top]:
        authors = len({rec.author_norm for rec in records if rec.author_norm})
        mark = " [пустое название]" if records[0].generic else ""
        print(f"  {len(records):4d} копий  {statuses[key]:12s} {group_id}  "
              f"{records[0].title[:46]!r}  разных авторов: {authors}{mark}")

    print("\n=========== ТОП-20 ГРУПП БЕЗ «ПУСТЫХ» НАЗВАНИЙ ===========")
    shown = 0
    for key, records, group_id in rows:
        if records[0].generic:
            continue
        authors = len({rec.author_norm for rec in records if rec.author_norm})
        print(f"  {len(records):4d} копий  {statuses[key]:12s} {group_id}  "
              f"{records[0].title[:46]!r}  разных авторов: {authors}")
        shown += 1
        if shown >= top:
            break

    for status in ("probable", "possible"):
        print(f"\n=========== 20 ПРИМЕРОВ {status.upper()} (без пустых названий) ===========")
        shown = 0
        for key, records, group_id in rows:
            if statuses[key] != status or records[0].generic:
                continue
            label, bucket = matching_bucket(records, status)
            print(f"  {group_id}  копий {len(records)}  {records[0].title[:44]!r}  → {label}")
            for rec in bucket[:3]:
                print(f"      id={rec.id:<7} {rec.author[:32]!r:34} {rec.year:4s} "
                      f"{rec.size/1048576:7.2f} МБ  обл={'да' if rec.cover else 'нет'}")
            shown += 1
            if shown >= 20:
                break
        if not shown:
            print("  (нет)")

    print("\n=== KEEPER БЕЗ ОБЛОЖКИ, А У КОПИИ ОБЛОЖКА ЕСТЬ ===")
    suspicious = []
    no_cover_at_all = 0
    for key, records, group_id in rows:
        keeper = keeper_of[key]
        if not any(rec.cover for rec in records):
            no_cover_at_all += 1
            continue
        if keeper.has_cover:
            continue
        suspicious.append((key, records, group_id, keeper))
    with open(OUT_DIR / "keeper_without_cover.csv", "w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(["group_id", "status", "title", "keeper_id", "copies_with_cover",
                         "ids_with_cover", "keeper_file"])
        for key, records, group_id, keeper in suspicious:
            covered = [rec for rec in records if rec.cover]
            writer.writerow([group_id, statuses[key], records[0].title, keeper.id,
                             len(covered), "; ".join(str(rec.id) for rec in covered), keeper.file])
    print(f"  таких групп: {len(suspicious)} → dedup_out/keeper_without_cover.csv")
    print("  (по правилу приоритета keeper всегда с обложкой, если она есть хоть у одной "
          "копии, поэтому список пуст by design — он остаётся как проверка)")
    print(f"  групп, где обложки нет вообще ни у одной копии: {no_cover_at_all}")


# --------------------------------------------------------------------------
# Точка входа
# --------------------------------------------------------------------------
def analyse(args) -> int:
    data_dir = Path(args.data).resolve()
    groups, total, has_hash = load_groups(data_dir, args.hash_mode, args.limit)
    if not groups:
        print("не найдено ни одной записи — проверьте --data")
        return 1

    # «Пустые» названия помечаем после того, как известны размеры групп.
    for key, records in groups.items():
        generic = is_generic(key[0], len(records))
        for rec in records:
            rec.generic = generic

    # pair_status — что дало сравнение по автору/году/размеру, status — итог
    # с учётом «пустого» названия и обложек.
    pair_status = {key: classify(records, has_hash) for key, records in groups.items()}
    statuses = {key: ("generic" if groups[key][0].generic else value)
                for key, value in pair_status.items()}

    upgraded = 0
    if args.phash:
        overrides = phash_pass(data_dir, groups, args.jobs)
        statuses.update(overrides)
        upgraded = len(overrides)
    print(f"по обложкам поднято до exact_cover: {upgraded} групп")

    keeper_of = {key: max(records, key=keeper_rank) for key, records in groups.items()}
    for key, keeper in keeper_of.items():
        keeper.keeper = True

    dup_keys = [key for key, records in groups.items() if len(records) > 1]
    # Сначала настоящие дубликаты, «пустые» названия — в конец файла: по этому
    # отчёту удаляют книги, и первые строки должны быть про реальные группы.
    dup_keys.sort(key=lambda key: (groups[key][0].generic, -len(groups[key]),
                                   groups[key][0].title_norm))
    rows = [(key, groups[key], f"g{index:06d}") for index, key in enumerate(dup_keys, 1)]
    singles = [key for key, records in groups.items() if len(records) == 1]
    singles.sort(key=lambda key: groups[key][0].title_norm)
    single_rows = [(key, groups[key], f"s{index:06d}") for index, key in enumerate(singles, 1)]
    if not ONLY_DUPLICATE_GROUPS:
        rows += single_rows

    # В план идут ВСЕ записи, включая одиночные группы: метаданные разводят
    # копии одной книги по разным группам (разный series_number, «Том 2» в
    # названии у одной копии и нет у другой), и такая пара должна склеиться по
    # тексту. Если дать плану только группы с дублями, обе копии окажутся в
    # одиночных группах и текстовый слой их просто не увидит.
    plan_rows = rows + single_rows if ONLY_DUPLICATE_GROUPS else rows

    print_summary(groups, total, statuses, has_hash, args.hash_mode)
    write_csvs(rows, groups, statuses, pair_status, keeper_of)
    print(f"\nзаписано: {OUT_DIR/'groups.csv'} ({len(rows)} строк), "
          f"{OUT_DIR/'records.csv'} ({sum(len(r[1]) for r in rows)} строк)")

    phash = load_phash_cache(OUT_DIR / PHASH_CACHE_NAME) if args.phash else {}
    if not args.phash:
        print("план без обложек: запустите с --phash, чтобы ловить копии по обложке")
    text = {} if args.no_text else load_text_cache(OUT_DIR / TEXT_NAME)
    if not text:
        print("отпечатков текста нет — запустите textdup.py, чтобы ловить копии по тексту")
    else:
        print(f"отпечатки текста: {len(text)} книг")
    kept, removed, bytes_out = write_plan(plan_rows, phash, args.aggressive, text)
    if args.merge_plan:
        added, skipped = merge_plan(Path(args.merge_plan), OUT_DIR / PLAN_NAME, data_dir)
        print(f"дописан прошлый план ({args.merge_plan}): +{added} строк, "
              f"пропущено {skipped} (уже в плане или стали keeper)")
    protected = protect_last_copy(data_dir, OUT_DIR / PLAN_NAME)
    if protected:
        print(f"оставлено последних копий: {protected} (иначе книга пропала бы совсем)")
    mode = "агрессивный (разные издания тоже)" if args.aggressive else "безопасный (только копии)"
    print(f"план выноса: {OUT_DIR/PLAN_NAME} — режим {mode}")
    print(f"  остаётся книг:  {kept}")
    print(f"  вынести книг:   {removed}  ({bytes_out / 2**30:.1f} ГБ)")

    print_extras(rows, groups, statuses, keeper_of)
    return 0


def main() -> int:
    global OUT_DIR
    parser = argparse.ArgumentParser(description="Поиск дубликатов книг")
    parser.add_argument("--data", default=str(DATA_DIR), help="каталог данных библиотеки")
    parser.add_argument("--out", default=str(OUT_DIR), help="каталог для отчётов")
    parser.add_argument("--hash-mode", default=HASH_MODE,
                        choices=["auto", "content", "docid", "none"],
                        help="auto: кэш content_hash, иначе без хеша")
    parser.add_argument("--limit", type=int, default=None, help="обработать только N записей")
    parser.add_argument("--jobs", type=int, default=JOBS, help="потоков для arx/pHash")
    parser.add_argument("--build-hashes", action="store_true",
                        help="посчитать content_hash всех книг (долго, читает архивы) и выйти")
    parser.add_argument("--phash", action="store_true",
                        help="дополнительно сравнить обложки по pHash (нужен Pillow)")
    parser.add_argument("--aggressive", action="store_true",
                        help="в план выноса включать и разные издания одной книги "
                             "(тот же автор+год или тот же автор при одном названии)")
    parser.add_argument("--no-text", action="store_true",
                        help="не использовать отпечатки текста (dedup_out/text.tsv)")
    parser.add_argument("--merge-plan", default="",
                        help="дописать в план строки прошлого прогона: их файлы "
                             "ещё лежат в архивах, хотя из metadata уже убраны")
    args = parser.parse_args()

    OUT_DIR = Path(args.out)
    if args.build_hashes:
        build_hash_cache(Path(args.data).resolve(), OUT_DIR / HASH_CACHE_NAME, args.limit, args.jobs)
        return 0
    return analyse(args)


if __name__ == "__main__":
    sys.exit(main())
