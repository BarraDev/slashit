// SPDX-License-Identifier: MPL-2.0
//! Safe rendering of untrusted Markdown to HTML.

use crate::services::is_safe_link_destination;

/// Render untrusted GitHub-flavored Markdown, such as a PR-comment body or
/// updater release notes, to HTML. Raw HTML is dropped, images are replaced
/// by their alt text so nothing is fetched on render, and a link keeps its
/// markup only when `is_safe_link_destination` accepts it; otherwise just its
/// text remains. Output is consumed via Leptos `inner_html` and styled via the
/// `prose prose-invert` Tailwind plugin. Clicks on its links are routed to the
/// opener by `route_external_links`, installed at startup.
pub fn render_safe_markdown(src: &str) -> String {
    render(src, false)
}

/// Render untrusted updater release notes like `render_safe_markdown`, but
/// keep every source line on its own line: a single newline inside a
/// paragraph becomes `<br />` instead of collapsing into a space.
pub fn render_release_notes_markdown(src: &str) -> String {
    render(src, true)
}

fn render(src: &str, hard_line_breaks: bool) -> String {
    use pulldown_cmark::{html, Event, Options, Parser, Tag, TagEnd};

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_GFM);

    // One entry per open link: whether its markup was kept.
    let mut open_links: Vec<bool> = Vec::new();
    let parser = Parser::new_ext(src, opts).filter_map(|ev| match ev {
        Event::Html(_) | Event::InlineHtml(_) => None,
        Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,
        Event::Start(Tag::Link { ref dest_url, .. }) => {
            let keep = is_safe_link_destination(dest_url);
            open_links.push(keep);
            keep.then_some(ev)
        }
        Event::End(TagEnd::Link) => open_links.pop().unwrap_or(false).then_some(ev),
        Event::SoftBreak if hard_line_breaks => Some(Event::HardBreak),
        ev => Some(ev),
    });

    let mut out = String::with_capacity(src.len() + src.len() / 4);
    html::push_html(&mut out, parser);
    out
}

#[cfg(test)]
mod tests {
    use super::{render_release_notes_markdown, render_safe_markdown};
    use crate::services::is_safe_link_destination;

    #[test]
    fn only_absolute_web_and_mail_destinations_are_links() {
        for ok in [
            "https://example.com/a?b=c",
            "HTTP://EXAMPLE.COM",
            "mailto:a@example.com",
        ] {
            assert!(is_safe_link_destination(ok), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            " javascript:alert(1)",
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            "\u{1}javascript:alert(1)",
            "data:text/html,x",
            "vbscript:x",
            "file:///etc/passwd",
            "tauri://localhost/",
            "//example.com/x",
            "/relative/path",
            "relative",
            "#fragment",
            "https:",
            "httpx://example.com",
            "https://exa mple.com",
            "",
        ] {
            assert!(!is_safe_link_destination(bad), "{bad:?}");
        }
    }

    #[test]
    fn unsafe_links_keep_their_text_but_not_their_destination() {
        for src in [
            "[click](javascript:alert(1))",
            "[click](JAVASCRIPT:alert(1))",
            "[click](&#106;avascript:alert(1))",
            "[click](java&#9;script:alert(1))",
            "[click](%6Aavascript:alert(1))",
            "[click](data:text/html;base64,AAAA)",
            "[click](//evil.example/x)",
            "[click](/relative)",
            "[click][r]\n\n[r]: javascript:alert(1)",
            "<javascript:alert(1)>",
        ] {
            let html = render_safe_markdown(src);
            assert!(!html.contains("href"), "{src}: {html}");
            assert!(!html.contains("<a"), "{src}: {html}");
        }
        assert_eq!(
            render_safe_markdown("[click](javascript:alert(1))").trim(),
            "<p>click</p>"
        );
    }

    #[test]
    fn safe_links_stay_clickable_and_escaped() {
        let html =
            render_safe_markdown("[a](https://example.com/x?a=1&b=\"2\") <mailto:a@example.com>");
        assert!(
            html.contains("<a href=\"https://example.com/x?a=1&amp;b=%222%22\">a</a>"),
            "{html}"
        );
        assert!(html.contains("<a href=\"mailto:a@example.com\">"), "{html}");
        let nested =
            render_safe_markdown("**[bold](https://example.com)** and [x](javascript:1) *y*");
        assert!(
            nested.contains("<strong><a href=\"https://example.com\">bold</a></strong>"),
            "{nested}"
        );
        assert!(nested.contains("and x <em>y</em>"), "{nested}");
    }

    #[test]
    fn images_are_replaced_by_their_alt_text_and_never_fetched() {
        for src in [
            "![alt text](https://tracker.example/p.png)",
            "![alt text][r]\n\n[r]: https://tracker.example/p.png",
            "![alt text](javascript:alert(1))",
        ] {
            let html = render_safe_markdown(src);
            assert!(
                !html.contains("<img") && !html.contains("src=") && !html.contains("tracker"),
                "{html}"
            );
            assert_eq!(html.trim(), "<p>alt text</p>");
        }
        let linked =
            render_safe_markdown("[![logo](https://t.example/p.png)](https://example.com)");
        assert_eq!(
            linked.trim(),
            "<p><a href=\"https://example.com\">logo</a></p>"
        );
    }

    #[test]
    fn raw_html_is_dropped_and_text_stays_escaped() {
        let html = render_safe_markdown(
            "<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n\ninline <b onclick=x>b</b> & \"q\" `<i>`",
        );
        assert!(
            !html.contains("<script")
                && !html.contains("<img")
                && !html.contains("onerror")
                && !html.contains("<b "),
            "{html}"
        );
        assert!(
            html.contains("&amp;") && html.contains("<code>&lt;i&gt;</code>"),
            "{html}"
        );
    }

    #[test]
    fn legitimate_markdown_is_unchanged() {
        let html = render_safe_markdown("# T\n\n- [x] done\n- item\n\n```rust\nlet a = 1 < 2;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n~~s~~");
        for needle in [
            "<h1>T</h1>",
            "type=\"checkbox\"",
            "<li>item</li>",
            "&lt; 2",
            "<table>",
            "<del>s</del>",
        ] {
            assert!(html.contains(needle), "{needle}: {html}");
        }
    }

    #[test]
    fn malformed_links_do_not_unbalance_the_output() {
        for src in [
            "[a](",
            "[a](<",
            "[a]()",
            "[[a](javascript:1)](https://example.com)",
            "[a](https://e.com",
            "]( [a",
        ] {
            let html = render_safe_markdown(src);
            assert!(!html.to_lowercase().contains("javascript"), "{src}: {html}");
            assert_eq!(
                html.matches("<a ").count(),
                html.matches("</a>").count(),
                "{src}: {html}"
            );
        }
    }
    #[test]
    fn release_notes_keep_their_structure() {
        let html = render_safe_markdown(
            "## What's new\n\n- **Faster** startup\n- See [notes](https://example.com/r)\n\n1. one\n\n_Note_: run `slashit --help`\n\n```sh\nslashit update\n```\n",
        );
        for needle in [
            "<h2>What's new</h2>",
            "<ul>",
            "<li><strong>Faster</strong> startup</li>",
            "<a href=\"https://example.com/r\">notes</a>",
            "<ol>",
            "<em>Note</em>",
            "<code>slashit --help</code>",
            "<pre><code class=\"language-sh\">slashit update\n</code></pre>",
        ] {
            assert!(html.contains(needle), "{needle}: {html}");
        }
    }

    #[test]
    fn malicious_release_notes_render_inert() {
        let html = render_safe_markdown(
            "## Fixes\n\n<iframe src=\"https://evil.example\"></iframe>\n\n- [update now](javascript:fetch('//evil'))\n- ![pixel](https://evil.example/t.gif)\n- <a href=\"https://evil.example\" onclick=\"x()\">raw</a>\n- [file](file:///etc/passwd)\n",
        );
        for banned in [
            "<iframe",
            "<img",
            "evil.example",
            "javascript",
            "onclick",
            "file:",
            "href",
        ] {
            assert!(!html.contains(banned), "{banned}: {html}");
        }
        for kept in [
            "<h2>Fixes</h2>",
            "<li>update now</li>",
            "<li>pixel</li>",
            "<li>file</li>",
        ] {
            assert!(html.contains(kept), "{kept}: {html}");
        }
    }

    #[test]
    fn release_notes_keep_each_line_on_its_own_line() {
        assert_eq!(
            render_release_notes_markdown("First line\nSecond line").trim(),
            "<p>First line<br />\nSecond line</p>"
        );
        // PR comments and agent summaries keep CommonMark soft breaks.
        assert_eq!(
            render_safe_markdown("First line\nSecond line").trim(),
            "<p>First line\nSecond line</p>"
        );
    }

    #[test]
    fn release_notes_line_breaks_keep_the_safety_rules() {
        let html = render_release_notes_markdown(
            "[x](javascript:alert(1))\n![p](https://evil.example/t.gif)\n<b onclick=x>b</b>\n[ok](https://example.com)",
        );
        assert_eq!(
            html.trim(),
            "<p>x<br />\np<br />\nb<br />\n<a href=\"https://example.com\">ok</a></p>"
        );
    }
}
