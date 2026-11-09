//! Нативный конвертер FB2 → EPUB 3.3.
//!
//! Заменяет внешний `ebook-convert` (Calibre). Никаких дочерних процессов:
//! FB2 разбирается `quick-xml`, EPUB собирается крейтом `zip`, который уже
//! используется в оптимизаторе картинок.
//!
//! Что даёт по сравнению с Calibre:
//!
//! * **EPUB 3.3** — `version="3.3"` в `content.opf`, `nav.xhtml` вместо
//!   устаревшего `toc.ncx`;
//! * **сноски** — `<body name="notes">` превращается в
//!   `<aside epub:type="footnote">`, ссылки — в `<a epub:type="noteref">`;
//!   примечания не создают отдельных глав в оглавлении;
//! * **стиль** — один `css/unified.css`, у заголовков нет ни рамок, ни
//!   фонов, которые Calibre добавляет от себя;
//! * **никакого мусора** — в EPUB попадают только те файлы, которые
//!   действительно нужны: ни старых калибровских стилей, ни шрифтов,
//!   ни `page_styles.css`, ни `titlepage.xhtml`-обвязки.
//!
//! Модуль намеренно не зависит от `worker.rs`: на вход путь к FB2, на выход
//! готовый EPUB. Оптимизация картинок (`images::optimize_epub`) выполняется
//! следующим шагом пайплайна и превращает вложенные JPEG/PNG в WebP.

use anyhow::{bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

// ---------------------------------------------------------------------------
// Минимальное дерево XML
// ---------------------------------------------------------------------------

/// Узел дерева: элемент или текст.
#[derive(Debug, Clone)]
pub enum Node {
    Element(Element),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    /// Атрибут по локальному имени: `href` найдёт и `l:href`, и `xlink:href`.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find_map(|(k, v)| {
            let local = k.rsplit(':').next().unwrap_or(k.as_str());
            (local == name).then_some(v.as_str())
        })
    }

    pub fn kids<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter_map(move |n| match n {
            Node::Element(e) if e.name == name => Some(e),
            _ => None,
        })
    }

    pub fn first(&self, name: &str) -> Option<&Element> {
        self.children.iter().find_map(|n| match n {
            Node::Element(e) if e.name == name => Some(e),
            _ => None,
        })
    }

    pub fn has(&self, name: &str) -> bool {
        self.first(name).is_some()
    }

    /// Весь текст внутри элемента, без разделителей.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    fn collect_text(&self, out: &mut String) {
        for n in &self.children {
            match n {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) => e.collect_text(out),
            }
        }
    }
}

/// Разобрать XML в дерево. Ошибка — а не молча пустой результат.
fn parse_xml(xml: &str) -> Result<Element> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;

    loop {
        match reader.read_event() {
            Err(e) => bail!("не удалось разобрать XML на позиции {}: {e}", reader.buffer_position()),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let el = Element {
                    name: e.name().as_ref().to_string(),
                    attrs: read_attrs(&e)?,
                    children: Vec::new(),
                };
                stack.push(el);
            }
            Ok(Event::Empty(e)) => {
                let el = Element {
                    name: e.name().as_ref().to_string(),
                    attrs: read_attrs(&e)?,
                    children: Vec::new(),
                };
                attach(&mut stack, &mut root, Node::Element(el))?;
            }
            Ok(Event::End(_)) => {
                let Some(el) = stack.pop() else {
                    bail!("лишний закрывающий тег на позиции {}", reader.buffer_position());
                };
                attach(&mut stack, &mut root, Node::Element(el))?;
            }
            Ok(Event::Text(t)) => {
                let decoded = quick_xml::escape::unescape(&t)
                    .with_context(|| format!("битая сущность на позиции {}", reader.buffer_position()))?;
                if !decoded.is_empty() {
                    attach(&mut stack, &mut root, Node::Text(decoded.into_owned()))?;
                }
            }
            Ok(Event::CData(c)) => {
                let raw: &str = &c;
                if !raw.is_empty() {
                    attach(&mut stack, &mut root, Node::Text(raw.to_string()))?;
                }
            }
            // `&amp;` и прочие ссылки quick-xml отдаёт отдельным событием.
            Ok(Event::GeneralRef(r)) => {
                let text = if r.is_char_ref() {
                    r.resolve_char_ref().ok().flatten().map(|c| c.to_string())
                } else {
                    match r.into_inner().as_ref() {
                        "amp" => Some("&".to_string()),
                        "lt" => Some("<".to_string()),
                        "gt" => Some(">".to_string()),
                        "apos" => Some("'".to_string()),
                        "quot" => Some("\"".to_string()),
                        // Незнакомую сущность не выбрасываем: пусть будет видно.
                        other => Some(format!("&{other};")),
                    }
                };
                if let Some(t) = text {
                    attach(&mut stack, &mut root, Node::Text(t))?;
                }
            }
            // Decl, Comment, PI, DocType — для конвертации не нужны
            Ok(_) => {}
        }
    }

    root.context("в FB2 не нашлось корневого элемента")
}

fn read_attrs(e: &quick_xml::events::BytesStart<'_>) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for a in e.attributes() {
        let a = a.context("битый атрибут в FB2")?;
        let key = a.key.as_ref().to_string();
        let value = a
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .context("битое значение атрибута")?
            .into_owned();
        out.push((key, value));
    }
    Ok(out)
}

fn attach(stack: &mut [Element], root: &mut Option<Element>, node: Node) -> Result<()> {
    match stack.last_mut() {
        Some(parent) => {
            // quick-xml рвёт текст на куски вокруг сущностей — склеиваем обратно.
            if let Node::Text(next) = &node {
                if let Some(Node::Text(prev)) = parent.children.last_mut() {
                    prev.push_str(next);
                    return Ok(());
                }
            }
            parent.children.push(node);
        }
        None => {
            let Node::Element(el) = node else {
                return Ok(()); // текст вне корня игнорируем
            };
            if root.is_some() {
                bail!("в FB2 больше одного корневого элемента");
            }
            *root = Some(el);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Разбор FB2 в модель книги
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Book {
    title: String,
    authors: Vec<String>,
    lang: String,
    annotation: String,
    genres: Vec<String>,
    date: String,
    publisher: String,
    city: String,
    isbn: String,
    doc_id: String,
    series: String,
    series_number: String,
    /// Тело книги (первое `<body>` без `name`).
    body: Option<Element>,
    /// Секции из `<body name="notes">`, по одной на примечание.
    notes: Vec<Element>,
    /// `id` бинарника → (media type, байты).
    binaries: HashMap<String, (String, Vec<u8>)>,
    cover_id: Option<String>,
}

impl Book {
    fn parse(root: &Element) -> Result<Self> {
        let mut book = Book::default();

        if let Some(desc) = root.first("description") {
            if let Some(ti) = desc.first("title-info") {
                book.title = ti.first("book-title").map(|e| clean_text(&e.text())).unwrap_or_default();
                book.authors = ti
                    .kids("author")
                    .map(|a| {
                        let mut parts: Vec<String> = Vec::new();
                        for tag in ["first-name", "middle-name", "last-name", "nickname"] {
                            if let Some(v) = a.first(tag) {
                                let v = clean_text(&v.text());
                                if !v.is_empty() {
                                    parts.push(v);
                                }
                            }
                        }
                        parts.join(" ")
                    })
                    .filter(|s| !s.is_empty())
                    .collect();

                let codes: Vec<String> = ti
                    .kids("genre")
                    .map(|g| clean_text(&g.text()))
                    .filter(|s| !s.is_empty())
                    .collect();
                // Неизвестные коды остаются как есть — таблица жанров живёт отдельно.
                book.genres = crate::genres::names(&codes);

                book.lang = ti.first("lang").map(|e| clean_text(&e.text())).unwrap_or_default();
                if book.lang.is_empty() {
                    book.lang = "ru".into();
                }
                if let Some(a) = ti.first("annotation") {
                    book.annotation = render_plain(a);
                }
                if let Some(d) = ti.first("date") {
                    book.date = clean_text(&d.text());
                }
                if let Some(cp) = ti.first("coverpage") {
                    book.cover_id = cp.first("image").and_then(|i| i.attr("href")).map(strip_hash);
                }
            }

            if let Some(si) = desc.first("src-title-info") {
                if let Some(d) = si.first("date") {
                    book.date = clean_text(&d.text());
                }
                if let Some(l) = si.first("lang") {
                    if book.lang.is_empty() {
                        book.lang = clean_text(&l.text());
                    }
                }
                // По спецификации `<coverpage>` живёт в `title-info`, но
                // lib.rus.ec массово кладёт её в `src-title-info` — у книг с
                // переводом так оформлено больше половины обложек. Без этого
                // книга молча остаётся без обложки, хотя в исходнике она есть.
                if book.cover_id.is_none() {
                    if let Some(cp) = si.first("coverpage") {
                        book.cover_id =
                            cp.first("image").and_then(|i| i.attr("href")).map(strip_hash);
                    }
                }
            }

            if let Some(pi) = desc.first("publish-info") {
                book.publisher = pi.first("publisher").map(|e| clean_text(&e.text())).unwrap_or_default();
                book.city = pi.first("city").map(|e| clean_text(&e.text())).unwrap_or_default();
                book.isbn = pi.first("isbn").map(|e| clean_text(&e.text())).unwrap_or_default();
            }

            if let Some(di) = desc.first("document-info") {
                book.doc_id = di.first("id").map(|e| clean_text(&e.text())).unwrap_or_default();
                if let Some(s) = di.first("sequence") {
                    book.series = s.attr("name").unwrap_or_default().trim().to_string();
                    book.series_number = s.attr("number").unwrap_or_default().trim().to_string();
                }
            }
        }

        // Тела: первое без `name` — основное, тела `notes`/`footnotes` — примечания.
        for body in root.kids("body") {
            let name = body.attr("name").unwrap_or("").trim().to_ascii_lowercase();
            if name == "notes" || name == "footnotes" || name == "comments" {
                collect_notes(body, &mut book.notes);
            } else if book.body.is_none() && name.is_empty() {
                book.body = Some(body.clone());
            }
            // остальные тела (например, второе оглавление) просто отбрасываем
        }
        if book.body.is_none() {
            book.body = root.kids("body").next().cloned();
        }

        for b in root.kids("binary") {
            let Some(id) = b.attr("id") else { continue };
            let media = b.attr("content-type").unwrap_or("image/jpeg").trim().to_string();
            let raw: String = b
                .children
                .iter()
                .filter_map(|n| match n {
                    Node::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            let Some(bytes) = decode_base64(&raw) else {
                // Битую картинку не глотаем молча.
                eprintln!("fb2epub: бинарник `{id}` не декодируется из base64, пропущен");
                continue;
            };
            book.binaries.insert(id.to_string(), (media, bytes));
        }

        Ok(book)
    }
}

/// Секции примечаний, включая вложенные.
fn collect_notes(node: &Element, out: &mut Vec<Element>) {
    for sec in node.kids("section") {
        if sec.attr("id").is_some() {
            out.push(sec.clone());
        }
        collect_notes(sec, out);
    }
}

fn strip_hash(s: &str) -> String {
    s.trim().trim_start_matches('#').to_string()
}

/// Приводим пробелы и неразрывные пробелы к обычному виду.
fn clean_text(s: &str) -> String {
    s.replace('\u{a0}', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Текст из размеченного фрагмента (аннотация, заголовок) — без тегов.
fn render_plain(el: &Element) -> String {
    let mut out = String::new();
    for child in &el.children {
        match child {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                let inner = render_plain(e);
                if !inner.trim().is_empty() {
                    if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                        out.push(' ');
                    }
                    out.push_str(&inner);
                }
            }
        }
    }
    out
}

fn decode_base64(raw: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lut = [255u8; 256];
    for (i, c) in TABLE.iter().enumerate() {
        lut[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(raw.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for b in raw.bytes() {
        if b == b'=' || b == b'\n' || b == b'\r' || b == b' ' || b == b'\t' {
            continue;
        }
        let v = lut[b as usize];
        if v == 255 {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Рендер в XHTML
// ---------------------------------------------------------------------------

/// Пункт оглавления с вложенностью.
struct NavNode {
    title: String,
    href: String,
    children: Vec<NavNode>,
}

/// Ссылка на примечание — чтобы поставить обратную ссылку из `notes.xhtml`.
struct NoteRef {
    note: String,
    part: String,
    anchor: String,
}

struct Ctx {
    /// id бинарника → путь внутри EPUB (`images/img0001.jpg`).
    images: HashMap<String, String>,
    /// id примечания → суффикс его якоря в `notes.xhtml`.
    note_anchor: HashMap<String, String>,
    refs: Vec<NoteRef>,
    /// Якоря, которые реально будут в EPUB. Всё остальное — битые ссылки FB2.
    anchors: HashSet<String>,
    missing_notes: Vec<String>,
    missing_anchors: Vec<String>,
    missing_images: Vec<String>,
    part_file: String,
    counter: usize,
}

impl Ctx {
    fn next_id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}{:04}", self.counter)
    }

    fn img_src(&self, id: &str) -> Option<String> {
        self.images.get(id).map(|p| format!("../{p}"))
    }

    /// Ссылка на картинку, которой нет среди `<binary>`.
    fn note_missing_image(&mut self, id: &str) {
        let id = if id.is_empty() { "(без href)" } else { id };
        if !self.missing_images.iter().any(|x| x == id) {
            self.missing_images.push(id.to_string());
        }
    }
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

/// Якорь из произвольного id: только буквы, цифры, `-` и `_`.
fn sanitize_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('x');
    }
    if out.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        out.insert(0, 'x');
    }
    out
}

/// Элементы, у которых `id_attr` действительно печатает `id`. Только их id
/// и считаются существующими якорями.
const ID_ELEMENTS: [&str; 21] = [
    "section", "title", "p", "subtitle", "epigraph", "cite", "text-author", "date",
    "poem", "stanza", "v", "table", "annotation", "image", "strong", "emphasis",
    "strikethrough", "sub", "sup", "code", "style",
];

/// Все id, которые попадут в EPUB: по ним проверяются внутренние ссылки.
fn collect_anchors(el: &Element, out: &mut HashSet<String>) {
    if ID_ELEMENTS.contains(&el.name.as_str()) {
        if let Some(id) = el.attr("id") {
            if !id.trim().is_empty() {
                out.insert(sanitize_id(id));
            }
        }
    }
    for child in &el.children {
        if let Node::Element(e) = child {
            collect_anchors(e, out);
        }
    }
}

/// ` id="…"`, если у элемента он есть. Без этого внутренние ссылки FB2
/// (`<a l:href="#x635428">`) повисают: epubcheck ругается RSC-012.
fn id_attr(el: &Element) -> String {
    match el.attr("id") {
        Some(raw) if !raw.trim().is_empty() => {
            format!(" id=\"{}\"", escape_attr(&sanitize_id(raw)))
        }
        _ => String::new(),
    }
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "subtitle"
            | "empty-line"
            | "poem"
            | "stanza"
            | "v"
            | "cite"
            | "epigraph"
            | "table"
            | "image"
            | "section"
            | "title"
            | "annotation"
            | "date"
            | "text-author"
    )
}

fn render_blocks(children: &[Node], out: &mut String, ctx: &mut Ctx, depth: usize) {
    for child in children {
        let Node::Element(e) = child else { continue };
        render_block(e, out, ctx, depth);
    }
}

fn render_block(el: &Element, out: &mut String, ctx: &mut Ctx, depth: usize) {
    match el.name.as_str() {
        "section" => {
            let path = ctx.next_id("x");
            render_section(el, out, ctx, depth, &path);
        }

        "title" => render_title(el, out, ctx, depth),

        "p" => {
            let class = match el.attr("style") {
                Some("center") => " class=\"center\"",
                _ => "",
            };
            out.push_str(&format!("<p{class}{}>", id_attr(el)));
            render_inline_children(el, out, ctx);
            out.push_str("</p>\n");
        }

        "subtitle" => {
            out.push_str(&format!("<p class=\"subtitle\"{}>", id_attr(el)));
            render_inline_children(el, out, ctx);
            out.push_str("</p>\n");
        }

        "empty-line" => out.push_str("<div class=\"empty-line\"></div>\n"),

        "epigraph" => {
            out.push_str(&format!("<blockquote class=\"epigraph\"{}>\n", id_attr(el)));
            render_blocks(&el.children, out, ctx, depth);
            out.push_str("</blockquote>\n");
        }

        "cite" => {
            out.push_str(&format!("<blockquote class=\"cite\"{}>\n", id_attr(el)));
            render_blocks(&el.children, out, ctx, depth);
            out.push_str("</blockquote>\n");
        }

        "text-author" => {
            out.push_str(&format!("<p class=\"text-author\"{}>", id_attr(el)));
            render_inline_children(el, out, ctx);
            out.push_str("</p>\n");
        }

        "date" => {
            out.push_str(&format!("<p class=\"date\"{}>", id_attr(el)));
            render_inline_children(el, out, ctx);
            out.push_str("</p>\n");
        }

        "poem" => {
            out.push_str(&format!("<div class=\"poem\"{}>\n", id_attr(el)));
            if let Some(t) = el.first("title") {
                render_title(t, out, ctx, depth);
            }
            render_blocks(&el.children, out, ctx, depth);
            out.push_str("</div>\n");
        }

        "stanza" => {
            out.push_str(&format!("<div class=\"stanza\"{}>\n", id_attr(el)));
            if let Some(t) = el.first("title") {
                render_title(t, out, ctx, depth);
            }
            if let Some(s) = el.first("subtitle") {
                out.push_str("<p class=\"subtitle\">");
                render_inline_children(s, out, ctx);
                out.push_str("</p>\n");
            }
            for v in el.kids("v") {
                out.push_str("<p class=\"v\">");
                render_inline_children(v, out, ctx);
                out.push_str("</p>\n");
            }
            for a in el.kids("text-author") {
                out.push_str("<p class=\"text-author\">");
                render_inline_children(a, out, ctx);
                out.push_str("</p>\n");
            }
            out.push_str("</div>\n");
        }

        "v" => {
            out.push_str(&format!("<p class=\"v\"{}>", id_attr(el)));
            render_inline_children(el, out, ctx);
            out.push_str("</p>\n");
        }

        "table" => {
            out.push_str(&format!("<table{}>\n", id_attr(el)));
            render_blocks(&el.children, out, ctx, depth);
            out.push_str("</table>\n");
        }

        "tr" => {
            out.push_str("<tr>");
            render_blocks(&el.children, out, ctx, depth);
            out.push_str("</tr>\n");
        }

        "td" | "th" => {
            out.push_str(&format!("<{}>", el.name));
            render_blocks(&el.children, out, ctx, depth);
            out.push_str(&format!("</{}>", el.name));
        }

        "annotation" => {
            out.push_str(&format!("<div class=\"annotation\"{}>\n", id_attr(el)));
            render_blocks(&el.children, out, ctx, depth);
            out.push_str("</div>\n");
        }

        "image" => render_image(el, out, ctx),

        // Неизвестный блок — не теряем содержимое.
        _ => {
            if is_block(&el.name) {
                render_blocks(&el.children, out, ctx, depth);
            } else {
                out.push_str("<p>");
                render_inline(el, out, ctx);
                out.push_str("</p>\n");
            }
        }
    }
}

/// id секции: родной из FB2, а если его нет — из пути в документе.
fn section_key(sec: &Element, path: &str) -> String {
    match sec.attr("id") {
        Some(raw) => sanitize_id(raw),
        None => format!("sec{path}"),
    }
}

/// Отрисовать секцию целиком и вернуть её id. Дети обходятся один раз,
/// иначе заголовки, эпиграфы и иллюстрации дублировались бы.
fn render_section(
    sec: &Element,
    out: &mut String,
    ctx: &mut Ctx,
    depth: usize,
    path: &str,
) -> String {
    let id = section_key(sec, path);
    out.push_str(&format!("<section id=\"{}\">\n", escape_attr(&id)));

    let mut n = 0usize;
    for child in &sec.children {
        let Node::Element(e) = child else { continue };
        if e.name == "section" {
            n += 1;
            render_section(e, out, ctx, depth + 1, &format!("{path}-{n}"));
        } else if e.name == "title" {
            render_title(e, out, ctx, depth);
        } else {
            render_block(e, out, ctx, depth + 1);
        }
    }

    out.push_str("</section>\n");
    id
}

fn render_title(t: &Element, out: &mut String, ctx: &mut Ctx, depth: usize) {
    let level = (depth + 1).min(4);
    let mut lines: Vec<String> = Vec::new();
    for p in t.kids("p") {
        let mut s = String::new();
        render_inline_children(p, &mut s, ctx);
        lines.push(s);
    }
    if lines.is_empty() {
        let mut s = String::new();
        render_inline_children(t, &mut s, ctx);
        lines.push(s);
    }
    let mut it = lines.into_iter();
    if let Some(first) = it.next() {
        out.push_str(&format!(
            "<h{level} class=\"chapter-title\"{}>{first}</h{level}>\n",
            id_attr(t)
        ));
        for rest in it {
            out.push_str(&format!("<p class=\"subtitle\">{rest}</p>\n"));
        }
    }
}

fn render_image(el: &Element, out: &mut String, ctx: &mut Ctx) {
    let alt = el.attr("alt").unwrap_or("").to_string();
    let Some(href) = el.attr("href") else {
        out.push_str("<div class=\"empty-line\"></div>\n");
        return;
    };
    let id = strip_hash(href);
    match ctx.img_src(&id) {
        Some(src) => out.push_str(&format!(
            "<div class=\"illustration\"{}><img src=\"{}\" alt=\"{}\"/></div>\n",
            id_attr(el),
            escape_attr(&src),
            escape_attr(&alt)
        )),
        None => {
            // Картинки нет в бинарниках — не выдумываем ссылку в пустоту,
            // но и не молчим: дефект попадёт в отчёт.
            ctx.note_missing_image(&id);
            out.push_str(&format!(
                "<p class=\"missing-image\">[иллюстрация: {}]</p>\n",
                escape_text(&alt)
            ));
        }
    }
}

fn render_inline_children(el: &Element, out: &mut String, ctx: &mut Ctx) {
    for child in &el.children {
        match child {
            Node::Text(t) => out.push_str(&escape_text(t)),
            Node::Element(e) => render_inline(e, out, ctx),
        }
    }
}

fn render_inline(el: &Element, out: &mut String, ctx: &mut Ctx) {
    match el.name.as_str() {
        "strong" => wrap("strong", el, out, ctx),
        "emphasis" => wrap("em", el, out, ctx),
        "strikethrough" => wrap("s", el, out, ctx),
        "sub" => wrap("sub", el, out, ctx),
        "sup" => wrap("sup", el, out, ctx),
        "code" => wrap("code", el, out, ctx),

        "style" => {
            let name = el.attr("name").unwrap_or("").trim();
            if name.is_empty() {
                render_inline_children(el, out, ctx);
            } else {
                out.push_str(&format!(
                    "<span class=\"style-{}\"{}>",
                    escape_attr(&sanitize_id(name)),
                    id_attr(el)
                ));
                render_inline_children(el, out, ctx);
                out.push_str("</span>");
            }
        }

        "image" => {
            let alt = el.attr("alt").unwrap_or("").to_string();
            let id = el.attr("href").map(strip_hash).unwrap_or_default();
            match ctx.img_src(&id) {
                Some(src) => out.push_str(&format!(
                    "<img class=\"inline\" src=\"{}\" alt=\"{}\"/>",
                    escape_attr(&src),
                    escape_attr(&alt)
                )),
                None => {
                    ctx.note_missing_image(&id);
                    out.push_str(&escape_text(&format!("[{}]", alt)));
                }
            }
        }

        "a" => render_link(el, out, ctx),

        _ => {
            // Внутри абзаца неизвестный тег — просто прозрачный контейнер.
            render_inline_children(el, out, ctx);
        }
    }
}

fn wrap(tag: &str, el: &Element, out: &mut String, ctx: &mut Ctx) {
    out.push_str(&format!("<{tag}{}>", id_attr(el)));
    render_inline_children(el, out, ctx);
    out.push_str(&format!("</{tag}>"));
}

/// Ссылки: сноски — `<a epub:type="noteref">`, остальное — обычные ссылки.
fn render_link(el: &Element, out: &mut String, ctx: &mut Ctx) {
    let href = el.attr("href").unwrap_or("").trim().to_string();
    let is_note = el.attr("type").map(|t| t.eq_ignore_ascii_case("note")).unwrap_or(false);

    if href.starts_with('#') {
        let target = strip_hash(&href);
        if is_note {
            if let Some(anchor) = ctx.note_anchor.get(&target).cloned() {
                let anchor_id = format!("nr-{anchor}");
                out.push_str(&format!(
                    "<a class=\"noteref\" epub:type=\"noteref\" id=\"{}\" href=\"notes.xhtml#fn-{}\">",
                    escape_attr(&anchor_id),
                    escape_attr(&anchor)
                ));
                render_inline_children(el, out, ctx);
                out.push_str("</a>");
                // `notes.xhtml` лежит в той же папке `text/`, поэтому путь —
                // только имя файла, без каталога.
                let part = ctx
                    .part_file
                    .rsplit('/')
                    .next()
                    .unwrap_or(&ctx.part_file)
                    .to_string();
                ctx.refs.push(NoteRef {
                    note: target,
                    part,
                    anchor: anchor_id,
                });
                return;
            }
            // Примечания с таким id в книге нет — фиксируем как дефект FB2.
            if !ctx.missing_notes.contains(&target) {
                ctx.missing_notes.push(target.clone());
            }
        }
        // Внутренняя ссылка. Если такого якоря в FB2 нет (`#635428`,
        // `#botbybase_bot?startgroup=1` — мусор от источника), ставим просто
        // текст: болтающаяся ссылка хуже, чем её отсутствие (epubcheck RSC-012).
        let anchor = sanitize_id(&target);
        if ctx.anchors.contains(&anchor) {
            out.push_str(&format!("<a href=\"#{}\">", escape_attr(&anchor)));
            render_inline_children(el, out, ctx);
            out.push_str("</a>");
        } else {
            if !ctx.missing_anchors.contains(&target) {
                ctx.missing_anchors.push(target.clone());
            }
            render_inline_children(el, out, ctx);
        }
        return;
    }

    if href.is_empty() {
        render_inline_children(el, out, ctx);
        return;
    }

    // Внешняя ссылка. fb2 часто пишет href без схемы (например, `www.lib.ru`).
    let url = if href.contains("://") || href.starts_with("mailto:") {
        href
    } else {
        format!("http://{href}")
    };
    out.push_str(&format!("<a href=\"{}\">", escape_attr(&url)));
    render_inline_children(el, out, ctx);
    out.push_str("</a>");
}

// ---------------------------------------------------------------------------
// Разбиение на части
// ---------------------------------------------------------------------------

/// Секция начинает новую часть, если у неё есть заголовок или собственный текст.
fn is_section_start(sec: &Element) -> bool {
    if sec.has("title") {
        return true;
    }
    sec.children.iter().any(|n| match n {
        Node::Element(e) => matches!(
            e.name.as_str(),
            "p" | "poem" | "subtitle" | "cite" | "table" | "image" | "empty-line" | "annotation"
        ),
        Node::Text(t) => !t.trim().is_empty(),
    })
}

/// Собрать части: каждая — список элементов, которые попадут в один XHTML-файл.
/// Секции-контейнеры без заголовка и текста раскрываются, чтобы не плодить
/// пустые страницы.
fn collect_parts<'a>(
    children: &'a [Node],
    current: &mut Vec<&'a Element>,
    parts: &mut Vec<Vec<&'a Element>>,
) {
    for n in children {
        let Node::Element(e) = n else { continue };
        if e.name == "section" {
            if is_section_start(e) {
                // Заголовок книги и эпиграфы не отделяем от первой главы:
                // иначе получается страница из одной строки.
                if current.iter().any(|c| c.name == "section") {
                    parts.push(std::mem::take(current));
                }
                current.push(e);
            } else {
                collect_parts(&e.children, current, parts);
            }
        } else {
            current.push(e);
        }
    }
}

// ---------------------------------------------------------------------------
// Сборка EPUB
// ---------------------------------------------------------------------------

/// Отчёт о конвертации — попадает в статистику импорта.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConvertReport {
    pub parts: usize,
    pub notes: usize,
    pub images: usize,
    /// Ссылки на примечания, которых нет в `<body name="notes">`.
    pub missing_notes: Vec<String>,
    /// Иллюстрации, на которые FB2 ссылается, но бинарника в нём нет.
    pub missing_images: Vec<String>,
    /// Внутренние ссылки на якоря, которых в FB2 нет.
    pub missing_anchors: Vec<String>,
}

/// Старшая половина таблицы Windows-1251 (коды 0x80..0xBF).
/// Коды 0xC0..0xFF — сплошной блок `А`..`я` и считаются формулой.
const CP1251_HIGH: [char; 64] = [
    '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}', '\u{040B}', '\u{040F}',
    '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{0098}', '\u{2122}', '\u{0459}', '\u{203A}', '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}',
    '\u{00A0}', '\u{040E}', '\u{045E}', '\u{0408}', '\u{00A4}', '\u{0490}', '\u{00A6}', '\u{00A7}',
    '\u{0401}', '\u{00A9}', '\u{0404}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{0407}',
    '\u{00B0}', '\u{00B1}', '\u{0406}', '\u{0456}', '\u{0491}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{0451}', '\u{2116}', '\u{0454}', '\u{00BB}', '\u{0458}', '\u{0405}', '\u{0455}', '\u{00BF}',
];

/// Перекодировать Windows-1251 в UTF-8.
fn decode_cp1251(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 8);
    for &b in bytes {
        match b {
            0x00..=0x7F => out.push(b as char),
            0x80..=0xBF => out.push(CP1251_HIGH[(b - 0x80) as usize]),
            // 0xC0..=0xFF — А(0x410)..я(0x44F) подряд.
            _ => out.push(char::from_u32(0x0410 + (b as u32 - 0xC0)).unwrap_or('\u{FFFD}')),
        }
    }
    out
}

/// Объявленная в прологе кодировка, в нижнем регистре.
fn declared_encoding(bytes: &[u8]) -> Option<String> {
    // Пролог короткий: смотрим только первые 400 байт.
    let head = &bytes[..bytes.len().min(400)];
    let needle = b"encoding";
    let pos = head
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))?;
    let rest = &head[pos + needle.len()..];
    let eq = rest.iter().position(|&b| b == b'=')?;
    let rest = &rest[eq + 1..];
    let quote = *rest.iter().find(|b| **b == b'"' || **b == b'\'')?;
    let start = rest.iter().position(|&b| b == quote)? + 1;
    let end = rest[start..].iter().position(|&b| b == quote)? + start;
    Some(
        String::from_utf8_lossy(&rest[start..end])
            .trim()
            .to_ascii_lowercase(),
    )
}

/// Привести текст FB2 к UTF-8.
///
/// Спецификация FB2 разрешает любую кодировку, лишь бы она была объявлена в
/// прологе, и lib.rus.ec этим пользовался: в корпусе из 2017 книг 6 приходят
/// Сколько букв каждой письменности встретилось в тексте книги.
///
/// Нужна, чтобы отличить русскую книгу от нерусской **по самому тексту**, а не
/// по названию: в корпусе полно русских книг с английским заголовком и
/// английской аннотацией, и по метаданным их не отличить.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScriptStats {
    /// Буквы кириллицы.
    pub cyrillic: u64,
    /// Буквы латиницы.
    pub latin: u64,
    /// Буквы греческого алфавита.
    pub greek: u64,
    /// Буквы всех прочих письменностей.
    pub other: u64,
    /// Кириллические буквы, которых нет в русском алфавите, но которые есть
    /// в украинском, белорусском или сербском (`і`, `ї`, `є`, `ґ`, `ў`, `ђ`…).
    pub foreign_cyrillic: u64,
}

impl ScriptStats {
    /// Всего букв — без цифр, знаков препинания и пробелов.
    pub fn letters(&self) -> u64 {
        self.cyrillic + self.latin + self.greek + self.other
    }

    /// Доля кириллицы среди всех букв.
    pub fn cyrillic_share(&self) -> f64 {
        let n = self.letters();
        if n == 0 {
            0.0
        } else {
            self.cyrillic as f64 / n as f64
        }
    }

    /// Доля «нерусских» букв среди кириллицы.
    pub fn foreign_share(&self) -> f64 {
        if self.cyrillic == 0 {
            0.0
        } else {
            self.foreign_cyrillic as f64 / self.cyrillic as f64
        }
    }
}

/// Кириллические буквы, отсутствующие в русском алфавите.
const FOREIGN_CYRILLIC: [char; 11] = [
    'і', 'ї', 'є', 'ґ', // украинские
    'ў', // белорусская
    'ђ', 'ј', 'љ', 'њ', 'ћ', 'џ', // сербские
];

/// Посчитать буквы по письменностям в тексте FB2.
///
/// Служебные блоки выбрасываются **до** подсчёта: `<binary>` — это картинки в
/// base64, то есть сплошная латиница, и без этого любая книга с иллюстрациями
/// выглядела бы английской. Теги, атрибуты и сущности (`&#257;`) тоже не
/// считаются — иначе разметка перевесила бы текст.
pub fn script_stats(bytes: &[u8]) -> ScriptStats {
    let text = decode_fb2(bytes);
    let mut st = ScriptStats::default();
    let mut in_tag = false;
    let mut in_binary = false;
    let mut tag = String::new();

    for ch in text.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let t = tag.to_ascii_lowercase();
                let t = t.trim();
                let closing = t.starts_with('/');
                let name = t
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("");
                if name == "binary" {
                    in_binary = !closing;
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            tag.clear();
            continue;
        }
        if in_binary {
            continue;
        }
        let c = ch.to_lowercase().next().unwrap_or(ch);
        if ('\u{0400}'..='\u{04FF}').contains(&c) {
            st.cyrillic += 1;
            if FOREIGN_CYRILLIC.contains(&c) {
                st.foreign_cyrillic += 1;
            }
        } else if c.is_ascii_alphabetic() {
            st.latin += 1;
        } else if ('\u{0370}'..='\u{03FF}').contains(&c) {
            st.greek += 1;
        } else if c.is_alphabetic() {
            st.other += 1;
        }
    }
    st
}

/// в `windows-1251`. Раньше они молча выпадали из импорта с ошибкой
/// «stream did not contain valid UTF-8».
///
/// Порядок проверок важен: сначала честная проверка на валидный UTF-8, потому
/// что объявление в прологе бывает устаревшим, а испортить им корректный файл
/// нельзя. И только если файл не UTF-8 — верим объявлению.
pub fn decode_fb2(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => return text.to_string(),
        // Обрыв ровно на границе символа — это обрезанный префикс файла, а не
        // чужая кодировка: считаем UTF-8 и просто теряем последний символ.
        Err(err) if err.valid_up_to() + 3 >= bytes.len() => {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        Err(_) => {}
    }
    match declared_encoding(bytes).as_deref() {
        // Однобайтовые западные кодировки: старший байт как есть.
        Some("iso-8859-1") | Some("latin1") | Some("windows-1252") | Some("cp1252") => {
            bytes.iter().map(|&b| b as char).collect()
        }
        // Всё остальное, включая незнакомые и необъявленные, — cp1251:
        // корпус русский, и это самый вероятный вариант.
        _ => decode_cp1251(bytes),
    }
}

/// Сконвертировать FB2 в EPUB 3.3. Возвращает отчёт; ошибка вместо пустого файла.
pub fn convert(fb2: &Path, out: &Path) -> Result<ConvertReport> {
    let bytes = fs::read(fb2).with_context(|| format!("не удалось прочитать {}", fb2.display()))?;
    convert_bytes(&bytes, fb2, out)
}

/// То же, но байты FB2 уже прочитаны вызывающим.
///
/// `fb2` нужен только для сообщений об ошибках: имя исходника в них полезнее,
/// чем имя временного EPUB.
///
/// Нужно, когда файл пришлось прочитать раньше (проверка языка): FB2 на 20 МБ
/// читался дважды — один раз ради статистики письма, второй раз здесь.
pub fn convert_bytes(bytes: &[u8], fb2: &Path, out: &Path) -> Result<ConvertReport> {
    let xml = decode_fb2(bytes);
    let root = parse_xml(&xml).with_context(|| format!("{}: не разобран FB2", fb2.display()))?;
    if root.name != "FictionBook" {
        bail!("{}: корневой элемент `{}`, а не `FictionBook`", fb2.display(), root.name);
    }
    let book = Book::parse(&root)?;

    // ---- картинки: имена файлов и обложка ---------------------------------
    let mut images: HashMap<String, String> = HashMap::new();
    let mut image_entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut n = 0usize;
    let mut ordered: Vec<(&String, &(String, Vec<u8>))> = book.binaries.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    for (id, (media, bytes)) in ordered {
        // Формат — из байтов: в FB2 `<binary content-type="image/png">` часто
        // лежит JPEG, и доверять заявленному типу нельзя (epubcheck: OPF-029).
        let ext = match crate::images::sniff_image(bytes) {
            Some((ext, _)) => ext,
            None => extension_for(media),
        };
        let name = if book.cover_id.as_deref() == Some(id.as_str()) {
            format!("cover.{ext}")
        } else {
            n += 1;
            format!("img{n:04}.{ext}")
        };
        let path = format!("images/{name}");
        images.insert(id.clone(), path.clone());
        image_entries.push((path, bytes.clone()));
    }
    let cover_path = book
        .cover_id
        .as_ref()
        .and_then(|id| images.get(id))
        .cloned();

    // ---- примечания: якоря --------------------------------------------------
    let mut note_anchor: HashMap<String, String> = HashMap::new();
    for sec in &book.notes {
        let raw = sec.attr("id").unwrap_or("").to_string();
        let base = sanitize_id(&raw);
        let mut candidate = base.clone();
        let mut k = 1;
        while note_anchor.values().any(|v| *v == candidate) {
            k += 1;
            candidate = format!("{base}_{k}");
        }
        note_anchor.insert(raw, candidate);
    }

    // Якоря собираем заранее: по ним проверяются внутренние ссылки.
    let mut anchors: HashSet<String> = HashSet::new();
    if let Some(body) = &book.body {
        collect_anchors(body, &mut anchors);
    }
    for sec in &book.notes {
        collect_anchors(sec, &mut anchors);
    }

    let mut ctx = Ctx {
        images,
        note_anchor,
        anchors,
        refs: Vec::new(),
        missing_notes: Vec::new(),
        missing_anchors: Vec::new(),
        missing_images: Vec::new(),
        part_file: String::new(),
        counter: 0,
    };

    // ---- части --------------------------------------------------------------
    let mut raw_parts: Vec<Vec<&Element>> = Vec::new();
    let mut current: Vec<&Element> = Vec::new();
    if let Some(body) = &book.body {
        collect_parts(&body.children, &mut current, &mut raw_parts);
    }
    if !current.is_empty() {
        raw_parts.push(current);
    }
    if raw_parts.is_empty() {
        bail!("{}: в FB2 нет текста для конвертации", fb2.display());
    }

    let mut nav: Vec<NavNode> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    for (i, elems) in raw_parts.iter().enumerate() {
        let file = format!("text/part{:04}.xhtml", i + 1);
        ctx.part_file = file.clone();

        let mut body = String::new();
        for (j, el) in elems.iter().enumerate() {
            if el.name == "section" {
                // Путь секции делает id детерминированным: одно и то же значение
                // получают и разметка, и оглавление — без общего счётчика.
                let path = (j + 1).to_string();
                let id = render_section(el, &mut body, &mut ctx, 0, &path);
                let title = section_title(el);
                let children = sub_nav(el, &file, &path);
                if title.is_empty() {
                    nav.extend(children);
                } else {
                    nav.push(NavNode {
                        title,
                        href: format!("{file}#{id}"),
                        children,
                    });
                }
            } else {
                render_block(el, &mut body, &mut ctx, 0);
            }
        }
        // Пустая часть (одна разметка без текста) — не создаём файл.
        if body.trim().is_empty() {
            continue;
        }
        files.push((file, xhtml_page(&page_title(elems), &body, CSS_FROM_TEXT)));
    }

    // ---- notes.xhtml --------------------------------------------------------
    let mut notes_body = String::new();
    for sec in &book.notes {
        let raw = sec.attr("id").unwrap_or("").to_string();
        let Some(anchor) = ctx.note_anchor.get(&raw).cloned() else { continue };
        notes_body.push_str(&format!(
            "<aside class=\"footnote\" epub:type=\"footnote\" id=\"fn-{}\">\n",
            escape_attr(&anchor)
        ));
        // Номер примечания из <title> становится ссылкой назад в текст.
        let label = sec.first("title").map(render_plain).unwrap_or_default();
        let label = clean_text(&label);
        let back = ctx
            .refs
            .iter()
            .find(|r| r.note == raw)
            .map(|r| format!("{}#{}", r.part, r.anchor));
        match back {
            Some(target) => notes_body.push_str(&format!(
                "<p class=\"note-title\"><a epub:type=\"backlink\" href=\"{}\">{}</a></p>\n",
                escape_attr(&target),
                escape_text(if label.is_empty() { "↩" } else { &label })
            )),
            None => {
                if !label.is_empty() {
                    notes_body.push_str(&format!("<p class=\"note-title\">{}</p>\n", escape_text(&label)));
                }
            }
        }
        for child in &sec.children {
            let Node::Element(e) = child else { continue };
            if e.name == "title" {
                continue;
            }
            render_block(e, &mut notes_body, &mut ctx, 1);
        }
        notes_body.push_str("</aside>\n");
    }
    let notes_count = book.notes.len();
    let has_notes = !notes_body.is_empty();
    if has_notes {
        files.push((
            "text/notes.xhtml".to_string(),
            xhtml_page(
                "Примечания",
                &format!(
                    "<section epub:type=\"footnotes\" role=\"doc-endnotes\">\n<h1 class=\"chapter-title\">Примечания</h1>\n{notes_body}</section>\n"
                ),
                CSS_FROM_TEXT,
            ),
        ));
    }

    // ---- nav.xhtml ----------------------------------------------------------
    let toc = render_nav(&nav);
    let mut landmarks = String::new();
    if let Some((first, _)) = files.first() {
        landmarks.push_str(&format!(
            "<li><a epub:type=\"bodymatter\" href=\"{}\">Начало</a></li>\n",
            escape_attr(first)
        ));
    }
    if has_notes {
        landmarks.push_str("<li><a epub:type=\"endnotes\" href=\"text/notes.xhtml\">Примечания</a></li>\n");
    }
    let nav_doc = xhtml_page(
        "Оглавление",
        &format!(
            "<nav epub:type=\"toc\" id=\"toc\">\n<h1 class=\"chapter-title\">Оглавление</h1>\n{toc}</nav>\n\
             <nav epub:type=\"landmarks\" hidden=\"hidden\">\n<ol>\n{landmarks}</ol>\n</nav>\n"
        ),
        CSS_FROM_ROOT,
    );

    // ---- content.opf --------------------------------------------------------
    let uuid = uuid_for(&book, fb2);
    let opf = build_opf(&book, &uuid, &files, &image_entries[..], cover_path.as_deref());
    let container = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\n",
        "  <rootfiles>\n",
        // OCF требует именно `application/oebps-package+xml`, а не epub+zip:
        // с последним epubcheck ругается RSC-003.
        "    <rootfile full-path=\"content.opf\" media-type=\"application/oebps-package+xml\"/>\n",
        "  </rootfiles>\n",
        "</container>\n"
    );

    // ---- запись -------------------------------------------------------------
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).with_context(|| format!("не удалось создать {}", dir.display()))?;
    }
    let io = fs::File::create(out).with_context(|| format!("не удалось создать {}", out.display()))?;
    let mut zip = ZipWriter::new(io);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // mimetype обязан идти первым и без сжатия — требование OCF.
    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;
    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(container.as_bytes())?;
    zip.start_file("content.opf", deflated)?;
    zip.write_all(opf.as_bytes())?;
    zip.start_file("nav.xhtml", deflated)?;
    zip.write_all(nav_doc.as_bytes())?;
    zip.start_file("css/unified.css", deflated)?;
    zip.write_all(UNIFIED_CSS.as_bytes())?;
    if let Some(cover) = &cover_path {
        let cover_page = xhtml_page(
            "Обложка",
            &format!(
                "<div class=\"cover-page\"><img class=\"cover\" src=\"../{}\" alt=\"Обложка\"/></div>\n",
                escape_attr(cover)
            ),
            CSS_FROM_TEXT,
        );
        zip.start_file("text/cover.xhtml", deflated)?;
        zip.write_all(cover_page.as_bytes())?;
    }
    for (name, page) in &files {
        zip.start_file(name.as_str(), deflated)?;
        zip.write_all(page.as_bytes())?;
    }
    for (name, bytes) in &image_entries {
        zip.start_file(name.as_str(), deflated)?;
        zip.write_all(bytes)?;
    }
    zip.finish().context("не удалось дописать EPUB")?;

    Ok(ConvertReport {
        parts: files.len(),
        notes: notes_count,
        images: image_entries.len(),
        missing_notes: std::mem::take(&mut ctx.missing_notes),
        missing_images: std::mem::take(&mut ctx.missing_images),
        missing_anchors: std::mem::take(&mut ctx.missing_anchors),
    })
}

fn section_title(sec: &Element) -> String {
    sec.first("title").map(|t| clean_text(&render_plain(t))).unwrap_or_default()
}

fn page_title(elems: &[&Element]) -> String {
    for el in elems {
        if el.name == "section" {
            let t = section_title(el);
            if !t.is_empty() {
                return t;
            }
        }
    }
    "Текст".to_string()
}

/// Вложенные пункты оглавления. Путь считается так же, как в `render_section`,
/// поэтому ссылки указывают на существующие якоря.
fn sub_nav(sec: &Element, file: &str, path: &str) -> Vec<NavNode> {
    let mut out = Vec::new();
    let mut n = 0usize;
    for child in &sec.children {
        let Node::Element(e) = child else { continue };
        if e.name != "section" {
            continue;
        }
        n += 1;
        let child_path = format!("{path}-{n}");
        let id = section_key(e, &child_path);
        let title = section_title(e);
        let children = sub_nav(e, file, &child_path);
        if title.is_empty() {
            out.extend(children);
        } else {
            out.push(NavNode {
                title,
                href: format!("{file}#{id}"),
                children,
            });
        }
    }
    out
}

fn render_nav(nodes: &[NavNode]) -> String {
    fn emit(nodes: &[NavNode], out: &mut String) {
        if nodes.is_empty() {
            return;
        }
        out.push_str("<ol>\n");
        for n in nodes {
            out.push_str(&format!(
                "<li><a href=\"{}\">{}</a>",
                escape_attr(&n.href),
                escape_text(&n.title)
            ));
            emit(&n.children, out);
            out.push_str("</li>\n");
        }
        out.push_str("</ol>\n");
    }
    let mut out = String::new();
    emit(nodes, &mut out);
    if out.is_empty() {
        out.push_str("<ol>\n<li><a href=\"text/part0001.xhtml\">Текст</a></li>\n</ol>\n");
    }
    out
}

fn extension_for(media: &str) -> &'static str {
    match media.trim().to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/svg+xml" => "svg",
        _ => "jpg",
    }
}

/// Страницы лежат в `text/`, поэтому путь к стилю у них разный.
/// `css_href` передаётся явно — иначе `nav.xhtml` в корне ссылается в никуда.
fn xhtml_page(title: &str, body: &str, css_href: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <!DOCTYPE html>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\" \
         xmlns:epub=\"http://www.idpf.org/2007/ops\" xml:lang=\"ru\" lang=\"ru\">\n\
         <head>\n<meta charset=\"utf-8\"/>\n<title>{}</title>\n\
         <link rel=\"stylesheet\" type=\"text/css\" href=\"{}\"/>\n</head>\n\
         <body>\n{}</body>\n</html>\n",
        escape_text(title),
        escape_attr(css_href),
        body
    )
}

/// Стиль для страниц внутри `text/`.
const CSS_FROM_TEXT: &str = "../css/unified.css";
/// Стиль для документов в корне EPUB.
const CSS_FROM_ROOT: &str = "css/unified.css";

/// `dc:date` в EPUB 3 должен быть по ISO 8601, а в FB2 дата — свободный
/// текст: «2026-09-01 00:43», «1863-1867», «неизвестно». Берём ведущую дату.
fn iso_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    let chars: Vec<char> = s.chars().take(10).collect();
    if chars.len() < 4 || !chars[..4].iter().all(char::is_ascii_digit) {
        return None;
    }
    let year: u32 = s[..4].parse().ok()?;
    if !(1000..=2999).contains(&year) {
        return None;
    }
    let mut out = s[..4].to_string();
    let b = s.as_bytes();
    if b.len() >= 7 && b[4] == b'-' && b[5].is_ascii_digit() && b[6].is_ascii_digit() {
        let month: u32 = s[5..7].parse().ok()?;
        if (1..=12).contains(&month) {
            out.push('-');
            out.push_str(&s[5..7]);
            if b.len() >= 10 && b[7] == b'-' && b[8].is_ascii_digit() && b[9].is_ascii_digit() {
                let day: u32 = s[8..10].parse().ok()?;
                if (1..=31).contains(&day) {
                    out.push('-');
                    out.push_str(&s[8..10]);
                }
            }
        }
    }
    Some(out)
}

fn build_opf(
    book: &Book,
    uuid: &str,
    files: &[(String, String)],
    images: &[(String, Vec<u8>)],
    cover_path: Option<&str>,
) -> String {
    let mut manifest = String::new();
    manifest.push_str("<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n");
    manifest.push_str("<item id=\"css\" href=\"css/unified.css\" media-type=\"text/css\"/>\n");

    let mut spine = String::new();
    if cover_path.is_some() {
        manifest.push_str("<item id=\"cover-page\" href=\"text/cover.xhtml\" media-type=\"application/xhtml+xml\"/>\n");
        spine.push_str("<itemref idref=\"cover-page\"/>\n");
    }
    for (i, (name, _)) in files.iter().enumerate() {
        manifest.push_str(&format!(
            "<item id=\"p{i}\" href=\"{}\" media-type=\"application/xhtml+xml\"/>\n",
            escape_attr(name)
        ));
        spine.push_str(&format!("<itemref idref=\"p{i}\"/>\n"));
    }
    spine.push_str("<itemref idref=\"nav\"/>\n");

    for (i, (name, _)) in images.iter().enumerate() {
        // Расширение уже отражает настоящий формат (см. `images::sniff_image`),
        // поэтому и media-type берётся из него — они не могут разойтись.
        let media = match name.rsplit('.').next().unwrap_or("jpg") {
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "svg" => "image/svg+xml",
            "tif" | "tiff" => "image/tiff",
            _ => "image/jpeg",
        };
        let props = if Some(name.as_str()) == cover_path {
            " properties=\"cover-image\""
        } else {
            ""
        };
        manifest.push_str(&format!(
            "<item id=\"i{i}\" href=\"{}\" media-type=\"{}\"{}/>\n",
            escape_attr(name),
            media,
            props
        ));
    }

    let mut metadata = String::new();
    metadata.push_str(&format!("<dc:identifier id=\"book-id\">{}</dc:identifier>\n", escape_text(uuid)));
    metadata.push_str(&format!("<dc:title>{}</dc:title>\n", escape_text(&book.title)));
    for a in &book.authors {
        metadata.push_str(&format!("<dc:creator>{}</dc:creator>\n", escape_text(a)));
    }
    metadata.push_str(&format!("<dc:language>{}</dc:language>\n", escape_text(&bcp47(&book.lang))));
    if let Some(date) = iso_date(&book.date) {
        metadata.push_str(&format!("<dc:date>{date}</dc:date>\n"));
    }
    if !book.publisher.is_empty() {
        metadata.push_str(&format!("<dc:publisher>{}</dc:publisher>\n", escape_text(&book.publisher)));
    }
    if !book.annotation.is_empty() {
        metadata.push_str(&format!("<dc:description>{}</dc:description>\n", escape_text(&book.annotation)));
    }
    if !book.isbn.is_empty() {
        metadata.push_str(&format!("<dc:identifier>ISBN {}</dc:identifier>\n", escape_text(&book.isbn)));
    }
    if !book.series.is_empty() {
        let mut v = book.series.clone();
        if !book.series_number.is_empty() {
            v.push_str(&format!(" #{}", book.series_number));
        }
        metadata.push_str(&format!(
            "<meta property=\"belongs-to-collection\" id=\"series\">{}</meta>\n\
             <meta refines=\"#series\" property=\"collection-type\">series</meta>\n",
            escape_text(&v)
        ));
    }
    for g in &book.genres {
        metadata.push_str(&format!("<dc:subject>{}</dc:subject>\n", escape_text(g)));
    }
    metadata.push_str(&format!(
        "<meta property=\"dcterms:modified\">{}</meta>\n",
        now_iso8601()
    ));
    if cover_path.is_some() {
        // Совместимость с читалками, которые ищут обложку по EPUB 2-соглашению.
        metadata.push_str("<meta name=\"cover\" content=\"cover-page\"/>\n");
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.3\" unique-identifier=\"book-id\" \
         xml:lang=\"{lang}\">\n\
         <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n{metadata}</metadata>\n\
         <manifest>\n{manifest}</manifest>\n\
         <spine>\n{spine}</spine>\n\
         </package>\n",
        lang = escape_attr(&bcp47(&book.lang)),
        metadata = metadata,
        manifest = manifest,
        spine = spine
    )
}

/// Язык для `dc:language`: берём часть до дефиса, если тег не похож на BCP47.
fn bcp47(lang: &str) -> String {
    let l = lang.trim();
    if l.is_empty() {
        return "ru".into();
    }
    let head = l.split(['-', '_']).next().unwrap_or(l);
    if head.len() == 2 && head.chars().all(|c| c.is_ascii_alphabetic()) {
        head.to_ascii_lowercase()
    } else {
        "ru".into()
    }
}

/// `dcterms:modified` обязателен для EPUB 3 и должен быть в UTC.
pub(crate) fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Дни от 1970-01-01 → (год, месяц, день). Алгоритм Говарда Хиннанта.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Детерминированный `urn:uuid` — один и тот же FB2 даёт один и тот же EPUB.
fn uuid_for(book: &Book, fb2: &Path) -> String {
    let mut seed = String::new();
    seed.push_str(&book.doc_id);
    seed.push('|');
    seed.push_str(&book.title);
    seed.push('|');
    seed.push_str(&book.authors.join(","));
    seed.push('|');
    seed.push_str(&fb2.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());

    let a = fnv1a(seed.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let b = fnv1a(seed.as_bytes(), 0x9e37_79b9_7f4a_7c15);
    let hex = format!("{a:016x}{b:016x}");
    format!(
        "urn:uuid:{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

fn fnv1a(data: &[u8], seed: u64) -> u64 {
    let mut h = seed;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Единственный стиль книги. Никаких калибровских рамок вокруг заголовков.
const UNIFIED_CSS: &str = r#"/* Единый стиль книг библиотеки. Один файл на книгу, без наследия Calibre. */

/* Без этого объявления селектор `[epub|type]` невалиден и браузер молча
   выбрасывает правило — то есть стиль «есть», но не применяется. */
@namespace epub url("http://www.idpf.org/2007/ops");

html {
  font-size: 100%;
}

body {
  margin: 0 5%;
  padding: 0;
  line-height: 1.35;
  text-align: justify;
  widows: 2;
  orphans: 2;
}

/* Заголовки: без рамок, без фона, без обводок. */
h1, h2, h3, h4, h5, h6,
.chapter-title {
  border: 0 none;
  background: transparent;
  box-shadow: none;
  text-decoration: none;
  text-align: left;
  text-indent: 0;
  font-weight: bold;
  line-height: 1.2;
  margin: 1.2em 0 0.8em;
  padding: 0;
  page-break-after: avoid;
  break-after: avoid;
}

h1 { font-size: 1.45em; margin-top: 0.6em; }
h2 { font-size: 1.3em; }
h3 { font-size: 1.15em; }
h4 { font-size: 1.05em; }

p {
  margin: 0;
  padding: 0;
  text-indent: 1.2em;
}

/* Первый абзац после заголовка — без отступа, как в бумажной книге. */
h1 + p, h2 + p, h3 + p, h4 + p,
section > h1 + p, section > h2 + p, section > h3 + p {
  text-indent: 0;
}

p.subtitle {
  text-indent: 0;
  text-align: center;
  font-style: italic;
  margin: 1em 0;
}

p.center, .center {
  text-indent: 0;
  text-align: center;
}

p.text-author, p.date {
  text-indent: 0;
  text-align: right;
  font-style: italic;
  margin: 0.4em 0;
}

p.v {
  text-indent: 0;
  text-align: left;
  margin: 0;
  padding-left: 1.5em;
}

div.empty-line {
  height: 1em;
}

blockquote.epigraph,
blockquote.cite {
  margin: 1em 2em;
  font-style: italic;
}

blockquote.epigraph p,
blockquote.cite p {
  text-indent: 0;
}

div.poem {
  margin: 1em 0;
}

div.stanza {
  margin: 0.8em 0;
}

div.annotation {
  margin: 1em 0;
  font-style: italic;
}

div.illustration, figure {
  margin: 1em 0;
  text-align: center;
  text-indent: 0;
}

div.illustration img, figure img, img.inline {
  max-width: 100%;
  height: auto;
}

img.inline {
  vertical-align: middle;
}

p.missing-image {
  text-indent: 0;
  text-align: center;
  color: #666;
  font-style: italic;
}

table {
  margin: 1em auto;
  border-collapse: collapse;
}

td, th {
  border: 1px solid #999;
  padding: 0.25em 0.5em;
}

/* Сноски: noteref в тексте и aside в конце книги. */
a.noteref {
  text-decoration: none;
  vertical-align: super;
  font-size: 0.75em;
  line-height: 0;
  color: #0645ad;
}

section[epub|type~="footnotes"] {
  margin-top: 1em;
}

aside.footnote {
  margin: 0.9em 0;
  padding-left: 1.2em;
  text-indent: 0;
}

aside.footnote p {
  text-indent: 0;
}

p.note-title {
  font-size: 0.9em;
  color: #444;
}

p.note-title a {
  text-decoration: none;
}

/* Обложка. */
div.cover-page {
  margin: 0;
  padding: 0;
  text-align: center;
}

img.cover {
  max-width: 100%;
  max-height: 100%;
  height: auto;
}

a {
  color: inherit;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn el(name: &str, attrs: &[(&str, &str)], children: Vec<Node>) -> Element {
        Element {
            name: name.into(),
            attrs: attrs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
            children,
        }
    }

    fn text(s: &str) -> Node {
        Node::Text(s.into())
    }

    #[test]
    fn base64_decodes_png_header() {
        // 1x1 PNG в base64
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let bytes = decode_base64(png).expect("декодируется");
        assert_eq!(&bytes[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    }

    #[test]
    fn base64_rejects_garbage() {
        assert!(decode_base64("это не base64!!!").is_none());
    }

    #[test]
    fn xml_parses_attributes_with_prefix() {
        let doc = r##"<FictionBook><body><section id="s1"><a l:href="#n1" type="note">1</a></section></body></FictionBook>"##;
        let root = parse_xml(doc).expect("разбирается");
        let a = root.first("body").unwrap().first("section").unwrap().first("a").unwrap();
        assert_eq!(a.attr("href"), Some("#n1"));
        assert_eq!(a.attr("type"), Some("note"));
    }

    #[test]
    fn xml_joins_text_split_by_entities() {
        let doc = r##"<FictionBook><p>a&amp;b</p></FictionBook>"##;
        let root = parse_xml(doc).expect("разбирается");
        assert_eq!(root.first("p").unwrap().text(), "a&b");
    }

    #[test]
    fn civil_date_is_correct() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn section_id_and_titles_are_sanitized() {
        let s = el("section", &[("id", "n.1:2")], vec![]);
        assert_eq!(sanitize_id(s.attr("id").unwrap()), "n_1_2");
        assert_eq!(sanitize_id("1abc"), "x1abc");
    }

    #[test]
    fn container_section_is_flattened_into_parts() {
        let inner = el(
            "section",
            &[],
            vec![Node::Element(el("title", &[], vec![text("Глава")]))],
        );
        let container = el("section", &[], vec![Node::Element(inner)]);
        let body = el("body", &[], vec![Node::Element(container)]);
        let mut parts = Vec::new();
        let mut cur = Vec::new();
        collect_parts(&body.children, &mut cur, &mut parts);
        if !cur.is_empty() {
            parts.push(cur);
        }
        assert_eq!(parts.len(), 1, "контейнер без заголовка не создаёт пустую часть");
        assert_eq!(parts[0][0].name, "section");
        assert!(parts[0][0].has("title"));
    }

    #[test]
    fn nav_is_nested_and_lists_every_chapter() {
        let nodes = vec![
            NavNode { title: "Глава 1".into(), href: "text/part0001.xhtml#s1".into(), children: vec![] },
            NavNode {
                title: "Глава 2".into(),
                href: "text/part0002.xhtml#s2".into(),
                children: vec![NavNode {
                    title: "Раздел".into(),
                    href: "text/part0002.xhtml#s3".into(),
                    children: vec![],
                }],
            },
        ];
        let html = render_nav(&nodes);
        assert_eq!(html.matches("<li>").count(), 3);
        assert!(html.contains("text/part0002.xhtml#s3"));
        assert!(html.contains("<ol>"));
    }

    #[test]
    fn dangling_internal_link_becomes_plain_text() {
        let dir = std::env::temp_dir().join(format!("fb2epub_anchor_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fb2 = dir.join("a.fb2");
        let out = dir.join("a.epub");
        // `#635428` — так выглядит мусорная ссылка из сетевой литературы:
        // якоря с таким id в FB2 нет вообще.
        let doc = r##"<?xml version="1.0" encoding="utf-8"?>
<FictionBook><description><title-info><book-title>Якоря</book-title><lang>ru</lang></title-info></description>
<body><section><title><p>Глава</p></title>
<p id="real">Настоящий абзац.</p>
<p><a l:href="#real">рабочая ссылка</a> и <a l:href="#635428">битая ссылка</a>.</p>
</section></body>
</FictionBook>"##;
        fs::write(&fb2, doc).unwrap();

        let rep = convert(&fb2, &out).expect("конвертация проходит");
        assert_eq!(rep.missing_anchors, vec!["635428".to_string()]);

        let mut zip = zip::ZipArchive::new(fs::File::open(&out).unwrap()).unwrap();
        let mut part = String::new();
        use std::io::Read;
        zip.by_name("text/part0001.xhtml").unwrap().read_to_string(&mut part).unwrap();
        assert!(part.contains(r##"<p id="real">"##), "id абзаца сохранён");
        assert!(part.contains(r##"<a href="#real">"##), "рабочая ссылка осталась");
        assert!(!part.contains("635428"), "битой ссылки в выводе быть не должно");
        assert!(part.contains("битая ссылка"), "текст битой ссылки сохранён");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_format_comes_from_bytes_not_from_content_type() {
        let dir = std::env::temp_dir().join(format!("fb2epub_fmt_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fb2 = dir.join("f.fb2");
        let out = dir.join("f.epub");
        // Внутри JPEG, а `content-type` врёт про PNG — так делают реальные FB2.
        let jpeg = "/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/wAALCAABAAEBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AKp//2Q==";
        let doc = format!(
            r##"<?xml version="1.0" encoding="utf-8"?>
<FictionBook><description><title-info><book-title>Формат</book-title><lang>ru</lang></title-info></description>
<body><section><title><p>Глава</p></title><p>Текст.</p><image l:href="#pic.png"/></section></body>
<binary id="pic.png" content-type="image/png">{jpeg}</binary>
</FictionBook>"##
        );
        fs::write(&fb2, doc).unwrap();

        convert(&fb2, &out).expect("конвертация проходит");
        let mut zip = zip::ZipArchive::new(fs::File::open(&out).unwrap()).unwrap();
        assert!(
            zip.by_name("images/img0001.jpg").is_ok(),
            "расширение должно быть по байтам (jpg), а не по content-type"
        );
        assert!(zip.by_name("images/img0001.png").is_err());
        let mut opf = String::new();
        use std::io::Read;
        zip.by_name("content.opf").unwrap().read_to_string(&mut opf).unwrap();
        assert!(opf.contains(r#"media-type="image/jpeg""#), "MIME совпадает с содержимым");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_binary_and_missing_note_are_reported() {
        let dir = std::env::temp_dir().join(format!("fb2epub_missing_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fb2 = dir.join("m.fb2");
        let out = dir.join("m.epub");
        // Ссылки на картинку и на примечание, которых в файле нет.
        let doc = r##"<?xml version="1.0" encoding="utf-8"?>
<FictionBook><description><title-info><book-title>Дыры</book-title><lang>ru</lang></title-info></description>
<body><section><title><p>Глава</p></title>
<p>Текст<a l:href="#nope" type="note">1</a>.</p>
<image l:href="#gone.png"/>
</section></body>
</FictionBook>"##;
        fs::write(&fb2, doc).unwrap();

        let rep = convert(&fb2, &out).expect("конвертация не падает на дырах");
        assert_eq!(rep.missing_notes, vec!["nope".to_string()]);
        assert_eq!(rep.missing_images, vec!["gone.png".to_string()]);
        assert_eq!(rep.images, 0);

        // В тексте должна быть видимая заглушка, а не ссылка в пустоту.
        let mut zip = zip::ZipArchive::new(fs::File::open(&out).unwrap()).unwrap();
        let mut part = String::new();
        use std::io::Read;
        zip.by_name("text/part0001.xhtml").unwrap().read_to_string(&mut part).unwrap();
        assert!(part.contains("missing-image"), "заглушка вместо битой картинки");
        assert!(!part.contains("gone.png"), "ссылки на отсутствующий файл быть не должно");
        assert!(!part.contains("notes.xhtml#fn-nope"), "ссылки на несуществующее примечание быть не должно");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn whole_book_converts_with_footnotes_and_cover() {
        let dir = std::env::temp_dir().join(format!("fb2epub_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fb2 = dir.join("t.fb2");
        let out = dir.join("t.epub");
        // 1x1 PNG
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let doc = format!(
            r##"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0" xmlns:l="http://www.w3.org/1999/xlink">
<description><title-info>
<genre>sf</genre><author><first-name>Иван</first-name><last-name>Петров</last-name></author>
<book-title>Тестовая книга</book-title><lang>ru</lang>
<annotation><p>Аннотация.</p></annotation>
<coverpage><image l:href="#cover.png"/></coverpage>
</title-info><document-info><id>12345678-1234-1234-1234-123456789abc</id></document-info></description>
<body><title><p>Тестовая книга</p></title>
<section><title><p>Глава первая</p></title>
<p>Текст со сноской<a l:href="#n1" type="note">1</a> и продолжение.</p>
<image l:href="#pic.png"/>
</section>
<section><title><p>Глава вторая</p></title><p>Ещё текст.</p></section>
</body>
<body name="notes"><section id="n1"><title><p>1</p></title><p>Текст примечания.</p></section></body>
<binary id="cover.png" content-type="image/png">{png}</binary>
<binary id="pic.png" content-type="image/png">{png}</binary>
</FictionBook>"##
        );
        fs::write(&fb2, doc).unwrap();

        let rep = convert(&fb2, &out).expect("конвертация без ошибок");
        assert_eq!(rep.notes, 1);
        assert_eq!(rep.images, 2);
        assert!(rep.missing_notes.is_empty());

        let file = fs::File::open(&out).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
        assert_eq!(names[0], "mimetype", "mimetype обязан быть первым");
        assert!(!names.iter().any(|n| n.ends_with(".ncx")), "toc.ncx быть не должно");
        assert!(names.contains(&"nav.xhtml".to_string()));
        assert!(names.contains(&"images/cover.png".to_string()));
        assert!(names.contains(&"text/notes.xhtml".to_string()));
        assert!(names.contains(&"css/unified.css".to_string()));

        let mut read = |n: &str| {
            let mut s = String::new();
            use std::io::Read;
            zip.by_name(n).unwrap().read_to_string(&mut s).unwrap();
            s
        };
        let opf = read("content.opf");
        assert!(opf.contains(r#"version="3.3""#), "нужен EPUB 3.3");
        assert!(opf.contains("dcterms:modified"));
        assert!(opf.contains(r#"properties="cover-image""#));
        assert!(opf.contains(r#"properties="nav""#));

        let nav = read("nav.xhtml");
        assert!(nav.contains("Глава первая") && nav.contains("Глава вторая"));
        assert!(!nav.contains("Текст примечания"), "сноски не должны попадать в оглавление");

        let part = read("text/part0001.xhtml");
        assert!(part.contains(r#"epub:type="noteref""#), "ссылка-сноска помечена");
        assert!(part.contains("notes.xhtml#fn-n1"));
        assert!(!part.contains(r##"<a href="#n1""##), "сноска не должна оставаться простым якорем");

        let notes = read("text/notes.xhtml");
        assert!(notes.contains(r#"epub:type="footnote""#));
        assert!(notes.contains(r#"id="fn-n1""#));
        assert!(notes.contains(r#"epub:type="backlink""#));

        let _ = fs::remove_dir_all(&dir);
    }
}

/// Диагностика фильтра языка на настоящих книгах.
/// `cargo test --release -- --ignored lang_probe --nocapture`
#[test]
#[ignore]
fn lang_probe() {
    let dir = std::path::Path::new("/tmp/nc2");
    if !dir.exists() {
        eprintln!("нет /tmp/nc2");
        return;
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "fb2"))
        .collect();
    files.sort();
    for f in files {
        let bytes = std::fs::read(&f).unwrap();
        let st = script_stats(&bytes);
        println!(
            "{:<12} букв {:<8} кир {:<8} лат {:<8} доля кир {:>5.1}%  нерус.кир {:>5.2}%  {}",
            f.file_stem().unwrap().to_string_lossy(),
            st.letters(),
            st.cyrillic,
            st.latin,
            st.cyrillic_share() * 100.0,
            st.foreign_share() * 100.0,
            if st.cyrillic_share() >= 0.5 { "РУССКАЯ" } else { "НЕ РУССКАЯ" }
        );
    }
}

#[cfg(test)]
mod script_tests {
    use super::*;

    #[test]
    fn counts_cyrillic_and_latin_separately() {
        let fb2 = "<FictionBook><body><section><p>Привет, мир!</p></section></body></FictionBook>";
        let st = script_stats(fb2.as_bytes());
        assert_eq!(st.cyrillic, 9, "Привет + мир = 9 букв");
        assert_eq!(st.latin, 0);
        assert!(st.cyrillic_share() > 0.99);
    }

    #[test]
    fn ignores_base64_inside_binary() {
        // `<binary>` — картинка в base64, то есть сплошная латиница. Если её
        // посчитать, любая книга с иллюстрациями станет «английской».
        let fb2 = "<FictionBook><body><section><p>Привет мир</p></section></body>\
                   <binary id=\"a.jpg\">AAAABBBBCCCCDDDDEEEEFFFF</binary></FictionBook>";
        let st = script_stats(fb2.as_bytes());
        assert_eq!(st.latin, 0, "base64 не должен попадать в латиницу");
        assert_eq!(st.cyrillic, 9);
    }

    #[test]
    fn ignores_tags_and_entities() {
        // `<p>` и `l:href` — разметка; `&#257;` — сущность. Ни то, ни другое
        // не буквы текста.
        let fb2 = "<body><p l:href=\"#n1\">&#257;&#257;</p><p>Слово</p></body>";
        let st = script_stats(fb2.as_bytes());
        assert_eq!(st.cyrillic, 5);
        assert_eq!(st.latin, 0);
    }

    #[test]
    fn separates_ukrainian_from_russian() {
        let russian = "Это русская книга, и в ней нет ни одной нерусской буквы. ".repeat(6);
        let ukrainian = "Це українська книга, і вона не російська, її треба відрізнити. ".repeat(6);
        let ru = script_stats(russian.as_bytes());
        let uk = script_stats(ukrainian.as_bytes());
        assert!(ru.cyrillic > 200 && uk.cyrillic > 200);
        assert_eq!(ru.foreign_share(), 0.0, "в русском тексте нерусских букв нет");
        assert!(
            uk.foreign_share() > 0.03,
            "украинских букв должно быть заметно: {}",
            uk.foreign_share()
        );
    }

    #[test]
    fn latvian_text_is_not_cyrillic() {
        let lv = "Sidraba vītols ir latviešu grāmata, un tā nav krieviska. ".repeat(6);
        let st = script_stats(lv.as_bytes());
        assert!(st.cyrillic_share() < 0.01);
    }
}
