//! What a reader sees on a page, from a comrak parse.

use comrak::arena_tree::NodeEdge;
use comrak::nodes::NodeValue;
use comrak::{Arena, Options};

/// Markup characters, lines, entities and autolinks (counted twice). Dense emphasis costs
/// comrak about 540 bytes per marker and an autolink about 640 bytes, so one tree stays near
/// 9 MB.
const MARKUP_BUDGET: usize = 16_384;

/// The visible parts of a page.
#[derive(Debug, PartialEq, Eq)]
pub struct Visible {
    /// The text of the first level-1 heading.
    pub title: Option<String>,
    /// The text of each block on its own line, without the title heading.
    pub text: String,
}

fn options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options
}

/// Whether comrak may parse the page. Its tree for a page with very much markup would need
/// more memory than the server has, so such a page is read as raw text.
fn formatted(markdown: &str) -> bool {
    let markers = markdown
        .bytes()
        .filter(|b| b"*_`[]<>!\\|~#&@\n\r".contains(b))
        .count();
    let autolinks = markdown.matches("www.").count() + markdown.matches("://").count();
    markers + 2 * autolinks <= MARKUP_BUDGET
}

/// The title and the text that a reader sees on the page. A page over the markup budget has
/// no title here, and its text is the raw Markdown.
#[must_use]
pub fn visible(markdown: &str) -> Visible {
    if !formatted(markdown) {
        return Visible {
            title: None,
            text: markdown.to_owned(),
        };
    }
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, markdown, &options());
    let title = root
        .descendants()
        .find(|node| matches!(node.data().value, NodeValue::Heading(ref h) if h.level == 1));
    let mut text = String::with_capacity(markdown.len());
    let mut traverse = root.traverse();
    while let Some(edge) = traverse.next() {
        match edge {
            NodeEdge::Start(node) if title.is_some_and(|title| std::ptr::eq(node, title)) => {
                for edge in traverse.by_ref() {
                    if matches!(edge, NodeEdge::End(end) if std::ptr::eq(end, node)) {
                        break;
                    }
                }
            }
            NodeEdge::Start(node) => match &node.data().value {
                NodeValue::Text(literal) => text.push_str(literal),
                NodeValue::Code(code) => text.push_str(&code.literal),
                NodeValue::CodeBlock(block) => text.push_str(&block.literal),
                NodeValue::HtmlInline(html) => text.push_str(html),
                NodeValue::HtmlBlock(html) => text.push_str(&html.literal),
                NodeValue::SoftBreak | NodeValue::LineBreak => text.push(' '),
                _ => {}
            },
            NodeEdge::End(node) if node.data().value.block() && !text.ends_with('\n') => {
                text.push('\n');
            }
            NodeEdge::End(_) => {}
        }
    }
    Visible {
        title: title.map(comrak::arena_tree::Node::collect_text),
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_level_one_heading_is_the_title_and_not_the_text() {
        let page = visible("Intro line.\n\n# The *real* title\n\n# Second\n\nBody.\n");
        assert_eq!(page.title.as_deref(), Some("The real title"));
        assert_eq!(page.text, "Intro line.\nSecond\nBody.\n");
    }

    #[test]
    fn a_page_without_a_level_one_heading_has_no_title() {
        assert_eq!(visible("## Only a section\n\nText.\n").title, None);
    }

    #[test]
    fn link_text_is_visible_and_the_target_is_not() {
        let page = visible("See [the harbour](ships/hidden-target.md).\n");
        assert_eq!(page.text, "See the harbour.\n");
    }

    #[test]
    fn inline_code_and_code_blocks_are_visible() {
        let page = visible("Run `cargo build`.\n\n```sh\nmise run gate\n```\n");
        assert_eq!(page.text, "Run cargo build.\nmise run gate\n");
    }

    #[test]
    fn a_page_over_the_markup_budget_is_raw_text() {
        let markdown = format!("# Title\n\n{}", "*a* ".repeat(MARKUP_BUDGET));
        let page = visible(&markdown);
        assert_eq!(page.title, None);
        assert_eq!(page.text, markdown);
    }
}
