#!/usr/bin/env python3
"""Fuzz forward and inverse SyncTeX mappings for any TeX-built PDF.

Forward probes compare pdfterm's page with raw SyncTeX results and use Poppler
text to catch a selected source word missing from one raw candidate when
another raw candidate contains the word and its source context. Raw page
disagreement is reported, not failed: a resolver may refine an imprecise
SyncTeX source position.
Inverse probes round-trip each raw `synctex edit` result through `synctex view`.
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
from pathlib import Path
from typing import Iterator


def raw_pages(pdf: Path, source: Path, line: int, column: int = 1) -> set[int]:
    result = subprocess.run(
        ["synctex", "view", "-i", f"{line}:{column}:{source}", "-o", str(pdf)],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode:
        raise RuntimeError(f"synctex view: {result.stderr.strip()[:200]}")
    return set(int(p) for p in re.findall(r"^Page:(\d+)$", result.stdout, re.M))


def raw_inverse(pdf: Path, x: float, y: float, page: int) -> tuple[int, str] | None:
    result = subprocess.run(
        ["synctex", "edit", "-o", f"{page}:{x:.2f}:{y:.2f}:{pdf}"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode:
        raise RuntimeError(f"synctex edit: {result.stderr.strip()[:200]}")
    matches = re.findall(r"^Input:(.*)$\n^Line:(\d+)", result.stdout, re.M)
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
            yield {"direction": "forward", "line": line, "ok": False, "error": str(error)}
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
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            yield {"direction": "forward", "line": line, "column": column,
                   "ok": False, "error": str(error)}
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
            valid = (
                bool(expected)
                and got["pdf"] == str(pdf)
                and type(page) is int
                and 1 <= page <= page_count
            )
        except (ValueError, KeyError, TypeError) as error:
            yield {"direction": "forward", "line": line, "ok": False, "error": str(error)}
            continue
        entry = {
            "direction": "forward",
            "line": line,
            "column": column,
            "ok": valid,
            "resolved_page": page,
            "raw_pages": sorted(expected),
            "raw_page_match": page in expected,
        }
        if (
            valid
            and visible_words is not None
            and len(expected) > 1
            and page in expected
            and got.get("word")
        ):
            try:
                hint = got["word"]
                chosen_score = context_score(visible_words[page - 1], hint)
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
                    entry["error"] = "selected word absent from chosen page but present with source context on another SyncTeX page"
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
    pages: list[int],
    n: int,
    info: dict[int, tuple[float, float]],
    rng: random.Random,
) -> Iterator[dict]:
    for page in pages:
        width, height = info[page]
        for point in sample_points(width, height, n, rng):
            try:
                raw = raw_inverse(pdf, point[0], point[1], page)
            except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
                yield {"direction": "inverse", "page": page, "ok": False, "error": str(error)}
                continue
            entry = {
                "direction": "inverse",
                "page": page,
                "x": point[0],
                "y": point[1],
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
                    entry["error"] = "synctex edit returned a missing file or invalid line"
                else:
                    try:
                        view_pages = raw_pages(pdf, file, raw[0])
                        entry["ok"] = page in view_pages
                        if not entry["ok"]:
                            entry["error"] = "synctex edit/view round trip changed page"
                            entry["raw_pages"] = sorted(view_pages)
                    except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
                        entry["ok"] = False
                        entry["error"] = str(error)
            yield entry


def sample_points(
    width: float, height: float, n: int, rng: random.Random
) -> list[tuple[float, float]]:
    columns = (0.1, 0.35, 0.6, 0.85)
    rows = (0.1, 0.3, 0.5, 0.7, 0.9)
    points = [(width * fx, height * fy) for fy in rows for fx in columns]
    points.extend(
        (width * rng.uniform(0.05, 0.95), height * rng.uniform(0.05, 0.95))
        for _ in range(max(0, n - len(points)))
    )
    return points[:n]


def pdfinfo(pdf: Path) -> dict[int, tuple[float, float]]:
    sizes = {}
    result = subprocess.run(
        ["pdfinfo", "-l", "1000000", str(pdf)], capture_output=True, text=True
    )
    if result.returncode:
        raise SystemExit(f"pdfinfo: {result.stderr.strip()[:200]}")
    for row in result.stdout.splitlines():
        if match := re.match(r"Page\s+(\d+) size:\s+([\d.]+) x ([\d.]+)", row):
            sizes[int(match.group(1))] = (float(match.group(2)), float(match.group(3)))
    if not sizes:
        raise SystemExit("pdfinfo returned no page sizes")
    return sizes


def pdfterm_bin() -> Path:
    default = Path(__file__).parent.parent / "target" / "release" / "pdfterm"
    if default.exists():
        return default
    raise SystemExit(f"missing {default}; build pdfterm first")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pdf", type=Path)
    parser.add_argument("--source", type=Path, help="source file to sample (default: PDF basename with .tex)")
    parser.add_argument("--lines", type=int, default=100, help="source lines to sample")
    parser.add_argument("--points", type=int, default=20, help="inverse points per sampled page")
    parser.add_argument("--pages", type=int, default=12, help="inverse pages to sample")
    parser.add_argument("--max-fails", type=int, default=10)
    parser.add_argument("--seed", type=int, default=20260922)
    parser.add_argument("--allow-stale", action="store_true", help="accept an older PDF/SyncTeX pair")
    args = parser.parse_args()
    if min(args.lines, args.pages, args.points) < 0 or args.max_fails < 1:
        parser.error("counts must be nonnegative and --max-fails must be positive")

    pdf = args.pdf.resolve()
    source_file = (args.source or pdf.with_suffix(".tex")).resolve()
    if not source_file.is_file():
        raise SystemExit(f"missing source {source_file}; pass --source PATH")
    companion = next(
        (path for path in (pdf.with_suffix(".synctex.gz"), pdf.with_suffix(".synctex")) if path.exists()),
        None,
    )
    if not pdf.is_file() or companion is None:
        raise SystemExit("missing PDF or matching SyncTeX sidecar")
    if not args.allow_stale and min(pdf.stat().st_mtime_ns, companion.stat().st_mtime_ns) < source_file.stat().st_mtime_ns:
        raise SystemExit("PDF/SyncTeX pair predates the TeX source; rebuild or pass --allow-stale")
    if companion.suffix == ".gz":
        with gzip.open(companion, "rb") as stream:
            while stream.read(1024 * 1024):
                pass
    source = source_file.read_text()

    rng = random.Random(args.seed)
    forward_positions = sample_source_positions(source, rng, args.lines)
    info = pdfinfo(pdf)
    pages = sorted(rng.sample(list(info), min(args.pages, len(info))))
    visible_words = pdf_visible_words(pdf) if forward_positions else []
    if visible_words and len(visible_words) != len(info):
        raise SystemExit("pdftotext page count differs from pdfinfo")

    results = []
    failures = 0
    for probe in forward(pdf, source_file, forward_positions, len(info), visible_words):
        results.append(probe)
        failures += not probe["ok"]
        if failures >= args.max_fails:
            break
    if failures < args.max_fails:
        for probe in inverse(pdf, source_file, pages, args.points, info, rng):
            results.append(probe)
            failures += not probe["ok"]
            if failures >= args.max_fails:
                break
    stopped = len(results) < len(forward_positions) + len(pages) * args.points
    print(
        json.dumps(
            {
                "pdf": str(pdf),
                "source": str(source_file),
                "probes": len(results),
                "fails": failures,
                "stopped_early": stopped,
                "visible_checked": sum(bool(probe.get("visible_checked")) for probe in results),
                "results": results,
            },
            indent=2,
        )
    )
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
