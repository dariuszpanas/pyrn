"""Check built documentation and README links against the offline site tree.

Pure Python standard library only (no third-party dependencies).
"""

from __future__ import annotations

import re
import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urljoin, urlsplit

SITE = "https://dariuszpanas.github.io/pyrn/"
SITE_PREFIX = "/pyrn/"
LOGO_REL = Path("assets/branding/pyrn_logo.png")
WORDMARK_REL = Path("assets/branding/pyrn_wordmark.svg")
LOGO_URL = f"{SITE}assets/branding/pyrn_logo.png"

_FENCE_RE = re.compile(r"^\s*(`{3,}|~{3,})")
_INLINE_CODE_RE = re.compile(r"`[^`]*`")
_MD_LINK_RE = re.compile(r"(!?)\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
_MD_REF_DEF_RE = re.compile(r"^\s*\[[^\]]+\]:\s*<?(\S+?)>?(?:\s|$)")
_MD_HEADING_RE = re.compile(r"^(#{1,6})\s+(.+?)\s*#*\s*$")


class HtmlLinkParser(HTMLParser):
    """Collect element IDs, anchor hrefs, and image sources from HTML."""

    def __init__(self) -> None:
        super().__init__()
        self.links: list[str] = []
        self.images: list[str] = []
        self.anchors: set[str] = set()

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attributes = dict(attrs)
        if anchor := attributes.get("id"):
            self.anchors.add(anchor)
        if tag == "a":
            href = (attributes.get("href") or "").strip()
            if href:
                self.links.append(href)
        elif tag == "img":
            src = (attributes.get("src") or "").strip()
            if src:
                self.images.append(src)
                self.links.append(src)


def _slugify_github(heading: str) -> str:
    """Approximate GitHub Markdown heading anchor slug."""
    cleaned = re.sub(r"`([^`]*)`", r"\1", heading)
    cleaned = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", cleaned)
    cleaned = cleaned.strip().lower()
    cleaned = re.sub(r"[^\w\s-]", "", cleaned)
    cleaned = re.sub(r"\s+", "-", cleaned)
    return cleaned


def extract_readme_links(markdown_text: str) -> tuple[list[str], list[str], set[str]]:
    """Extract links, images, and heading anchors from Markdown outside code blocks."""
    links: list[str] = []
    images: list[str] = []
    anchors: set[str] = set()
    slug_counts: dict[str, int] = {}

    in_fence = False
    fence_char = ""
    fence_len = 0
    non_code_lines: list[str] = []

    for line in markdown_text.splitlines():
        fence_match = _FENCE_RE.match(line)
        if fence_match:
            marker = fence_match.group(1)
            if not in_fence:
                in_fence = True
                fence_char = marker[0]
                fence_len = len(marker)
                continue
            if marker[0] == fence_char and len(marker) >= fence_len:
                in_fence = False
                continue
        if in_fence:
            continue

        if heading_match := _MD_HEADING_RE.match(line):
            base_slug = _slugify_github(heading_match.group(2))
            count = slug_counts.get(base_slug, 0)
            slug_counts[base_slug] = count + 1
            anchors.add(base_slug if count == 0 else f"{base_slug}-{count}")

        stripped = _INLINE_CODE_RE.sub("", line)
        non_code_lines.append(stripped)

        if ref_match := _MD_REF_DEF_RE.match(stripped):
            links.append(ref_match.group(1))

        for match in _MD_LINK_RE.finditer(stripped):
            is_image = match.group(1) == "!"
            target = match.group(2).strip("<>")
            if is_image:
                images.append(target)
            links.append(target)

    # Also inspect raw HTML <a> / <img> tags in README outside code blocks.
    html_parser = HtmlLinkParser()
    html_parser.feed("\n".join(non_code_lines))
    anchors.update(html_parser.anchors)
    links.extend(html_parser.links)
    images.extend(html_parser.images)

    return links, images, anchors


def _resolve_site_target(
    site: Path,
    source_label: str,
    source_url: str,
    link: str,
    pages: dict[Path, HtmlLinkParser],
) -> bool:
    """Validate one link if it resolves inside SITE; return True if checked."""
    if link.startswith(("mailto:", "javascript:", "data:", "tel:")):
        return False

    resolved = urljoin(source_url, link)
    if not resolved.startswith(SITE):
        return False

    url = urlsplit(resolved)
    local = unquote(url.path.removeprefix(SITE_PREFIX))
    target = (site / local).resolve()

    if not target.is_relative_to(site):
        raise ValueError(f"{source_label}: link escapes site root: {link}")
    if target.is_dir():
        target /= "index.html"
    if not target.is_file():
        raise ValueError(f"{source_label}: missing link target '{link}' -> {target}")

    if url.fragment and target in pages:
        fragment = unquote(url.fragment)
        if fragment not in pages[target].anchors:
            raise ValueError(f"{source_label}: missing fragment '#{fragment}' in {link}")
    return True


def check_site(site: Path, readme: Path) -> int:
    """Verify built HTML links/anchors, README links/logo, and branding asset presence."""
    site = site.resolve()
    pages: dict[Path, HtmlLinkParser] = {}

    for path in sorted(site.rglob("*.html")):
        parsed = HtmlLinkParser()
        parsed.feed(path.read_text(encoding="utf-8"))
        pages[path.resolve()] = parsed

    if not pages:
        raise ValueError("build the documentation before checking links (no HTML files in site/)")

    for asset_rel in (LOGO_REL, WORDMARK_REL):
        asset_path = (site / asset_rel).resolve()
        if not asset_path.is_file() or asset_path.stat().st_size == 0:
            raise ValueError(f"missing or empty branding asset in built site: {asset_path}")

    checked = 0

    # 1. Validate every internal link and fragment anchor across all site/**/*.html pages.
    for path, page in pages.items():
        relative = path.relative_to(site).as_posix()
        source_url = urljoin(SITE, relative)
        for link in page.links:
            if _resolve_site_target(site, relative, source_url, link, pages):
                checked += 1

    # 2. Validate README.md links, anchors, and public logo URL.
    readme_text = readme.read_text(encoding="utf-8")
    readme_links, readme_images, readme_anchors = extract_readme_links(readme_text)

    if LOGO_URL not in readme_images:
        raise ValueError(f"README.md must reference the public logo image: {LOGO_URL}")

    for link in readme_links:
        if link.startswith("#"):
            frag = unquote(link[1:])
            if frag not in readme_anchors:
                raise ValueError(f"README.md: missing internal heading anchor: {link}")
            checked += 1
            continue
        parsed_url = urlsplit(link)
        if parsed_url.scheme != "https" or not parsed_url.netloc:
            raise ValueError(f"README.md requires absolute HTTPS links: {link}")
        if _resolve_site_target(site, "README.md", SITE, link, pages):
            checked += 1

    return checked


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    try:
        count = check_site(root / "site", root / "README.md")
    except ValueError as err:
        print(f"docs check failed: {err}", file=sys.stderr)
        return 1
    print(
        f"Checked {count} documentation and README links "
        f"(logo verified at site/{LOGO_REL.as_posix()})."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
