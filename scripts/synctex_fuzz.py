#!/usr/bin/env python3
"""Fuzz forward and inverse SyncTeX mappings for any TeX-built PDF.

Forward probes compare pdfterm's page with raw SyncTeX results and use Poppler
text to check selected source words. When raw SyncTeX has no page but the
selected word appears in the PDF (for example, a title argument), it must
appear on pdfterm's chosen page. Raw page disagreement is reported, not
failed: a resolver may refine an imprecise SyncTeX source position.
Inverse probes click centers of on-page painted PDF glyph boxes found by PyMuPDF.
The raw `synctex edit` result must name a real source line; its `synctex view`
page is diagnostic because overlays can make the raw mapping asymmetric.
This script does not launch the viewer or Neovim or exercise pdfterm's inverse
word refinement. Output is one JSON summary. Any failed probe exits nonzero.

Usage: synctex_fuzz.py PDF [--source TEX] [--lines N]
       [--points N] [--pages N] [--max-fails N] [--allow-stale]
"""

import argparse
import gzip
import json
import random
import re
import subprocess
import sys
import unicodedata
from collections.abc import Iterator
from pathlib import Path

GlyphPoint = tuple[str, tuple[float, float, float, float]]


def raw_pages(pdf: Path, source: Path, line: int, column: int = 1) -> set[int]:
    result = subprocess.run(
        ["synctex", "view", "-i", f"{line}:{column}:{source}", "-o", str(pdf)],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"synctex view: {result.stderr.strip()[:200]}")
    return {int(p) for p in re.findall(r"^Page:(\d+)$", result.stdout, re.MULTILINE)}


def raw_inverse(pdf: Path, x: float, y: float, page: int) -> tuple[int, str] | None:
    result = subprocess.run(
        ["synctex", "edit", "-o", f"{page}:{x:.2f}:{y:.2f}:{pdf}"],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"synctex edit: {result.stderr.strip()[:200]}")
    matches = re.findall(r"^Input:(.*)$\n^Line:(\d+)", result.stdout, re.MULTILINE)
    return (int(matches[-1][1]), matches[-1][0]) if matches else None


def source_line_text(text: str) -> str:
    escaped = False
    for index, char in enumerate(text):
        if escaped:
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == "%":
            return text[:index]
    return text


def sample_source_positions(
    source: str, rng: random.Random, count: int
) -> list[tuple[int, int]]:
    candidates = []
    for line, row in enumerate(source.splitlines(), 1):
        if not row.strip() or row.lstrip().startswith("%"):
            continue
        row = source_line_text(row)
        words = [
            match.start() + 1
            for match in re.finditer(r"[A-Za-z]{6,}", row)
            if match.start() == 0 or row[match.start() - 1] != "\\"
        ]
        candidates.append((line, rng.choice(words) if words else 1))
    return sorted(rng.sample(candidates, min(count, len(candidates))))


def normalized_words(value: str) -> list[str]:
    folded = "".join(
        char
        for char in unicodedata.normalize("NFKD", value).casefold()
        if not unicodedata.combining(char)
    )
    return re.findall(r"\w+", folded)


def context_score(tokens: list[str], hint: dict) -> int | None:
    source = [normalized_words(word) for word in hint["words"]]
    if any(len(word) != 1 for word in source):
        return None
    source = [word[0] for word in source]
    selected = hint["selected"]
    best = None
    for index, word in enumerate(tokens):
        if word != source[selected]:
            continue
        score = 0
        for distance in range(1, 4):
            for direction in (-1, 1):
                neighbor = index + direction * distance
                source_neighbor = selected + direction * distance
                if (
                    0 <= neighbor < len(tokens)
                    and 0 <= source_neighbor < len(source)
                    and tokens[neighbor] == source[source_neighbor]
                ):
                    score += 4 - distance
        best = max(score, best or 0)
    return best


def pdf_visible_words(pdf: Path) -> list[list[str]]:
    result = subprocess.run(
        ["pdftotext", "-layout", str(pdf), "-"],
        capture_output=True,
        text=True,
        errors="replace",
        timeout=120,
        check=False,
    )
    if result.returncode:
        raise SystemExit(f"pdftotext: {result.stderr.strip()[:200]}")
    pages = result.stdout.split("\f")
    if pages[-1].strip() == "":
        pages.pop()
    return [normalized_words(page) for page in pages]


def forward(
    pdf: Path,
    source: Path,
    positions: list[tuple[int, int]],
    page_count: int,
    visible_words: list[list[str]] | None = None,
) -> Iterator[dict]:
    for line, column in positions:
        try:
            expected = raw_pages(pdf, source, line, column)
        except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
            yield {
                "direction": "forward",
                "line": line,
                "ok": False,
                "error": str(error),
            }
            continue
        try:
            result = subprocess.run(
                [
                    str(pdfterm_bin()),
                    str(pdf),
                    "--synctex-view",
                    str(source),
                    "--line",
                    str(line),
                    "--column",
                    str(column),
                ],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            yield {
                "direction": "forward",
                "line": line,
                "column": column,
                "ok": False,
                "error": str(error),
            }
            continue
        if result.returncode != 0:
            yield {
                "direction": "forward",
                "line": line,
                "ok": not expected and "no complete match" in result.stderr,
                "error": result.stderr.strip()[:200],
                "raw_pages": sorted(expected),
            }
            continue
        try:
            got = json.loads(result.stdout)
            page = got["page"]
            valid = got["pdf"] == str(pdf) and type(page) is int and 1 <= page <= page_count
        except (ValueError, KeyError, TypeError) as error:
            yield {
                "direction": "forward",
                "line": line,
                "ok": False,
                "error": str(error),
            }
            continue
        entry = {
            "direction": "forward",
            "line": line,
            "column": column,
            "ok": valid,
            "resolved_page": page,
            "raw_pages": sorted(expected),
            "raw_page_match": page in expected,
            "raw_unmapped": not expected,
        }
        if (
            valid
            and visible_words is not None
            and (not expected or len(expected) > 1)
            and got.get("word")
        ):
            try:
                hint = got["word"]
                chosen_score = context_score(visible_words[page - 1], hint)
                if not expected:
                    elsewhere = chosen_score is None and any(
                        context_score(words, hint) is not None
                        for words in visible_words
                    )
                    entry["visible_checked"] = chosen_score is not None or elsewhere
                    if elsewhere:
                        entry["ok"] = False
                        entry["error"] = "raw SyncTeX has no page and selected word is absent from chosen PDF page"
                elif page in expected:
                    entry["visible_checked"] = True
                    better = [
                        candidate
                        for candidate in expected
                        if candidate != page
                        and 1 <= candidate <= page_count
                        and (context_score(visible_words[candidate - 1], hint) or 0) > 0
                    ]
                    if chosen_score is None and better:
                        entry["ok"] = False
                        entry["error"] = (
                            "selected word absent from chosen page but present with source context on another SyncTeX page"
                        )
                        entry["visible_alternatives"] = sorted(better)
            except (IndexError, KeyError, TypeError, ValueError) as error:
                entry["ok"] = False
                entry["error"] = f"invalid forward word or PDF page: {error}"
        yield entry


def input_path(name: str, pdf: Path, source: Path) -> Path:
    path = Path(name)
    if path.is_absolute():
        return path.resolve()
    for root in (pdf.parent, source.parent):
        candidate = (root / path).resolve()
        if candidate.is_file():
            return candidate
    return (pdf.parent / path).resolve()


def inverse(
    pdf: Path,
    source: Path,
    glyph_points: dict[int, list[GlyphPoint]],
) -> Iterator[dict]:
    for page, glyphs in glyph_points.items():
        for symbol, box in glyphs:
            x = (box[0] + box[2]) / 2
            y = (box[1] + box[3]) / 2
            try:
                raw = raw_inverse(pdf, x, y, page)
            except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
                yield {
                    "direction": "inverse",
                    "page": page,
                    "symbol": symbol,
                    "ok": False,
                    "error": str(error),
                }
                continue
            entry = {
                "direction": "inverse",
                "page": page,
                "x": x,
                "y": y,
                "symbol": symbol,
                "glyph_box": box,
                "ok": raw is not None,
                "raw_line": raw[0] if raw else None,
            }
            if raw is None:
                entry["error"] = "synctex edit returned no match"
            else:
                file = input_path(raw[1], pdf, source)
                entry["raw_file"] = str(file)
                if not file.is_file() or raw[0] < 1:
                    entry["ok"] = False
                    entry["error"] = (
                        "synctex edit returned a missing file or invalid line"
                    )
                else:
                    try:
                        view_pages = raw_pages(pdf, file, raw[0])
                        entry["roundtrip_page_match"] = page in view_pages
                        entry["roundtrip_pages"] = sorted(view_pages)
                    except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
                        entry["ok"] = False
                        entry["error"] = str(error)
            yield entry


def sample_glyph_points(
    pdf: Path, page_count: int, pages: int, points: int, rng: random.Random
) -> tuple[dict[int, list[GlyphPoint]], int]:
    """Sample character boxes with paint, opacity, and an on-page center."""
    if pages == 0 or points == 0:
        return {}, 0
    try:
        import pymupdf
    except ImportError as error:
        raise SystemExit(
            "inverse glyph probes require PyMuPDF in the Python environment"
        ) from error

    selected = {}
    skipped = 0
    candidates = list(range(1, page_count + 1))
    rng.shuffle(candidates)
    with pymupdf.open(pdf) as document:
        if document.page_count != page_count:
            raise SystemExit("PyMuPDF page count differs from pdfinfo")
        for page_number in candidates:
            page = document[page_number - 1]
            glyphs = []
            for span in page.get_texttrace():
                if span["type"] not in (0, 1) or span["opacity"] <= 0:
                    continue
                for codepoint, _, _, coordinates in span["chars"]:
                    if not 0 < codepoint <= 0x10FFFF:
                        continue
                    symbol = chr(codepoint)
                    box = pymupdf.Rect(coordinates)
                    if (
                        not symbol.isspace()
                        and box.width > 0
                        and box.height > 0
                        and page.rect.contains(box.tl + (box.br - box.tl) / 2)
                    ):
                        glyphs.append((symbol, tuple(coordinates)))
            if glyphs:
                selected[page_number] = rng.sample(glyphs, min(points, len(glyphs)))
                if len(selected) == pages:
                    break
            else:
                skipped += 1
    if not selected:
        raise SystemExit("no on-page painted text glyphs to probe in this PDF")
    return dict(sorted(selected.items())), skipped


def pdf_page_count(pdf: Path) -> int:
    result = subprocess.run(
        ["pdfinfo", str(pdf)], capture_output=True, text=True, check=False
    )
    if result.returncode:
        raise SystemExit(f"pdfinfo: {result.stderr.strip()[:200]}")
    match = re.search(r"^Pages:\s+(\d+)$", result.stdout, re.MULTILINE)
    if not match:
        raise SystemExit("pdfinfo returned no page count")
    return int(match.group(1))


def pdfterm_bin() -> Path:
    default = Path(__file__).parent.parent / "target" / "release" / "pdfterm"
    if default.exists():
        return default
    raise SystemExit(f"missing {default}; build pdfterm first")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pdf", type=Path)
    parser.add_argument(
        "--source",
        type=Path,
        help="source file to sample (default: PDF basename with .tex)",
    )
    parser.add_argument("--lines", type=int, default=100, help="source lines to sample")
    parser.add_argument(
        "--points", type=int, default=20, help="inverse points per sampled page"
    )
    parser.add_argument("--pages", type=int, default=12, help="inverse pages to sample")
    parser.add_argument("--max-fails", type=int, default=10)
    parser.add_argument("--seed", type=int, default=20260922)
    parser.add_argument(
        "--allow-stale", action="store_true", help="accept an older PDF/SyncTeX pair"
    )
    args = parser.parse_args()
    if min(args.lines, args.pages, args.points) < 0 or args.max_fails < 1:
        parser.error("counts must be nonnegative and --max-fails must be positive")

    pdf = args.pdf.resolve()
    source_file = (args.source or pdf.with_suffix(".tex")).resolve()
    if not source_file.is_file():
        raise SystemExit(f"missing source {source_file}; pass --source PATH")
    companion = next(
        (
            path
            for path in (pdf.with_suffix(".synctex.gz"), pdf.with_suffix(".synctex"))
            if path.exists()
        ),
        None,
    )
    if not pdf.is_file() or companion is None:
        raise SystemExit("missing PDF or matching SyncTeX sidecar")
    if (
        not args.allow_stale
        and min(pdf.stat().st_mtime_ns, companion.stat().st_mtime_ns)
        < source_file.stat().st_mtime_ns
    ):
        raise SystemExit(
            "PDF/SyncTeX pair predates the TeX source; rebuild or pass --allow-stale"
        )
    if companion.suffix == ".gz":
        with gzip.open(companion, "rb") as stream:
            while stream.read(1024 * 1024):
                pass
    source = source_file.read_text()

    rng = random.Random(args.seed)
    forward_positions = sample_source_positions(source, rng, args.lines)
    page_count = pdf_page_count(pdf)
    glyph_points, skipped_pages = sample_glyph_points(
        pdf, page_count, args.pages, args.points, rng
    )
    visible_words = pdf_visible_words(pdf) if forward_positions else []
    if visible_words and len(visible_words) != page_count:
        raise SystemExit("pdftotext page count differs from pdfinfo")

    results = []
    failures = 0
    for probe in forward(
        pdf, source_file, forward_positions, page_count, visible_words
    ):
        results.append(probe)
        failures += not probe["ok"]
        if failures >= args.max_fails:
            break
    if failures < args.max_fails:
        for probe in inverse(pdf, source_file, glyph_points):
            results.append(probe)
            failures += not probe["ok"]
            if failures >= args.max_fails:
                break
    expected_probes = len(forward_positions) + sum(map(len, glyph_points.values()))
    stopped = len(results) < expected_probes
    print(
        json.dumps(
            {
                "pdf": str(pdf),
                "source": str(source_file),
                "probes": len(results),
                "fails": failures,
                "stopped_early": stopped,
                "inverse_pages": len(glyph_points),
                "glyphless_pages_skipped": skipped_pages,
                "inverse_roundtrip_mismatches": sum(
                    probe.get("roundtrip_page_match") is False for probe in results
                ),
                "visible_checked": sum(
                    bool(probe.get("visible_checked")) for probe in results
                ),
                "results": results,
            },
            indent=2,
        )
    )
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
