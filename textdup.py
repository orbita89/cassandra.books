#!/usr/bin/env python3
"""Отпечатки текста книг: `dedup_out/text.tsv`.

Зачем это нужно. Правила по метаданным ловят дубликаты там, где совпали
название, автор, год, размер или ISBN. Но одна и та же книга, собранная из
разных исходников, отличается как раз мелочами: у двух записей Кира Булычёва
«Усилия любви» 18158 и 18069 знаков текста — это одна и та же книга, а
метаданные у неё разные. Поэтому здесь считается отпечаток самого текста.

Как считается. EPUB читается, из всех HTML-файлов вырезаются теги, текст
приводится к нижнему регистру (ё → е) и режется на слова. Дальше берутся
шинглы — пятёрки подряд идущих слов, — каждая хешируется, и остаются 192
самых маленьких хеша (bottom-k sketch). По двум таким наборам оценка Жаккара
считается как |пересечение| / |объединение|, и она устойчива к вставкам и
опечаткам: у настоящих дубликатов выходит 0.95–1.00, у разных книг — единицы
процентов. Замер на 32 парах «одно название + один автор»: минимум 0.949.

Почему bottom-k, а не «каждый N-й шингл»: выборка по позиции рассыпается от
одной вставки в начале текста, а выборка по значению хеша — нет.

Архивы не распаковываются: они монтируются через `arx mount`, книги читаются
прямо оттуда. Распаковка писала бы 446 ГБ мелких файлов (Btrfs тянет их на
~60 МБ/с, и весь проход упирался бы в запись), а с монтированием всё упирается
в процессор: 55 мс на книгу, то есть ~35 минут на библиотеку в 12 потоков.

Запуск:

    python3 textdup.py --jobs 12

Проход можно прерывать: уже посчитанные книги лежат в `dedup_out/text.tsv.part`
и при следующем запуске не считаются заново. В конце файл переименовывается в
`dedup_out/text.tsv`: `id`, число слов, 192 хеша через запятую. Его читает
`dedup.py` и добавляет рёбра «тот же текст» в план выноса.
"""

from __future__ import annotations

import argparse
import glob
import heapq
import json
import os
import re
import shutil
import subprocess
import sys
import time
from multiprocessing import Pool

ROOT = os.path.dirname(os.path.abspath(__file__))
OUT_DIR = os.path.join(ROOT, "dedup_out")
TEXT_NAME = "text.tsv"
WORK_DIR = os.path.join(ROOT, "data", "tmp", "textdup")

#: Сколько хешей шинглов храним на книгу.
SKETCH = 192
#: Длина шингла в словах.
SHINGLE = 5

_TAG = re.compile(r"(?is)<(script|style).*?</\1>|<[^>]+>")
_WORD = re.compile(r"[0-9a-zа-я]+")
_MASK = (1 << 64) - 1
_PRIME = 0x100000001B3
_word_cache: dict[str, int] = {}


def word_hash(word: str) -> int:
    """Детерминированный хеш слова (встроенный `hash` солится и не годится)."""
    value = _word_cache.get(word)
    if value is None:
        value = 0
        for byte in word.encode("utf-8"):
            value = (value * 131 + byte) & _MASK
        _word_cache[word] = value
    return value


def sketch_of(path: str) -> tuple[int, list[int]] | None:
    """Отпечаток одного EPUB: (слов, хеши шинглов)."""
    import zipfile

    try:
        with zipfile.ZipFile(path) as archive:
            words: list[str] = []
            for name in sorted(archive.namelist()):
                low = name.lower()
                if not low.endswith((".xhtml", ".html", ".htm")):
                    continue
                raw = archive.read(name).decode("utf-8", "ignore")
                text = _TAG.sub(" ", raw).lower().replace("ё", "е")
                words += _WORD.findall(text)
    except Exception:
        return None
    if len(words) < SHINGLE:
        return None

    # Катящийся хеш по пятёркам слов: следующая пятёрка считается за одно
    # умножение, без склейки строк.
    hashes: list[int] = []
    rolling = 0
    power = 1
    for index, word in enumerate(words):
        rolling = (rolling * _PRIME + word_hash(word)) & _MASK
        if index >= SHINGLE:
            rolling = (rolling - power * word_hash(words[index - SHINGLE])) & _MASK
        else:
            power = (power * _PRIME) & _MASK
        if index >= SHINGLE - 1:
            hashes.append(rolling)
    if not hashes:
        return None
    return len(words), heapq.nsmallest(SKETCH, hashes)


def unmount(path: str) -> bool:
    """Снять FUSE-монтирование любым доступным способом."""
    for command in (["fusermount3", "-u"], ["fusermount", "-u"], ["umount"], ["umount", "-l"]):
        if shutil.which(command[0]) is None:
            continue
        done = subprocess.run(command + [path], capture_output=True)
        if done.returncode == 0:
            return True
    return False


def cleanup_mounts() -> None:
    """Убрать монтирования, оставшиеся от оборванного запуска."""
    if not os.path.isdir(WORK_DIR):
        return
    for name in os.listdir(WORK_DIR):
        path = os.path.join(WORK_DIR, name)
        if os.path.ismount(path):
            unmount(path)
    for name in os.listdir(WORK_DIR):
        shutil.rmtree(os.path.join(WORK_DIR, name), ignore_errors=True)


def books_by_archive(metadata_dir: str) -> dict[str, list[tuple[int, str]]]:
    """`архив → [(id, путь внутри архива)]` по метаданным библиотеки."""
    out: dict[str, list[tuple[int, str]]] = {}
    for path in glob.glob(os.path.join(metadata_dir, "*.jsonl")):
        with open(path, encoding="utf-8") as handle:
            for line in handle:
                if not line.strip():
                    continue
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                out.setdefault(row["archive"], []).append((row["id"], row["internal_path"]))
    return out


def wait_for_mount(path: str, seconds: float = 30.0) -> str:
    """Дождаться, пока в точке монтирования появятся файлы.

    `arx mount` уходит в фон (демонизируется), поэтому о состоянии судим по
    самой точке: в пустом каталоге файлов нет, в смонтированном архиве — есть.
    Проверять `poll()` тут нельзя, а читать stderr-пайп нельзя тем более:
    трубу держит и потомок, и чтение повисло бы навсегда.
    """
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            if os.listdir(path):
                return ""
        except OSError:
            pass
        time.sleep(0.2)
    return "монтирование не поднялось"


def process_archive(job: tuple[str, list[tuple[int, str]]]) -> tuple[str, list[tuple[int, int, list[int]]], str]:
    """Смонтировать архив, снять отпечатки, размонтировать."""
    archive, books = job
    stem = archive[:-4] if archive.endswith(".arx") else archive
    mount_point = os.path.join(WORK_DIR, stem)
    shutil.rmtree(mount_point, ignore_errors=True)
    os.makedirs(mount_point, exist_ok=True)
    source = os.path.join(ROOT, "data", "archives", archive)
    process = subprocess.Popen(
        ["arx", "mount", source, mount_point],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        error = wait_for_mount(mount_point)
        if error:
            return archive, [], error
        rows: list[tuple[int, int, list[int]]] = []
        for book_id, internal in books:
            path = os.path.join(mount_point, internal)
            try:
                if not os.path.isfile(path):
                    continue
                result = sketch_of(path)
            except Exception:
                continue
            if result is None:
                continue
            words, hashes = result
            rows.append((book_id, words, hashes))
        return archive, rows, ""
    finally:
        unmount(mount_point)
        if process.poll() is None:
            process.terminate()
        shutil.rmtree(mount_point, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser(description="Отпечатки текста книг для выноса дубликатов")
    parser.add_argument("--jobs", type=int, default=0, help="сколько архивов обрабатывать сразу")
    parser.add_argument("--data", default=os.path.join(ROOT, "data"), help="каталог данных")
    parser.add_argument("--limit", type=int, default=0, help="взять только N архивов (для проверки)")
    parser.add_argument("--fresh", action="store_true", help="начать заново, не продолжая прерванный проход")
    args = parser.parse_args()

    jobs = args.jobs or max(1, min(12, os.cpu_count() or 4))
    os.makedirs(OUT_DIR, exist_ok=True)
    os.makedirs(WORK_DIR, exist_ok=True)
    cleanup_mounts()

    part_path = os.path.join(OUT_DIR, TEXT_NAME + ".part")
    if args.fresh:
        for path in (part_path, os.path.join(OUT_DIR, TEXT_NAME)):
            if os.path.exists(path):
                os.remove(path)

    done: set[int] = set()
    if os.path.exists(part_path):
        with open(part_path, encoding="utf-8") as handle:
            for line in handle:
                head = line.split("\t", 1)[0]
                if head.isdigit():
                    done.add(int(head))
        print(f"продолжаю прерванный проход: уже посчитано {len(done)} книг", flush=True)

    by_archive = books_by_archive(os.path.join(args.data, "metadata"))
    items = []
    for archive, books in sorted(by_archive.items()):
        left = [(book_id, internal) for book_id, internal in books if book_id not in done]
        if left:
            items.append((archive, left))
    if args.limit:
        items = items[: args.limit]
    total_books = sum(len(books) for _, books in items)
    print(f"архивов: {len(items)}, книг: {total_books}, потоков: {jobs}", flush=True)

    started = time.time()
    done_archives = 0
    done_books = 0
    failed: list[str] = []
    mode = "a" if done else "w"
    with open(part_path, mode, encoding="utf-8") as handle, Pool(jobs) as pool:
        for archive, rows, error in pool.imap_unordered(process_archive, items):
            done_archives += 1
            if error:
                failed.append(f"{archive}: {error}")
            for book_id, words, hashes in rows:
                handle.write(f"{book_id}\t{words}\t{','.join(format(h, '016x') for h in hashes)}\n")
            done_books += len(rows)
            if done_archives % 5 == 0 or done_archives == len(items):
                handle.flush()
                speed = done_books / max(1e-9, time.time() - started)
                left = (total_books - done_books) / max(1e-9, speed)
                print(
                    f"[{done_archives}/{len(items)}] книг {done_books}/{total_books}, "
                    f"осталось ~{left/60:.0f} мин, ошибок {len(failed)}",
                    flush=True,
                )
    os.replace(part_path, os.path.join(OUT_DIR, TEXT_NAME))
    print(f"готово: {os.path.join(OUT_DIR, TEXT_NAME)}, книг {done_books + len(done)}, "
          f"за {(time.time()-started)/60:.1f} мин", flush=True)
    for line in failed[:10]:
        print(f"  не вышло: {line}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
