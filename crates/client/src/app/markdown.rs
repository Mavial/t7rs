use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use egui_extras::syntax_highlighting::{CodeTheme, highlight};
use htmd::element_handler::{HandlerResult, Handlers};
use markup5ever_rcdom::{Node, NodeData};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::time::Duration;
use ureq::http::Uri;

use super::widgets::icon_button;
use crate::icons::Icon;
use crate::{image_preview, theme};

pub(super) struct Body {
    pub text: String,
    pub link: Option<Uri>,
    rich: bool,
    render_blocks: Option<Box<egui_commonmark::RenderHtmlFn>>,
}

impl Body {
    pub fn plain(text: String) -> Self {
        Self { text, link: None, rich: false, render_blocks: None }
    }

    pub fn parse(mut text: String) -> Self {
        let mut rich = false;
        let mut code = None;
        let mut snippets = Vec::new();
        let mut link = None;
        let mut edits = Vec::new();
        let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        for (event, mut range) in Parser::new_ext(&text, options).into_offset_iter() {
            match event {
                Event::Start(Tag::CodeBlock(kind)) => {
                    rich = true;
                    code = Some((kind, String::new()));
                }
                Event::End(TagEnd::CodeBlock) => {
                    let (kind, contents) = code.take().expect("paired code block events");
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_owned(),
                        CodeBlockKind::Indented => {
                            // The parser's range begins after the four columns that introduce
                            // indented code. Remove those too, retaining any quote/list prefix.
                            let mut columns = 0;
                            for c in text[..range.start].chars().rev() {
                                match c {
                                    ' ' => columns += 1,
                                    '\t' => columns = 4,
                                    _ => break,
                                }
                                range.start -= 1;
                                if columns == 4 {
                                    break;
                                }
                            }
                            String::new()
                        }
                    };
                    snippets.push((range, lang, contents));
                }
                Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                    rich = true;
                    if link.is_none() {
                        link = image_preview::link(&dest_url);
                    }
                    if text.as_bytes()[range.start] == b'!' {
                        edits.push(image_to_link_below_preview(range.start));
                    }
                }
                Event::Text(s) if code.is_none() => {
                    rich |= s.as_ref() != &text[range];
                    if link.is_none() {
                        link = s.split_whitespace().find_map(image_preview::link);
                    }
                }
                Event::Text(s) => code.as_mut().expect("inside code block").1.push_str(&s),
                Event::SoftBreak => edits.push((range, "\\\n".to_owned())),
                Event::Start(Tag::Paragraph) | Event::End(_) => {}
                _ => rich = true,
            }
        }
        // The viewer exposes a custom block hook for HTML. Stand-ins retain the surrounding
        // Markdown document, including lists, quotes and link definitions across snippets.
        let render_blocks = (!snippets.is_empty()).then(|| {
            let mut marker = "<!--t7-code".to_owned();
            while text.contains(&marker) {
                marker.push('_');
            }
            for (index, (range, _, _)) in snippets.iter().enumerate() {
                let newline = if text[range.clone()].ends_with('\n') { "\n" } else { "" };
                edits.push((range.clone(), format!("{marker}{index}-->{newline}")));
            }
            edits.sort_by_key(|(range, _)| range.start);
            Box::new(move |ui: &mut egui::Ui, html: &str| {
                if let Some((_, lang, contents)) = html
                    .trim()
                    .strip_prefix(&marker)
                    .and_then(|s| s.strip_suffix("-->"))
                    .and_then(|s| s.parse::<usize>().ok())
                    .and_then(|i| snippets.get(i))
                {
                    code_block(ui, lang, contents);
                } else {
                    ui.label(html);
                }
            }) as Box<egui_commonmark::RenderHtmlFn>
        });
        if rich {
            for (range, replacement) in edits.into_iter().rev() {
                text.replace_range(range, &replacement);
            }
        }
        Self { text, link, rich, render_blocks }
    }

    pub fn show(&self, ui: &mut egui::Ui, cache: &mut CommonMarkCache) {
        if self.rich {
            ui.scope(|ui| {
                ui.style_mut().interaction.selectable_labels = true;
                ui.visuals_mut().extreme_bg_color = theme::SURFACE_HOVER;
                ui.style_mut().url_in_tooltip = true;
                CommonMarkViewer::new().render_html_fn(self.render_blocks.as_deref()).show(ui, cache, &self.text);
            });
        } else {
            ui.add(
                egui::Label::new(egui::RichText::new(&self.text).size(14.0).color(theme::TEXT)).selectable(true).wrap(),
            );
        }
    }
}

fn code_block(ui: &mut egui::Ui, language: &str, code: &str) {
    let width = ui.available_width();
    ui.vertical(|ui| {
        ui.set_width(width);
        ui.add_space(3.0);
        egui::Frame::new()
            .fill(theme::SURFACE)
            .stroke(egui::Stroke::new(1.0, theme::BORDER))
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    let id = ui.next_auto_id().with(("copied-code", code));
                    let now = ui.input(|i| i.time);
                    let until = ui.data(|d| d.get_temp::<f64>(id));
                    let (icon, tip) = if let Some(until) = until.filter(|until| now < *until) {
                        ui.ctx().request_repaint_after(Duration::from_secs_f64(until - now));
                        (Icon::Check, "Copied")
                    } else {
                        (Icon::Copy, "Copy code")
                    };
                    let copy = icon_button(ui, icon, tip);
                    copy.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tip));
                    if copy.clicked() {
                        ui.ctx().copy_text(code.to_owned());
                        ui.data_mut(|d| d.insert_temp(id, now + 1.2));
                        ui.ctx().request_repaint();
                    }
                    let width = ui.available_width();
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        ui.add_space(4.0);
                        let job = highlight(
                            ui.ctx(),
                            ui.style(),
                            &CodeTheme::dark(13.0),
                            code.strip_suffix('\n').unwrap_or(code),
                            language,
                        );
                        ui.add(egui::Label::new(job).selectable(true).wrap());
                    });
                });
            });
        ui.add_space(3.0);
    });
}

pub(super) fn draft_galley(ui: &egui::Ui, text: &dyn egui::TextBuffer, width: f32) -> std::sync::Arc<egui::Galley> {
    use egui::text::{LayoutJob, TextFormat};
    let draft = text.as_str();
    let font = egui::FontId::proportional(14.0);
    if !draft.contains("```") && !draft.contains("~~~") {
        return ui.painter().layout(draft.to_owned(), font, theme::TEXT, width);
    }
    let prose = TextFormat::simple(font, theme::TEXT);
    let fence = TextFormat::simple(egui::FontId::monospace(14.0), theme::TEXT_DIM);
    let mut job = LayoutJob::default();
    let mut pos = 0;
    let mut language = None;
    for (event, range) in Parser::new(draft).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                language = Some(info);
                job.append(&draft[pos..range.start], 0.0, prose.clone());
                pos = range.start;
            }
            Event::Text(_) if language.is_some() => {
                job.append(&draft[pos..range.start], 0.0, fence.clone());
                let lang = language.as_deref().and_then(|s| s.split_whitespace().next()).unwrap_or("");
                let highlighted = highlight(ui.ctx(), ui.style(), &CodeTheme::dark(14.0), &draft[range.clone()], lang);
                for section in highlighted.sections {
                    let bytes = section.byte_range.start.into()..section.byte_range.end.into();
                    job.append(&highlighted.text[bytes], section.leading_space, section.format);
                }
                pos = range.end;
            }
            Event::End(TagEnd::CodeBlock) if language.take().is_some() => {
                job.append(&draft[pos..range.end], 0.0, fence.clone());
                pos = range.end;
            }
            _ => {}
        }
    }
    job.append(&draft[pos..], 0.0, prose);
    job.wrap.max_width = width;
    ui.painter().layout_job(job)
}

pub(super) fn in_fenced_code(draft: &str, cursor: usize) -> bool {
    if !draft.contains("```") && !draft.contains("~~~") {
        return false;
    }
    let offset = draft.char_indices().nth(cursor).map_or(draft.len(), |(i, _)| i);
    let with_next_line = format!("{}\nx", &draft[..offset]);
    let next_line = with_next_line.len() - 1;
    let mut fenced = false;
    for (event, range) in Parser::new(&with_next_line).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) => fenced = true,
            Event::Text(_) if range.contains(&next_line) => return fenced,
            Event::End(TagEnd::CodeBlock) => fenced = false,
            _ => {}
        }
    }
    false
}

pub(super) fn in_code(draft: &str, cursor: usize) -> bool {
    let offset = draft.char_indices().nth(cursor).map_or(draft.len(), |(i, _)| i);
    let mut probe = draft.to_owned();
    probe.insert(offset, 'x');
    let mut block = false;
    Parser::new(&probe).into_offset_iter().any(|(event, range)| match event {
        Event::Start(Tag::CodeBlock(_)) => {
            block = true;
            false
        }
        Event::End(TagEnd::CodeBlock) => {
            block = false;
            false
        }
        Event::Text(_) if block => range.contains(&offset),
        Event::Code(_) => range.contains(&offset),
        _ => false,
    })
}

pub(super) fn rich_paste(plain: &str) -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let html = clipboard.get().html().ok()?;
    let html_is_from_same_copy = clipboard.get_text().ok()?.replace("\r\n", "\n") == plain;
    html_is_from_same_copy.then(|| from_html(&html)).flatten()
}

fn image_to_link_below_preview(bang: usize) -> (std::ops::Range<usize>, String) {
    (bang..bang + 1, String::new())
}

fn from_html(html: &str) -> Option<String> {
    if html.len() > 256 * 1024 {
        return None;
    }
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["head", "style", "script", "img"])
        .add_handler(vec!["pre", "span", "div"], styled_element)
        .build();
    let tree = converter.html_to_tree(html).ok()?;
    fn within_depth(node: &Node, remaining: usize) -> bool {
        remaining > 0 && node.children.borrow().iter().all(|child| within_depth(child, remaining - 1))
    }
    if !within_depth(&tree, 64) {
        return None;
    }
    let markdown = converter.tree_to_markdown(&tree);
    (!markdown.trim().is_empty()).then_some(markdown)
}

fn styled_element(handlers: &dyn Handlers, element: htmd::Element<'_>) -> Option<HandlerResult> {
    let style = element.attrs.iter().find(|a| a.name.local.as_ref() == "style").map_or("", |a| a.value.as_ref());
    let mut preformatted = element.tag == "pre";
    let mut bold = false;
    let mut italic = false;
    for (property, value) in style.split(';').filter_map(|pair| pair.split_once(':')) {
        match property.trim() {
            "white-space" => preformatted |= matches!(value.trim(), "pre" | "break-spaces"),
            "font-weight" => bold = value.trim() == "bold" || value.trim().parse::<u16>().is_ok_and(|w| w >= 600),
            "font-style" => italic = matches!(value.trim(), "italic" | "oblique"),
            _ => {}
        }
    }
    if preformatted {
        if element.tag == "pre" {
            let children = element.node.children.borrow();
            if let [child] = children.as_slice()
                && matches!(&child.data, NodeData::Element { name, .. } if name.local.as_ref() == "code")
            {
                return handlers.fallback(element);
            }
        }
        let mut code = String::new();
        code_text(element.node, &mut code);
        let ticks = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(3.max(ticks + 1));
        let end = if code.ends_with('\n') { "" } else { "\n" };
        return Some(format!("\n\n{fence}\n{code}{end}{fence}\n\n").into());
    }
    let mut result = handlers.fallback(element)?;
    if bold || italic {
        let marker = "*".repeat(usize::from(bold) * 2 + usize::from(italic));
        let content = &result.content;
        let inner = content.trim();
        if !inner.is_empty() {
            let start = content.len() - content.trim_start().len();
            result.content = format!("{}{marker}{inner}{marker}{}", &content[..start], &content[start + inner.len()..]);
        }
    }
    Some(result)
}

fn code_text(node: &Node, out: &mut String) {
    match &node.data {
        NodeData::Text { contents } => out.push_str(&contents.borrow()),
        NodeData::Element { name, .. } if name.local.as_ref() == "br" => out.push('\n'),
        _ => {
            // Editors such as VS Code put each line in a separate div.
            for (i, child) in node.children.borrow().iter().enumerate() {
                if i > 0 && matches!(&child.data, NodeData::Element { name, .. } if name.local.as_ref() == "div") {
                    out.push('\n');
                }
                code_text(child, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_blocks_stay_in_the_markdown_document() {
        for (source, count) in [
            ("    alpha\n    beta\n", 1),
            ("> ```python\n> print(1)\n> ```", 1),
            ("- item\n\n      nested\n      code\n", 1),
            ("```\na\n```\n```\nb\n```", 2),
        ] {
            let body = Body::parse(source.into());
            let blocks = Parser::new(&body.text).filter(|event| matches!(event, Event::Html(_))).count();
            assert_eq!(blocks, count, "{}", body.text);
        }
    }

    #[test]
    fn styled_clipboard_text_and_editor_code_keep_their_structure() {
        let html = r#"<p><b>Review</b> <span style="font-style: italic">this</span></p><pre>fn f() {
    println!("**literal**");
}</pre>"#;
        let markdown = from_html(html).unwrap();
        assert!(markdown.contains("**Review**") && markdown.contains("*this*"), "{markdown}");
        assert!(markdown.contains("```\nfn f() {\n    println!(\"**literal**\");\n}\n```"), "{markdown}");
        let editor =
            r#"<div style="white-space: pre"><div><span>line one</span></div><div>    line two &lt; 3</div></div>"#;
        let markdown = from_html(editor).unwrap();
        assert_eq!(markdown, "```\nline one\n    line two < 3\n```");
    }

    #[test]
    fn preserves_chat_newlines_and_only_previews_urls_outside_code() {
        let plain = "line one\n  line two\n\nlast";
        let body = Body::parse(plain.into());
        assert_eq!(body.text, plain);
        let body = Body::parse("**line one**\nline two\n\n```\nhttps://example.org/in-code\n```".into());
        assert!(body.text.starts_with("**line one**\\\nline two"));
        assert!(body.link.is_none());
        let body = Body::parse("`https://example.org/in-code` [photo](https://example.org/p.png)".into());
        assert_eq!(body.link.unwrap().path(), "/p.png");
        let draft = "🐈\n```rust\n";
        assert!(in_code(draft, draft.chars().count()));
        let draft = format!("{draft}    body\n```\n");
        assert!(!in_code(&draft, draft.chars().count()));
    }
}
