//! Отрисовка интерфейса.
//!
//! Четыре зоны:
//!   * верхняя панель — поиск, фильтры по автору и жанру, импорт, кэш;
//!   * центральная — сетка обложек (если обложки нет — название книги);
//!   * окно книги — все метаданные, аннотация, главы и кнопка «Читать»;
//!     открывается кликом по обложке, а не висит сбоку постоянно;
//!   * нижняя — прогресс импорта (появляется только во время работы).
//!
//! Замечание по egui 0.37: панели объединены в один тип [`egui::Panel`],
//! и все они раскрываются внутрь [`egui::Ui`], а не в [`egui::Context`].
//! Панели должны добавляться до центральной — она забирает остаток места.
//! Исключение — [`egui::Window`]: он по-прежнему раскрывается в контекст.

use crate::db::BookMetadata;
use crate::{human_size, App, CACHE_LIMIT_BYTES};
use anyhow::{Context, Result};
use egui::{Color32, RichText, Stroke};
use std::path::Path;
use std::process::{Command, Stdio};

/// Видимая ширина ячейки сетки обложек.
const CELL_W: f32 = 118.0;
/// Высота ячейки: обложка 3:4 плюс немного воздуха.
const CELL_H: f32 = 166.0;
/// Промежуток между ячейками.
const CELL_GAP: f32 = 10.0;
/// Высота строки сетки с учётом промежутка.
const ROW_H: f32 = CELL_H + 20.0;

/// Нарисовать весь кадр внутрь корневого `ui`.
pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    top_panel(app, ui);
    bottom_panel(app, ui);
    cover_grid(app, ui);
    book_window(app, ui);
    error_window(app, ui);

    // Поиск «на лету»: как только текст изменился — пересчитываем результаты.
    if app.query != app.last_query {
        if let Err(err) = app.refresh_search() {
            app.error = Some(format!("поиск не удался: {err:#}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Верхняя панель: поиск, фильтры, импорт, кэш
// ---------------------------------------------------------------------------

fn top_panel(app: &mut App, ui: &mut egui::Ui) {
    // Фильтры правим в локальных копиях и записываем обратно после панели:
    // так вложенные замыкания egui не конфликтуют за поля `App`.
    let mut filter_author = app.filter_author.clone();
    let mut filter_genre = app.filter_genre.clone();
    let authors = app.authors.clone();
    let genres = app.genres.clone();
    let mut filters_changed = false;
    let mut clear_cache = false;
    let (cache_files, cache_bytes) = app.cache;

    egui::Panel::top("top_panel").show(ui, |ui| {
        ui.add_space(6.0);

        // ---- строка 1: заголовок и счётчик книг ----------------------------
        ui.horizontal(|ui| {
            ui.heading("📚 Библиотека");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("книг: {}", app.library.books.len()))
                        .color(Color32::from_gray(160)),
                );
            });
        });

        // ---- строка 2: поиск и действия ------------------------------------
        // Важно: внутри `horizontal_wrapped` нельзя ставить `with_layout`
        // с выравниванием вправо — такой блок забирает всю доступную высоту,
        // и панель раздувается на сотни пикселей. Поэтому он вынесен выше.
        ui.horizontal_wrapped(|ui| {
            ui.label("Поиск:");
            ui.add_sized(
                [320.0, 22.0],
                egui::TextEdit::singleline(&mut app.query)
                    .hint_text("название, автор, аннотация…")
                    .desired_width(320.0),
            );
            if ui.button("Сбросить").clicked() {
                app.query.clear();
            }

            ui.separator();
            let importing = app.import.running;
            ui.add_enabled_ui(!importing, |ui| {
                if ui.button("📥 Импортировать ZIP…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("ZIP-архив с FB2", &["zip"])
                        .set_title("Выберите ZIP-архив с FB2")
                        .pick_file()
                    {
                        app.start_import(path);
                    }
                }
            });
            if importing {
                ui.spinner();
            }

            ui.separator();
            ui.add_enabled_ui(!importing, |ui| {
                ui.label("потоков:");
                ui.add(
                    egui::DragValue::new(&mut app.settings.jobs)
                        .range(0..=128)
                        .speed(1.0),
                )
                .on_hover_text(
                    "0 = все ядра автоматически. Меняется только до начала импорта.",
                );
            });

            ui.separator();
            let cache_text = format!("🧹 кэш: {}", human_size(cache_bytes));
            let hint = format!(
                "Распакованные для чтения книги лежат в data/cache.\n\
                 Файлов: {cache_files}, объём: {} (предел {}).\n\
                 Нажмите, чтобы очистить.",
                human_size(cache_bytes),
                human_size(CACHE_LIMIT_BYTES)
            );
            if ui.button(cache_text).on_hover_text(hint).clicked() {
                clear_cache = true;
            }
        });

        // ---- строка 3: фильтры по автору и жанру ---------------------------
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Автор:").color(Color32::from_gray(170)));
            egui::ComboBox::from_id_salt("author_filter")
                .selected_text(filter_author.clone().unwrap_or_else(|| "все авторы".into()))
                .width(230.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(filter_author.is_none(), "все авторы")
                        .clicked()
                    {
                        filter_author = None;
                        filters_changed = true;
                    }
                    for (name, count) in &authors {
                        let selected = filter_author.as_deref() == Some(name.as_str());
                        if ui
                            .selectable_label(selected, format!("{name}  ({count})"))
                            .clicked()
                        {
                            filter_author = Some(name.clone());
                            filters_changed = true;
                        }
                    }
                });

            ui.add_space(10.0);
            ui.label(RichText::new("Жанр:").color(Color32::from_gray(170)));
            egui::ComboBox::from_id_salt("genre_filter")
                .selected_text(filter_genre.clone().unwrap_or_else(|| "все жанры".into()))
                .width(230.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(filter_genre.is_none(), "все жанры")
                        .clicked()
                    {
                        filter_genre = None;
                        filters_changed = true;
                    }
                    for (name, count) in &genres {
                        let selected = filter_genre.as_deref() == Some(name.as_str());
                        if ui
                            .selectable_label(selected, format!("{name}  ({count})"))
                            .clicked()
                        {
                            filter_genre = Some(name.clone());
                            filters_changed = true;
                        }
                    }
                });

            if filter_author.is_some() || filter_genre.is_some() {
                ui.add_space(10.0);
                if ui.button("✖ сбросить фильтры").clicked() {
                    filter_author = None;
                    filter_genre = None;
                    filters_changed = true;
                }
            }

            ui.add_space(14.0);
            ui.label(RichText::new(format!("Найдено: {}", app.results.len())).strong());
            if app.matched > app.results.len() {
                ui.label(
                    RichText::new(format!("из {} подходящих", app.matched))
                        .small()
                        .color(Color32::from_gray(150)),
                );
            }
            ui.label(
                RichText::new("(фасеты ограничены 500 пунктами)")
                    .small()
                    .color(Color32::from_gray(110)),
            );
        });
        ui.add_space(6.0);
    });

    if filters_changed {
        app.filter_author = filter_author;
        app.filter_genre = filter_genre;
    }
    if clear_cache {
        app.clear_cache();
    }
}

// ---------------------------------------------------------------------------
// Центральная панель: сетка обложек
// ---------------------------------------------------------------------------

fn cover_grid(app: &mut App, ui: &mut egui::Ui) {
    let mut clicked: Option<usize> = None;
    let selected = app.selected;
    let opened = app.details_open;
    let dir = app.library.covers_dir();

    egui::CentralPanel::default().show(ui, |ui| {
        let results = &app.results;

        if results.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new("Ничего не найдено\nИмпортируйте ZIP-архив с FB2")
                        .size(16.0)
                        .color(Color32::from_gray(140)),
                );
            });
            return;
        }

        // Колонки считаем от ширины окна, но не меньше одной.
        let avail = ui.available_width() - 16.0;
        let step = CELL_W + CELL_GAP + 8.0;
        let cols = ((avail + CELL_GAP) / step).floor().max(1.0) as usize;
        let rows = results.len().div_ceil(cols);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, rows, |ui, row_range| {
                for row in row_range {
                    ui.horizontal(|ui| {
                        for col in 0..cols {
                            let idx = row * cols + col;
                            if idx >= results.len() {
                                break;
                            }
                            let is_sel = selected == Some(idx) && opened;
                            if cell(ui, &results[idx], &dir, is_sel).clicked() {
                                clicked = Some(idx);
                            }
                            ui.add_space(CELL_GAP);
                        }
                    });
                }
            });
    });

    // Клик по обложке открывает окно с информацией о книге.
    if let Some(i) = clicked {
        app.open_details(i);
    }
}

/// Одна ячейка сетки: обложка, а если её нет — название.
///
/// Размер ячейки выделяется **ровно один раз**. Раньше ветка «обложки нет»
/// добавляла внутри ещё один прямоугольник `CELL × CELL_H` — ячейка вырастала
/// вдвое, а `show_rows` в сетке считает высоту строки постоянной (`ROW_H`).
/// Из-за этого строки с книгой без обложки ломали прокрутку и попадать по ним
/// курсором было почти нельзя.
fn cell(ui: &mut egui::Ui, book: &BookMetadata, dir: &Path, selected: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(CELL_W, CELL_H), egui::Sense::click());

    let cover = book
        .cover
        .as_ref()
        .map(|name| dir.join(name))
        .filter(|p| p.exists());

    match cover {
        Some(path) => {
            egui::Image::from_uri(format!("file://{}", path.display()))
                .maintain_aspect_ratio(true)
                .paint_at(ui, rect);
        }
        None => {
            // Обложки нет — вместо неё название книги на сером фоне.
            let painter = ui.painter();
            painter.rect_filled(rect, 6.0, Color32::from_gray(46));
            painter.rect_stroke(
                rect,
                6.0,
                Stroke::new(1.0, Color32::from_gray(80)),
                egui::StrokeKind::Inside,
            );
            let galley = painter.layout(
                book.title.clone(),
                egui::FontId::proportional(12.0),
                Color32::from_gray(205),
                rect.width() - 16.0,
            );
            let pos = egui::pos2(
                rect.center().x - galley.size().x / 2.0,
                rect.top() + 8.0,
            );
            painter.galley(pos, galley, Color32::from_gray(205));
        }
    }

    if selected {
        ui.painter().rect_stroke(
            rect,
            6.0,
            Stroke::new(2.0, Color32::from_rgb(120, 180, 255)),
            egui::StrokeKind::Inside,
        );
    }

    let author = if book.author.trim().is_empty() {
        "—"
    } else {
        book.author.as_str()
    };
    response.on_hover_text(format!(
        "{}\n{}\n\nнажмите, чтобы открыть информацию",
        book.title, author
    ))
}

// ---------------------------------------------------------------------------
// Окно книги: всё, что есть в метаданных
// ---------------------------------------------------------------------------

enum Action {
    Read,
}

fn book_window(app: &mut App, ui: &mut egui::Ui) {
    if !app.details_open {
        return;
    }
    let Some(book) = app.selected_book().cloned() else {
        app.details_open = false;
        return;
    };

    let mut action: Option<Action> = None;
    let mut open = app.details_open;
    let ctx = ui.ctx().clone();

    // Заголовок окна постоянный — от него egui берёт id, и позиция окна
    // не сбрасывается при переходе к другой книге. Название книги — внутри.
    egui::Window::new("📖 Книга")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .default_height(560.0)
        .vscroll(true)
        .show(&ctx, |ui| {
            ui.heading(&book.title);
            if !book.author.trim().is_empty() {
                ui.label(RichText::new(&book.author).italics().size(15.0));
            }

            // Кнопка «Читать» — сразу под названием, а не в конце длинной
            // прокрутки: иначе до неё надо ещё долистать.
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button(RichText::new("📖  Читать").size(15.0))
                    .on_hover_text("распаковать в data/cache и открыть читалку")
                    .clicked()
                {
                    action = Some(Action::Read);
                }
                if let Some(toast) = &app.toast {
                    ui.label(RichText::new(toast).small().color(Color32::from_gray(150)));
                }
            });
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            ui.horizontal_top(|ui| {
                // Небольшая обложка слева, метаданные справа.
                if let Some(cover) = book
                    .cover
                    .as_ref()
                    .map(|name| app.library.covers_dir().join(name))
                    .filter(|p| p.exists())
                {
                    ui.add(
                        egui::Image::from_uri(format!("file://{}", cover.display()))
                            .max_width(140.0)
                            .max_height(200.0)
                            .maintain_aspect_ratio(true),
                    );
                    ui.add_space(10.0);
                }

                ui.vertical(|ui| {
                    let d = &book.details;
                    field(ui, "Авторы", &d.authors.join("; "));
                    field(ui, "Жанры", &book.genres.join(" · "));
                    field(ui, "Серия", &series_line(&d.series, &d.series_number));
                    field(ui, "Год", if d.year.is_empty() { &d.date } else { &d.year });
                    field(ui, "Дата написания", &d.date);
                    field(ui, "Издательство", &d.publisher);
                    field(ui, "Город", &d.city);
                    field(ui, "ISBN", &d.isbn);
                    field(ui, "Язык", &d.lang);
                    field(ui, "Язык оригинала", &d.src_lang);
                    field(ui, "Переводчики", &d.translators.join("; "));
                    field(ui, "Ключевые слова", &d.keywords);
                });
            });

            ui.add_space(8.0);
            ui.separator();
            let d = &book.details;
            field(ui, "Размер", &format!("{:.0} КБ", book.size as f64 / 1024.0));
            field(ui, "Архив", &book.archive);
            field(ui, "Внутри", &book.internal_path);
            field(ui, "ID книги", &book.id.to_string());
            field(ui, "ID FB2", &d.doc_id);
            field(ui, "Версия", &d.doc_version);
            field(ui, "Дата документа", &d.doc_date);
            field(ui, "Программа", &d.program_used);
            field(ui, "Источник", &d.src_url);
            field(ui, "OCR", &d.src_ocr);

            ui.add_space(10.0);

            // ---- аннотация -----------------------------------------
            if !d.annotation.trim().is_empty() {
                ui.collapsing(format!("Аннотация ({} символов)", d.annotation.chars().count()), |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(200.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(RichText::new(&d.annotation).size(12.5)).wrap(),
                            );
                        });
                });
            }

            // ---- главы ---------------------------------------------
            if !d.chapters.is_empty() {
                ui.collapsing(format!("Главы ({})", d.chapters.len()), |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (n, title) in d.chapters.iter().enumerate() {
                                ui.label(
                                    RichText::new(format!("{:>3}. {title}", n + 1)).size(12.5),
                                );
                            }
                        });
                });
            }

            ui.add_space(12.0);
        });

    app.details_open = open;

    if let Some(Action::Read) = action {
        // Кэш мог перерасти предел — урезаем до чтения.
        let _ = app.library.prune_cache(CACHE_LIMIT_BYTES);
        let result = read_book(app, &book);
        match result {
            Ok(msg) => app.toast = Some(msg),
            Err(err) => app.error = Some(format!("{err:#}")),
        }
        app.refresh_cache_usage();
    }
}

/// Строка «подпись: значение» — пропускается, если значение пустое.
fn field(ui: &mut egui::Ui, label: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("{label}: "))
                .small()
                .color(Color32::from_gray(150)),
        );
        ui.label(RichText::new(value).size(13.0));
    });
}

/// Серия и номер книги в ней, если они есть.
fn series_line(series: &str, number: &str) -> String {
    match (series.trim().is_empty(), number.trim().is_empty()) {
        (true, _) => String::new(),
        (false, true) => series.to_string(),
        (false, false) => format!("{series} — книга {number}"),
    }
}

/// Распаковать книгу в `data/cache` и открыть системной читалкой.
fn read_book(app: &App, book: &BookMetadata) -> Result<String> {
    let dir = app.library.cache_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("не удалось создать {}", dir.display()))?;
    let dest = dir.join(&book.internal_path);
    app.library.extract_to(book, &dest)?;

    // Сначала пробуем читалку для EPUB, потом — системный обработчик по умолчанию.
    // Вывод читалки глушим: иначе её предупреждения (у Foliate это WebKit
    // и Gtk-CRITICAL) заливают наш stderr мегабайтами.
    for program in ["foliate", "xdg-open"] {
        if Command::new(program)
            .arg(&dest)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
            return Ok(format!("Открыто в {program}: {}", dest.display()));
        }
    }
    Ok(format!("Извлечено в {}", dest.display()))
}

// ---------------------------------------------------------------------------
// Нижняя панель: прогресс импорта
// ---------------------------------------------------------------------------

fn bottom_panel(app: &mut App, ui: &mut egui::Ui) {
    let show = app.import.running
        || app.import.finished.is_some()
        || app.import.last_batch.is_some()
        || !app.import.errors.is_empty();
    if !show {
        return;
    }

    let mut hide = false;
    egui::Panel::bottom("progress_panel").show(ui, |ui| {
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            if app.import.running {
                ui.spinner();
            } else {
                ui.label("✅");
            }
            ui.label(&app.import.status);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !app.import.running && ui.small_button("скрыть").clicked() {
                    hide = true;
                }
                ui.label(
                    RichText::new(format!(
                        "потоков: {}",
                        crate::worker::threads_for(&app.settings)
                    ))
                    .small()
                    .color(Color32::from_gray(130)),
                );
                if let Some(batch) = &app.import.last_batch {
                    ui.separator();
                    ui.label(
                        RichText::new(format!("последний батч: {batch}"))
                            .small()
                            .color(Color32::from_gray(150)),
                    );
                }
            });
        });

        if app.import.running {
            ui.add(
                egui::ProgressBar::new(app.import.fraction())
                    .show_percentage()
                    .animate(true),
            );
            ui.label(
                RichText::new(format!("{} из {} книг", app.import.done, app.import.total))
                    .small()
                    .color(Color32::from_gray(150)),
            );
        }

        if app.import.defects > 0 {
            ui.label(
                RichText::new(format!(
                    "⚠ битых ссылок на картинки в исходных FB2: {} (подробности в stderr)",
                    app.import.defects
                ))
                .small()
                .color(Color32::from_rgb(220, 190, 120)),
            );
        }

        if !app.import.errors.is_empty() {
            ui.collapsing(
                format!("⚠ Пропущено книг: {}", app.import.errors.len()),
                |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(120.0)
                        .show(ui, |ui| {
                            for err in &app.import.errors {
                                ui.label(
                                    RichText::new(err)
                                        .small()
                                        .color(Color32::from_rgb(230, 150, 120)),
                                );
                            }
                        });
                },
            );
        }
        ui.add_space(4.0);
    });

    if hide {
        app.import = crate::ImportState::default();
    }
}

// ---------------------------------------------------------------------------
// Окно ошибки
// ---------------------------------------------------------------------------

fn error_window(app: &mut App, ui: &mut egui::Ui) {
    let Some(message) = app.error.clone() else {
        return;
    };
    // `Window` по-прежнему раскрывается в контекст, а не в `Ui`.
    let ctx = ui.ctx().clone();
    let mut open = true;
    egui::Window::new("Ошибка")
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .open(&mut open)
        .show(&ctx, |ui| {
            ui.label(RichText::new(&message).color(Color32::from_rgb(240, 120, 120)));
            ui.add_space(8.0);
            if ui.button("Закрыть").clicked() {
                app.error = None;
            }
        });
    if !open {
        app.error = None;
    }
}
