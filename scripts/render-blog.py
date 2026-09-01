#!/usr/bin/env python3
"""Render blog posts from Markdown to HTML.

Usage:
    scripts/.venv/bin/python scripts/render-blog.py

Each post is a Markdown file in docs/blog/ with front matter:

    ---
    title: "Post title"
    description: "One-sentence lede, shown under the title."
    date: 2026-09-01
    tag: Release
    ---

    Body in Markdown...

The rendered HTML is written next to the .md file and committed, so
GitHub Pages keeps serving static files. The .md source is not linked
anywhere; the .html output is what the blog index points at.
"""

import re
import sys
from pathlib import Path

import markdown

BLOG_DIR = Path(__file__).resolve().parent.parent / "docs" / "blog"

TEMPLATE = """<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <meta name="description" content="{description}">
    <meta name="theme-color" content="#0E1420">
    <link rel="canonical" href="https://cloudbridge.jetsquirrel.cloud/blog/{slug}.html">
    <link rel="icon" href="data:image/svg+xml,<svg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 100 100%22><text y=%22.9em%22 font-size=%2290%22>🌉</text></svg>">
    <title>{title} - CloudBridge Blog</title>

    <!-- Fonts -->
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@500;600;700&family=IBM+Plex+Sans:wght@400;500;600&family=IBM+Plex+Mono:wght@400;500&display=swap" rel="stylesheet">

    <style>
        * {{
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }}

        html {{
            scroll-behavior: smooth;
        }}

        body {{
            font-family: 'IBM Plex Sans', sans-serif;
            color: #47536B;
            background: #FAFBFC;
            line-height: 1.6;
            overflow-x: hidden;
        }}

        :root {{
            --ink: #0E1420;
            --panel: #151E30;
            --paper: #FAFBFC;
            --hairline: #E3E8EF;
            --hairline-dark: #263349;
            --fg-strong: #101828;
            --fg-body: #47536B;
            --fg-faint: #7A8699;
            --signal: #2F6FED;
            --local: #2FA97C;
            --radius: 6px;
            --nav-height: 4.5rem;
            --font-display: 'Space Grotesk', sans-serif;
            --font-mono: 'IBM Plex Mono', monospace;
        }}

        h1, h2 {{
            font-family: var(--font-display);
            color: var(--fg-strong);
            letter-spacing: -0.02em;
            line-height: 1.15;
        }}

        .container {{
            max-width: 1200px;
            margin: 0 auto;
            padding: 0 2rem;
        }}

        .skip-link {{
            position: absolute;
            left: -9999px;
            top: 0;
            background: var(--ink);
            color: #fff;
            padding: 0.75rem 1.25rem;
            z-index: 200;
            text-decoration: none;
            font-family: var(--font-mono);
            font-size: 0.875rem;
        }}

        .skip-link:focus {{
            left: 0;
        }}

        /* Navigation (shared) */
        nav {{
            background: var(--paper);
            border-bottom: 1px solid var(--hairline);
            height: var(--nav-height);
            display: flex;
            align-items: center;
            padding: 0 1rem;
            position: sticky;
            top: 0;
            z-index: 100;
        }}

        nav .container {{
            display: flex;
            justify-content: space-between;
            align-items: center;
            gap: 2rem;
            width: 100%;
        }}

        .logo {{
            display: inline-flex;
            align-items: center;
            gap: 0.6rem;
            font-family: var(--font-display);
            font-size: 1.25rem;
            font-weight: 700;
            color: var(--fg-strong);
            text-decoration: none;
            letter-spacing: -0.02em;
            white-space: nowrap;
        }}

        .logo::before {{
            content: '';
            width: 0.6rem;
            height: 0.6rem;
            background: var(--local);
            flex-shrink: 0;
        }}

        .nav-links {{
            display: flex;
            gap: 2rem;
            list-style: none;
            overflow-x: auto;
            white-space: nowrap;
            min-width: 0;
            scrollbar-width: none;
            -webkit-overflow-scrolling: touch;
        }}

        .nav-links::-webkit-scrollbar {{
            display: none;
        }}

        .nav-links a {{
            font-family: var(--font-mono);
            color: var(--fg-body);
            text-decoration: none;
            font-size: 0.875rem;
            transition: color 0.15s;
        }}

        .nav-links a:hover,
        .nav-links a[aria-current="page"] {{
            color: var(--signal);
        }}

        /* Article */
        .post {{
            max-width: 44rem;
            margin: 0 auto;
            padding: 4rem 0 6rem;
        }}

        .post .eyebrow {{
            font-family: var(--font-mono);
            font-size: 0.75rem;
            font-weight: 500;
            letter-spacing: 0.14em;
            text-transform: uppercase;
            color: var(--local);
            display: block;
            margin-bottom: 1.25rem;
        }}

        .post h1 {{
            font-size: 2.5rem;
            font-weight: 700;
            margin-bottom: 1.5rem;
        }}

        .post .lede {{
            font-size: 1.25rem;
            color: var(--fg-strong);
            margin-bottom: 2.5rem;
            padding-bottom: 2.5rem;
            border-bottom: 1px solid var(--hairline);
        }}

        .post h2 {{
            font-size: 1.5rem;
            font-weight: 700;
            margin: 3rem 0 1rem;
        }}

        .post p {{
            margin-bottom: 1.25rem;
            line-height: 1.8;
        }}

        .post a {{
            color: var(--signal);
        }}

        .post code {{
            font-family: var(--font-mono);
            font-size: 0.8125rem;
            background: #fff;
            border: 1px solid var(--hairline);
            padding: 0.05rem 0.4rem;
            border-radius: 4px;
        }}

        .post pre {{
            background: var(--ink);
            border-radius: var(--radius);
            padding: 1.25rem 1.5rem;
            overflow-x: auto;
            margin-bottom: 1.25rem;
        }}

        .post pre code {{
            background: none;
            border: none;
            padding: 0;
            color: #C4CEDE;
            font-size: 0.8125rem;
            line-height: 1.7;
        }}

        .post ul, .post ol {{
            margin: 0 0 1.25rem 1.25rem;
        }}

        .post li {{
            margin-bottom: 0.4rem;
            line-height: 1.7;
        }}

        .post-nav {{
            margin-top: 4rem;
            padding-top: 2rem;
            border-top: 1px solid var(--hairline);
            font-family: var(--font-mono);
            font-size: 0.875rem;
        }}

        .post-nav a {{
            color: var(--signal);
            text-decoration: none;
        }}

        .post-nav a:hover {{
            text-decoration: underline;
        }}

        /* Footer (shared, compact) */
        footer {{
            background: var(--ink);
            color: #8FA0BC;
            padding: 2rem 0;
            border-top: 1px solid var(--hairline-dark);
        }}

        footer .container {{
            display: flex;
            justify-content: space-between;
            gap: 1rem;
            flex-wrap: wrap;
            font-family: var(--font-mono);
            font-size: 0.75rem;
            color: var(--fg-faint);
        }}

        footer a {{
            color: #8FA0BC;
            text-decoration: none;
        }}

        footer a:hover {{
            color: #fff;
        }}

        @media (max-width: 768px) {{
            .post {{
                padding: 2.5rem 0 4rem;
            }}

            .post h1 {{
                font-size: 1.875rem;
            }}

            .nav-links {{
                gap: 1.25rem;
            }}
        }}

        a:focus-visible {{
            outline: 2px solid var(--signal);
            outline-offset: 2px;
        }}

        @media (prefers-reduced-motion: reduce) {{
            html {{
                scroll-behavior: auto;
            }}
        }}
    </style>
</head>
<body>
    <a class="skip-link" href="#main" data-i18n="a11y.skip">Skip to content</a>

    <nav aria-label="Main navigation">
        <div class="container">
            <a href="/" class="logo">CloudBridge</a>
            <ul class="nav-links">
                <li><a href="/docs.html" data-i18n="nav.docs">Docs</a></li>
                <li><a href="/blog.html" aria-current="page" data-i18n="nav.blog">Blog</a></li>
                <li><a href="https://github.com/JetSquirrel/cloudbridge" target="_blank" rel="noopener noreferrer">GitHub</a></li>
            </ul>
        </div>
    </nav>

    <!-- GENERATED FROM {slug}.md - edit the Markdown, then re-run scripts/render-blog.py -->
    <main id="main">
        <article class="post">
            <span class="eyebrow">{tag} · {date}</span>
            <h1>{title}</h1>
            <p class="lede">{description}</p>

            {content}

            <div class="post-nav">
                <a href="/blog.html" data-i18n="post.back">← All posts</a>
            </div>
        </article>
    </main>

    <footer>
        <div class="container">
            <span data-i18n="footer.copy">&copy; 2024-2026 CloudBridge. Released under MIT License.</span>
            <span>AWS · Alibaba Cloud · DeepSeek</span>
        </div>
    </footer>

    <!-- Same i18n scaffolding as index.html; see that file for how to add a locale. -->
    <script>
        (function () {{
            var LOCALES = {{
                en: {{}} // default: strings live in the markup
            }};

            function applyLocale(locale) {{
                var dict = LOCALES[locale];
                if (!dict) return;
                document.querySelectorAll('[data-i18n]').forEach(function (el) {{
                    var key = el.getAttribute('data-i18n');
                    if (typeof dict[key] === 'string') el.textContent = dict[key];
                }});
                document.querySelectorAll('[data-i18n-html]').forEach(function (el) {{
                    var key = el.getAttribute('data-i18n-html');
                    if (typeof dict[key] === 'string') el.innerHTML = dict[key];
                }});
                document.documentElement.lang = locale;
            }}

            var saved = null;
            try {{ saved = localStorage.getItem('cloudbridge-locale'); }} catch (e) {{}}
            var browser = (navigator.language || 'en').slice(0, 2);
            applyLocale(saved && LOCALES[saved] ? saved : (LOCALES[browser] ? browser : 'en'));
        }})();
    </script>
</body>
</html>
"""


def parse_front_matter(text: str) -> tuple[dict, str]:
    """Split 'key: value' front matter from the Markdown body."""
    match = re.match(r"^---\n(.*?)\n---\n+(.*)$", text, re.S)
    if not match:
        raise ValueError("missing front matter (--- ... ---)")
    meta = {}
    for line in match.group(1).splitlines():
        key, _, value = line.partition(":")
        meta[key.strip()] = value.strip().strip('"')
    return meta, match.group(2)


def render(md_path: Path) -> Path:
    meta, body = parse_front_matter(md_path.read_text(encoding="utf-8"))
    slug = md_path.stem
    for key in ("title", "description", "date", "tag"):
        if key not in meta:
            raise ValueError(f"{md_path.name}: front matter missing '{key}'")
    content = markdown.markdown(body, extensions=["extra"])
    html = TEMPLATE.format(
        slug=slug,
        title=meta["title"],
        description=meta["description"],
        date=meta["date"],
        tag=meta["tag"],
        content=content,
    )
    out_path = md_path.with_suffix(".html")
    out_path.write_text(html, encoding="utf-8")
    return out_path


def main() -> int:
    posts = sorted(BLOG_DIR.glob("*.md"))
    if not posts:
        print("no posts found in", BLOG_DIR)
        return 1
    for md_path in posts:
        out = render(md_path)
        print(f"{md_path.name} -> {out.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
