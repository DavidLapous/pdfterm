#!/usr/bin/env python3
"""Fuzz forward and inverse SyncTeX mappings for any TeX-built PDF.

Forward probes compare pdfterm's page with raw SyncTeX results. Inverse probes
enumerate painted PDF words with PyMuPDF, then send all selected word centers
through pdfterm's PDFium + inverse resolver batch diagnostic. A word is checked
against literal source in its raw-anchor frame when possible; macro-generated,
ambiguous, and unsupported hits are abstentions, never passes. Document
metadata (for example a footer title) is accounted for separately.
The report is one JSON object with exact probe failures and deterministic
coverage totals. Any failed probe exits nonzero.

Usage: synctex_fuzz.py PDF [--source TEX] [--lines N] [--page N ...]
       [--pages N] [--words N] [--seed N] [--allow-stale]

By default every eligible painted word on every page is probed. --pages and
--words provide seed-deterministic quick caps; --pages 0 disables inverse
probes and --words 0 means all words on each selected page. Repeat --page to
probe exact one-based pages (for example, --page 27 --page 28).
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

WordPoint = tuple[str, tuple[float, float, float, float], float]


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


def line_has_word(line: str, word: str) -> bool:
    text = source_line_text(line)
    if re.search(r"\\(?:newcommand|renewcommand|providecommand|def|gdef|edef|let)\b", text):
        return False
    text = re.sub(
        r"\\(?:includegraphics|includesvg|includepdf|includemedia|pgfimage|graphicspath|input|include|bibliography|addbibresource)\*?\s*(?:\[[^\]]*\]\s*)?\{[^}]*\}",
        " ",
        text,
    )
    text = re.sub(r"\\(?:label|ref|eqref|pageref|autoref|cref|Cref|cite|nocite)\*?\s*(?:\[[^\]]*\]\s*)?\{[^}]*\}", " ", text)
    text = re.sub(r"\\href\s*\{[^}]*\}\s*\{([^}]*)\}", r"\1", text)
    text = re.sub(r"\b[\w.-]+\.(?:pdf|png|jpe?g|svg|eps)\b", " ", text, flags=re.IGNORECASE)
    text = re.sub(r"\\[A-Za-z@]+\*?", " ", text)
    tokens = normalized_words(text)
    needle = normalized_words(word)
    return len(needle) == 1 and needle[0] in tokens


def metadata_line(line: str) -> bool:
    return bool(re.search(r"\\(?:title|subtitle|author|date|institute)\b", source_line_text(line)))


def frame_bounds(lines: list[str], line_number: int) -> tuple[int, int] | None:
    index = line_number - 1
    if not 0 <= index < len(lines):
        return None
    start = None
    for cursor in range(index, -1, -1):
        if re.search(r"\\begin\s*\{frame\}", source_line_text(lines[cursor])):
            start = cursor
            break
        if cursor == index:
            continue
        if re.search(r"\\end\s*\{frame\}", source_line_text(lines[cursor])):
            return None
    if start is None:
        return None
    for end in range(index, len(lines)):
        if re.search(r"\\end\s*\{frame\}", source_line_text(lines[end])):
            return start, end
    return None


def inverse(
    pdf: Path,
    source: Path,
    word_points: dict[int, list[WordPoint]],
) -> Iterator[dict]:
    points = []
    for page, words in word_points.items():
        for word, box, page_height in words:
            x = (box[0] + box[2]) / 2
            y = (box[1] + box[3]) / 2
            points.append({"page": page, "x": x, "y": y, "word": word, "box": box, "page_height": page_height})
    if not points:
        return
    result = subprocess.run(
        [str(pdfterm_bin()), str(pdf), "--synctex-edit-batch"],
        input="".join(json.dumps({key: point[key] for key in ("page", "x", "y")}) + "\n" for point in points),
        capture_output=True,
        text=True,
        timeout=max(30, len(points) * 2),
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"pdfterm --synctex-edit-batch exited {result.returncode}: {result.stderr.strip()[:500]}")
    rows = []
    for line in result.stdout.splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError as error:
            raise RuntimeError(f"invalid batch JSON: {error}: {line[:200]}") from error
    if len(rows) != len(points):
        raise RuntimeError(f"batch returned {len(rows)} results for {len(points)} points")
    source_lines = source.read_text(errors="replace").splitlines()
    for point, batch in zip(points, rows):
        page, word = point["page"], point["word"]
        entry = {
            "direction": "inverse",
            "page": page,
            "word": word,
            "word_box": point["box"],
            "page_height": point["page_height"],
            "raw_anchor": None,
            "location": batch.get("location"),
            "warning": batch.get("warning"),
            "ok": False,
        }
        if batch.get("warning"):
            entry.update(abstained=True, reason="resolver_warning", error=batch["warning"])
            yield entry
            continue
        try:
            raw = raw_inverse(pdf, point["x"], point["y"], page)
        except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
            entry["error"] = f"raw SyncTeX edit failed: {error}"
            yield entry
            continue
        if raw is None:
            entry["error"] = "raw SyncTeX edit returned no match"
            yield entry
            continue
        entry["raw_anchor"] = {"line": raw[0], "file": raw[1]}
        raw_file = input_path(raw[1], pdf, source)
        if not batch.get("ok"):
            error = batch.get("error", "batch returned no result")
            if "no text near point" in error or "hit-test failed" in error:
                entry.update(abstained=True, reason="pdfium_hit_unsupported", error=error)
            else:
                entry["error"] = error
            yield entry
            continue
        pdf_word = batch.get("pdf_word")
        entry["pdfium_word"] = pdf_word
        if not pdf_word:
            entry.update(abstained=True, reason="pdfium_selected_no_word")
            yield entry
            continue
        if normalized_words(pdf_word) != normalized_words(word):
            entry.update(abstained=True, reason="pdfium_word_mismatch")
            yield entry
            continue
        word = pdf_word
        entry["word"] = word
        location = batch["location"]
        resolved_file = Path(location["file"]).resolve()
        if resolved_file != source.resolve():
            entry.update(abstained=True, reason="resolved_source_is_not_requested_tex")
            yield entry
            continue
        resolved_line = int(location["line"])
        if not 1 <= resolved_line <= len(source_lines):
            entry["error"] = "SyncTeX source file or line is invalid"
            yield entry
            continue
        if location["precise"] is False:
            entry.update(abstained=True, reason="coarse_source_refinement")
            yield entry
            continue
        bounds = frame_bounds(source_lines, raw[0]) if raw_file == source.resolve() else None
        frame_has_word = bool(
            bounds and any(line_has_word(source_lines[i], word) for i in range(bounds[0], bounds[1] + 1))
        )
        metadata_occurrences = [
            line for line in source_lines if metadata_line(line) and line_has_word(line, word)
        ]
        if point["y"] >= point["page_height"] * 0.75 and metadata_occurrences:
            if metadata_line(source_lines[resolved_line - 1]) and line_has_word(source_lines[resolved_line - 1], word):
                entry.update(ok=True, match="document_metadata")
            else:
                entry.update(abstained=True, reason="footer_metadata_anchor_ambiguous")
            yield entry
            continue
        if frame_has_word:
            start, end = bounds
            if start + 1 <= resolved_line <= end + 1 and line_has_word(source_lines[resolved_line - 1], word):
                entry.update(ok=True, match="literal_frame_word")
            else:
                entry["error"] = "inverse refinement missed the literal word in the raw-anchor frame"
        elif line_has_word(source_lines[resolved_line - 1], word):
            entry.update(ok=True, match="document_metadata" if metadata_line(source_lines[resolved_line - 1]) else "literal_source")
        else:
            occurrences = [
                i for i, line in enumerate(source_lines)
                if line_has_word(line, word) and not line.lstrip().startswith("%")
            ]
            if not occurrences:
                entry.update(abstained=True, reason="word_not_literal_in_source_macro_or_generated")
            elif metadata_line(source_lines[resolved_line - 1]):
                entry.update(abstained=True, reason="document_metadata_not_frame_text")
            elif len(occurrences) > 1:
                entry.update(abstained=True, reason="ambiguous_literal_source_occurrences")
            else:
                entry["error"] = "inverse refinement selected a source line without the PDF word"
        yield entry


def painted_word_points(
    pdf: Path, page_count: int, page_limit: int | None, word_limit: int,
    rng: random.Random, specific_pages: list[int] | None,
) -> tuple[dict[int, list[WordPoint]], dict]:
    """Return every on-page word made from visible painted text characters."""
    if page_limit == 0 and specific_pages is None:
        return {}, {
            "eligible_pages": None,
            "pages_without_eligible_words": None,
            "selected_pages": [],
            "total": None,
            "selected": 0,
            "raw_extracted_words": None,
            "eligibility_filtered_words": None,
            "page_cap_unselected": None,
            "word_cap_unselected": 0,
            "eligible_words_by_selected_page": {},
            "selected_pages_without_eligible_words": [],
        }
    try:
        import pymupdf
    except ImportError as error:
        raise SystemExit("inverse word probes require PyMuPDF in the Python environment") from error
    selected = {}
    eligible_by_page = {}
    with pymupdf.open(pdf) as document:
        if document.page_count != page_count:
            raise SystemExit("PyMuPDF page count differs from pdfinfo")
        raw_extracted = 0
        for page_number, page in enumerate(document, 1):
            words = []
            painted = [
                (chr(codepoint), pymupdf.Rect(bounds))
                for span in page.get_texttrace()
                if span["type"] in (0, 1) and span["opacity"] > 0
                for codepoint, _, _, bounds in span["chars"]
                if 0 < codepoint <= 0x10FFFF
            ]
            text_words = page.get_text("words")
            raw_extracted += len(text_words)
            for x0, y0, x1, y1, word, *_ in text_words:
                rect = pymupdf.Rect(x0, y0, x1, y1)
                if rect.width <= 0 or rect.height <= 0:
                    continue
                if not page.rect.contains(rect.tl + (rect.br - rect.tl) / 2):
                    continue
                painted_word = "".join(
                    char for char, glyph in painted
                    if rect.contains(glyph.tl + (glyph.br - glyph.tl) / 2)
                )
                if normalized_words(word) and normalized_words(word) == normalized_words(painted_word):
                    words.append((word, tuple(rect), page.rect.height))
            if words:
                eligible_by_page[page_number] = words
    candidates = sorted(eligible_by_page)
    if specific_pages is not None:
        chosen = sorted(specific_pages)
    else:
        chosen = candidates if page_limit is None else sorted(rng.sample(candidates, min(page_limit, len(candidates))))
    for page in chosen:
        words = eligible_by_page.get(page, [])
        selected[page] = rng.sample(words, min(word_limit, len(words))) if word_limit else words
    eligible_total = sum(map(len, eligible_by_page.values()))
    coverage = {
        "eligible_pages": len(eligible_by_page),
        "pages_without_eligible_words": page_count - len(eligible_by_page),
        "selected_pages": chosen,
        "total": eligible_total,
        "raw_extracted_words": raw_extracted,
        "eligibility_filtered_words": raw_extracted - eligible_total,
        "selected": sum(map(len, selected.values())),
        "page_cap_unselected": sum(len(eligible_by_page[page]) for page in set(candidates) - set(chosen)),
        "word_cap_unselected": sum(len(eligible_by_page.get(page, [])) - len(selected[page]) for page in chosen),
        "eligible_words_by_selected_page": {page: len(eligible_by_page.get(page, [])) for page in chosen},
        "selected_pages_without_eligible_words": [page for page in chosen if not eligible_by_page.get(page)],
    }
    return selected, coverage


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
    parser.add_argument("--page", type=int, action="append", help="probe this one-based page; repeatable")
    parser.add_argument("--lines", type=int, default=100, help="source lines to sample")
    parser.add_argument("--pages", type=int, help="seed-selected inverse page cap; default probes all pages")
    parser.add_argument("--words", type=int, default=0, help="max words per selected page; 0 means all")
    parser.add_argument("--seed", type=int, default=20260922)
    parser.add_argument("--allow-stale", action="store_true", help="accept an older PDF/SyncTeX pair")
    args = parser.parse_args()
    if (
        min(args.lines, args.words) < 0
        or (args.pages is not None and args.pages < 0)
        or (args.page is not None and (args.pages is not None or min(args.page) < 1 or len(set(args.page)) != len(args.page)))
    ):
        parser.error("counts must be nonnegative; --page values must be unique and cannot combine with --pages")

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
    if args.page and max(args.page) > page_count:
        parser.error(f"--page must not exceed the PDF's {page_count} pages")
    word_points, inverse_coverage = painted_word_points(
        pdf, page_count, args.pages, args.words, rng, args.page
    )
    visible_words = pdf_visible_words(pdf) if forward_positions else []
    if visible_words and len(visible_words) != page_count:
        raise SystemExit("pdftotext page count differs from pdfinfo")

    results = list(forward(pdf, source_file, forward_positions, page_count, visible_words))
    batch_error = None
    try:
        results.extend(inverse(pdf, source_file, word_points))
    except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
        batch_error = str(error)
    failures = sum(not probe["ok"] and not probe.get("abstained") for probe in results) + bool(batch_error)
    inverse_results = [probe for probe in results if probe.get("direction") == "inverse"]
    reasons = {}
    for probe in inverse_results:
        if probe.get("abstained"):
            reason = probe["reason"]
            reasons[reason] = reasons.get(reason, 0) + 1
    if inverse_coverage["page_cap_unselected"]:
        reasons["page_cap_unselected"] = inverse_coverage["page_cap_unselected"]
    if inverse_coverage["word_cap_unselected"]:
        reasons["word_cap_unselected"] = inverse_coverage["word_cap_unselected"]
    abstained = sum(bool(probe.get("abstained")) for probe in inverse_results)
    inverse_coverage.update({
        "probed": len(inverse_results),
        "checked": len(inverse_results) - abstained,
        "abstained": abstained,
        "reasons": reasons,
        "failed": sum(not probe["ok"] and not probe.get("abstained") for probe in inverse_results) + bool(batch_error),
        "document_metadata_matches": sum(probe.get("match") == "document_metadata" for probe in inverse_results),
    })
    print(json.dumps({
        "pdf": str(pdf),
        "source": str(source_file),
        "probes": len(results),
        "fails": failures,
        "stopped_early": bool(batch_error),
        "batch_error": batch_error,
        "inverse_coverage": inverse_coverage,
        "visible_checked": sum(bool(probe.get("visible_checked")) for probe in results),
        "results": results,
    }, indent=2))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
