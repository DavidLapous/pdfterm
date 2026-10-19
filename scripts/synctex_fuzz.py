#!/usr/bin/env python3
"""Fuzz forward and inverse SyncTeX mappings for any TeX-built PDF.

Forward probes compare pdfterm's page with raw SyncTeX results. Inverse probes
enumerate painted PDF words with PyMuPDF, then send all selected word centers
through pdfterm's PDFium + inverse resolver batch diagnostic. File/line identity
is checked even for coarse results; fragile frames use the clicked sheet's
original source-map box, never the final reused .vrb contents. Precision is
checked against literal printed source spans; unsupported math remains separate.
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
import math
import random
import re
import subprocess
import sys
import unicodedata
from collections.abc import Iterator
from pathlib import Path

WordPoint = tuple[str, tuple[float, float, float, float], float]
PDFTERM_BINARY = Path(__file__).parent.parent / "target" / "release" / "pdfterm"


def usable_forward_record(record: dict[str, int | float]) -> bool:
    page = record.get("Page", 0)
    if not isinstance(page, int) or page <= 0:
        return False
    if not all(key in record for key in ("h", "v", "W", "H")):
        return False
    h, v, w, height = record["h"], record["v"], record["W"], record["H"]
    return (
        all(math.isfinite(val) for val in (h, v, w, height, h + w, v - height))
        and w >= 0
        and height >= 0
        and (w > 0 or height > 0)
    )


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
    pages, record = set(), {}
    for row in [*result.stdout.splitlines(), ""]:
        key, sep, value = row.strip().partition(":")
        if sep and key in {"Page", "h", "v", "W", "H"}:
            if key == "Page" and "Page" in record:
                if usable_forward_record(record):
                    pages.add(int(record["Page"]))
                record = {}
            try:
                record[key] = int(value) if key == "Page" else float(value)
            except ValueError:
                pass
        elif not row.strip() or row.strip().startswith("SyncTeX result end"):
            if usable_forward_record(record):
                pages.add(int(record["Page"]))
            record = {}
    return pages


def raw_inverse(pdf: Path, x: float, y: float, page: int) -> list[tuple[int, str]]:
    result = subprocess.run(
        ["synctex", "edit", "-o", f"{page}:{x}:{y}:{pdf}"],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"synctex edit: {result.stderr.strip()[:200]}")
    seen, records, current_input = set(), [], None
    for row in result.stdout.splitlines():
        if row.startswith("Input:"):
            current_input = row.removeprefix("Input:").strip()
        elif row.startswith("Line:") and current_input:
            file, current_input = current_input, None
            try:
                line = int(row.removeprefix("Line:").strip())
            except ValueError:
                continue
            if line > 0 and (file, line) not in seen:
                seen.add((file, line))
                records.append((line, file))
    return records


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
    source_text: str,
    positions: list[tuple[int, int]],
    page_count: int,
    visible_words: list[list[str]] | None = None,
    best_guess: bool = False,
) -> Iterator[dict]:
    extra = ["--synctex-best-guess"] if best_guess else []
    for line, column in positions:
        view_line = (
            frame_closing_line(source_text, line)
            or caption_closing_line(source_text, line, column)
            or line
        )
        try:
            expected = raw_pages(pdf, source, view_line, column)
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
                    *extra,
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
                "ok": not expected
                and (
                    "no complete match" in result.stderr
                    or "no usable compiler anchor geometry" in result.stderr
                ),
                "error": result.stderr.strip()[:200],
                "raw_pages": sorted(expected),
            }
            continue
        try:
            got = json.loads(result.stdout)
            page = got["page"]
            h, v, width, height = got["h"], got["v"], got["width"], got["height"]
            valid = (
                got["pdf"] == str(pdf)
                and type(page) is int
                and 1 <= page <= page_count
                and (not expected or page in expected)
                and all(
                    isinstance(val, (int, float))
                    and not isinstance(val, bool)
                    and math.isfinite(val)
                    for val in (h, v, width, height, h + width, v - height)
                )
                and width >= 0
                and height >= 0
                and (width > 0 or height > 0)
            )
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


def read_source_map(pdf: Path) -> dict:
    companion = next((path for path in (pdf.with_suffix(".synctex.gz"), pdf.with_suffix(".synctex")) if path.is_file()), None)
    if companion is None:
        raise RuntimeError("missing SyncTeX source map")
    opener = gzip.open if companion.suffix == ".gz" else open
    inputs, origins, page_inputs = {}, {}, {}
    page = None
    with opener(companion, "rt", encoding="utf-8") as stream:
        size = 0
        for row in stream:
            size += len(row.encode("utf-8"))
            if size > 128 * 1024 * 1024:
                raise RuntimeError("SyncTeX source map exceeds 128 MiB")
            if row.startswith("Input:"):
                tag, _, name = row[len("Input:"):].rstrip("\r\n").partition(":")
                path = Path(name)
                inputs[int(tag)] = (path if path.is_absolute() else pdf.parent / path).resolve()
            elif re.fullmatch(r"\{\d+\s*", row):
                page = int(row[1:])
                page_inputs[page] = set()
            elif page is not None and row.startswith("}"):
                if int(row[1:]) != page:
                    raise RuntimeError("SyncTeX source map has mismatched sheet bounds")
                page = None
            elif page is not None:
                node = re.match(r"([\[(vhkgx$r])(\d+),(\d+)(?:,\d+)?:", row)
                if not origins.get(page):
                    if not node or node[1] not in "[(":
                        raise RuntimeError("SyncTeX sheet has no enclosing source box")
                    origins[page] = (int(node[2]), int(node[3]))
                if node:
                    page_inputs[page].add(int(node[2]))
    if page is not None or 1 not in inputs:
        raise RuntimeError("SyncTeX source map is incomplete or has no main input")
    return {"inputs": inputs, "origins": origins, "page_inputs": page_inputs}


def tex_group_end(text: str, start: int, opening: str = "{") -> int | None:
    closing = "}" if opening == "{" else "]"
    start = tex_space_end(text, start)
    if start >= len(text) or text[start] != opening:
        return None
    depth, at = 1, start + 1
    while at < len(text):
        char = text[at]
        if char == "\\":
            at += 2
            continue
        if char == "%":
            end = text.find("\n", at)
            at = len(text) if end < 0 else end
            continue
        if opening == "[" and char == "{":
            end = tex_group_end(text, at)
            if end is None:
                return None
            at = end
            continue
        depth += (char == opening) - (char == closing)
        at += 1
        if depth == 0:
            return at
    return None


def tex_space_end(text: str, at: int) -> int:
    while at < len(text):
        if text[at].isspace():
            at += 1
        elif text[at] == "%":
            end = text.find("\n", at)
            at = len(text) if end < 0 else end
        else:
            break
    return at


def overlay_end(text: str, start: int) -> int | None:
    if start >= len(text) or text[start] != "<":
        return None
    end = text.find(">", start + 1)
    newline = text.find("\n", start + 1)
    if end < 0 or (0 <= newline < end):
        return None
    body = text[start + 1 : end]
    if not body:
        return None
    has_action = ":" in body or "@" in body
    allowed = set("0123456789+-,.()|* ")
    if has_action:
        allowed.update("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ:@")
    return end + 1 if all(ch in allowed for ch in body) else None


def opaque_command_end(text: str, command: str, at: int) -> tuple[int, bool] | None:
    if command == "verb":
        if at < len(text) and text[at] == "*":
            at += 1
        if at >= len(text):
            return None
        delim = text[at]
        at += 1
        end = text.find(delim, at)
        return (len(text) if end < 0 else end + 1, False)
    if command != "begin":
        return None
    body = tex_space_end(text, at)
    end = tex_group_end(text, body)
    if end is None:
        return None
    env = text[body + 1 : end - 1]
    base = env.rstrip("*")
    if base not in {"verbatim", "Verbatim", "lstlisting", "minted", "comment"}:
        return None
    if base == "comment":
        nl = text.find("\n", end)
        if nl < 0:
            return (len(text), True)
        pos = nl + 1
        for row in text[pos:].splitlines(keepends=True):
            pos += len(row)
            if row.rstrip("\r\n").rstrip(" ") == f"\\end{{{env}}}":
                return (pos, True)
        return (len(text), True)
    if base in {"Verbatim", "minted"}:
        nl = text.find("\n", end)
        if nl < 0:
            return (len(text), False)
        pos = nl + 1
        for row in text[pos:].splitlines(keepends=True):
            pos += len(row)
            idx = row.find("\\end{")
            if idx >= 0 and row[idx + 5 :].startswith(f"{env}}}"):
                return (pos, False)
        return (len(text), False)
    close = text.find(f"\\end{{{env}}}", end)
    return (len(text) if close < 0 else close + len(env) + 6, False)


def nonprinting_ranges(text: str) -> list[tuple[int, int]]:
    definitions = {
        "newcommand", "renewcommand", "providecommand", "DeclareRobustCommand",
        "NewDocumentCommand", "RenewDocumentCommand", "ProvideDocumentCommand",
        "DeclareDocumentCommand", "newenvironment", "renewenvironment",
        "def", "gdef", "edef", "xdef",
    }
    keys = {
        "label", "ref", "eqref", "pageref", "autoref", "cref", "Cref", "cite",
        "citep", "citet", "parencite", "textcite", "autocite", "footcite",
        "nocite", "href", "includegraphics", "includesvg", "input", "include",
        "bibliography", "addbibresource",
    }
    hidden, at = [], 0
    while at < len(text):
        if text[at] == "%":
            end = text.find("\n", at)
            at = len(text) if end < 0 else end
            continue
        if text[at] == "<" and at > 0 and text[at - 1] in "}]":
            if (end := overlay_end(text, at)) is not None:
                hidden.append((at, end))
                at = end
                continue
            at += 1
            continue
        if text[at] != "\\":
            at += 1
            continue
        match = re.match(r"\\([A-Za-z@]+)", text[at:])
        if match is None:
            at += 2
            continue
        start, command, cmd_end = at, match[1], at + match.end()
        if (opaque := opaque_command_end(text, command, cmd_end)) is not None:
            at, is_hidden = opaque
            if is_hidden:
                hidden.append((start, at))
            continue
        if command in {"let", "futurelet"}:
            at = tex_space_end(text, cmd_end)
            if at < len(text):
                at += (re.match(r"\\[A-Za-z@]+|\\.", text[at:], re.DOTALL).end() if text[at] == "\\" else 1)
            at = tex_space_end(text, at)
            if command == "let" and at < len(text) and text[at] == "=":
                at = tex_space_end(text, at + 1)
            if at < len(text):
                at += (re.match(r"\\[A-Za-z@]+|\\.", text[at:], re.DOTALL).end() if text[at] == "\\" else 1)
            hidden.append((start, at))
            continue
        at = cmd_end + (1 if cmd_end < len(text) and text[cmd_end] in "*+" else 0)
        if command in definitions:
            if command in {"def", "gdef", "edef", "xdef"}:
                while at < len(text) and text[at] != "{":
                    if text[at] == "\\":
                        at += 2
                    elif text[at] == "%":
                        at = tex_space_end(text, at)
                    else:
                        at += 1
            else:
                at = tex_space_end(text, at)
                if (end := tex_group_end(text, at)) is not None:
                    at = end
                elif (name := re.match(r"\\[A-Za-z@]+|\\.", text[at:], re.DOTALL)) is not None:
                    at += name.end()
                while (end := tex_group_end(text, at, "[")) is not None:
                    at = end
                if "Document" in command:
                    end = tex_group_end(text, at)
                    if end is None:
                        continue
                    at = end
            body_start = tex_space_end(text, at)
            end = tex_group_end(text, body_start)
            if end is None:
                if body_start < len(text) and text[body_start] == "{":
                    hidden.append((start, len(text)))
                    break
                at = body_start
                continue
            at = end
            if command.endswith("environment"):
                at = tex_group_end(text, at) or at
            hidden.append((start, at))
        elif command in keys:
            while (end := tex_group_end(text, at, "[") or overlay_end(text, at)) is not None:
                at = end
            body = tex_space_end(text, at)
            end = tex_group_end(text, body)
            if end is not None:
                hidden.append((start if command in {"includegraphics", "includesvg"} else body, end))
                at = end
        elif (end := overlay_end(text, at)) is not None:
            hidden.append((cmd_end, end))
            at = end
    return hidden


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
    hidden = nonprinting_ranges(text)
    text = "".join(" " if any(start <= at < end for start, end in hidden) else char for at, char in enumerate(text))
    text = re.sub(r"\\[A-Za-z@]+\*?", " ", text)
    needle = normalized_words(word)
    return len(needle) == 1 and needle[0] in normalized_words(text)


def metadata_spans(text: str) -> list[tuple[int, int]]:
    spans, start = [], 0
    for raw_row in text.splitlines(keepends=True):
        visible = source_line_text(raw_row.rstrip("\r\n"))
        for match in re.finditer(r"\\(?:title|subtitle|author|date|institute)(?![A-Za-z@])\*?", visible):
            at = start + match.end()
            while (opt_end := tex_group_end(text, at, "[")) is not None:
                spans.append((tex_space_end(text, at) + 1, opt_end - 1))
                at = opt_end
            body = tex_space_end(text, at)
            if (end := tex_group_end(text, body)) is not None:
                spans.append((body + 1, end))
        start += len(raw_row)
    return spans


def frame_bounds(text: str, line_number: int) -> tuple[int, int] | None:
    anchor = line_number - 1
    if anchor < 0:
        return None
    ignored, scan = [], 0
    while scan < len(text):
        if text[scan] == "%":
            end = text.find("\n", scan)
            scan = len(text) if end < 0 else end
        elif text[scan] == "\\":
            match = re.match(r"\\([A-Za-z@]+|.)", text[scan:], re.DOTALL)
            if match is None:
                scan += 1
                continue
            end = scan + match.end()
            if match[1].isalpha() or "@" in match[1]:
                if (opaque := opaque_command_end(text, match[1], end)) is not None:
                    ignored.append((scan, opaque[0]))
                    scan = opaque[0]
                    continue
            scan = end
        else:
            scan += 1
    ignored.extend(nonprinting_ranges(text))
    start, line_offset = None, 0
    for index, raw_row in enumerate(text.splitlines(keepends=True)):
        visible = source_line_text(raw_row.rstrip("\r\n"))
        cursor = 0
        while (rel := visible.find("\\", cursor)) >= 0:
            slash = rel
            abs_pos = line_offset + slash
            cursor = slash + 1
            if visible.startswith("\\", cursor):
                cursor += 1
                continue
            if any(lo <= abs_pos < hi for lo, hi in ignored):
                continue
            rest = visible[cursor:]
            if (m := re.match(r"begin\s*\{frame\}", rest)) is not None:
                cursor += m.end()
                if start is not None:
                    return None
                start = index
            elif (m := re.match(r"end\s*\{frame\}", rest)) is not None:
                cursor += m.end()
                if start is not None:
                    begin, start = start, None
                    if begin <= anchor <= index:
                        return begin, index
                elif anchor <= index:
                    return index, index
        line_offset += len(raw_row)
        if index >= anchor and start is None:
            return None
    return None


def frame_closing_line(text: str, line: int) -> int | None:
    bounds = frame_bounds(text, line)
    return bounds[1] + 1 if bounds is not None else None


def caption_argument(text: str, at: int) -> tuple[int, int, int] | None:
    if at < len(text) and text[at] == "*":
        at += 1
    at = tex_space_end(text, at)
    if at < len(text) and text[at] == "[":
        at = tex_group_end(text, at, "[")
        if at is None:
            return None
    body = tex_space_end(text, at)
    end = tex_group_end(text, body)
    return (body + 1, end - 1, end) if end is not None else None


def caption_bounds(text: str, line_number: int) -> tuple[int, int] | None:
    target = line_number - 1
    if target < 0:
        return None
    hidden = nonprinting_ranges(text)
    h_idx, i = 0, 0
    while i < len(text):
        while h_idx < len(hidden) and hidden[h_idx][1] <= i:
            h_idx += 1
        if h_idx < len(hidden) and hidden[h_idx][0] <= i:
            i = hidden[h_idx][1]
            continue
        if text[i] == "%":
            end = text.find("\n", i)
            i = len(text) if end < 0 else end
        elif text[i] == "\\":
            match = re.match(r"\\([A-Za-z]+|.)", text[i:], re.DOTALL)
            if match is None:
                return None
            name, i = match[1], i + match.end()
            if (opaque := opaque_command_end(text, name, i)) is not None:
                i = opaque[0]
            elif name == "caption" and (arg := caption_argument(text, i)) is not None:
                body_start, _, arg_end = arg
                first = text.count("\n", 0, body_start)
                last = first + text.count("\n", body_start, arg_end)
                if first <= target <= last:
                    return first, last
                if first > target:
                    return None
                i = arg_end
        else:
            i += 1
    return None


def caption_closing_line(text: str, line: int, column: int) -> int | None:
    rows = text.splitlines(keepends=True)
    if not (1 <= line <= len(rows) and column >= 1):
        return None
    row = rows[line - 1].rstrip("\r\n")
    if column - 1 >= len(row):
        return None
    pos = sum(len(r) for r in rows[: line - 1]) + (column - 1)
    hidden = nonprinting_ranges(text)
    h_idx, body_end, at = 0, None, 0
    while at <= pos:
        while h_idx < len(hidden) and hidden[h_idx][1] <= at:
            h_idx += 1
        if h_idx < len(hidden) and hidden[h_idx][0] <= at:
            at = hidden[h_idx][1]
            if pos < at:
                return None
            continue
        if text[at] == "%":
            end = text.find("\n", at)
            at = len(text) if end < 0 else end
            if pos < at:
                return None
        elif text[at] == "\\":
            match = re.match(r"\\([A-Za-z]+|.)", text[at:], re.DOTALL)
            if match is None:
                return None
            name, at = match[1], at + match.end()
            if (opaque := opaque_command_end(text, name, at)) is not None:
                at = opaque[0]
                if pos < at:
                    return None
            elif name == "caption" and (arg := caption_argument(text, at)) is not None:
                body_start, body_stop, arg_end = arg
                if body_start <= pos < body_stop:
                    at = body_start
                    body_end = body_stop
                else:
                    at = arg_end
        else:
            at += 1
    return text.count("\n", 0, body_end) + 1 if body_end is not None else None


def fragile_frame(text: str, starts: list[int], lines: list[str], bounds: tuple[int, int]) -> bool:
    row = source_line_text(lines[bounds[0]])
    row_start = starts[bounds[0]]
    at = None
    for match in re.finditer(r"(?<!\\)(?:\\\\)*\\begin", row):
        end = tex_group_end(text, row_start + match.end())
        if end is not None and text[tex_space_end(text, row_start + match.end()) + 1 : end - 1] == "frame":
            at = end
            break
    if at is None:
        return False
    at = tex_space_end(text, at)
    if at < len(text) and text[at] == "<":
        close = text.find(">", at)
        if close < 0:
            return False
        at = tex_space_end(text, close + 1)
    end = tex_group_end(text, at, "[")
    if end is None:
        return False
    options = text[tex_space_end(text, at) + 1 : end - 1]
    return any(
        (opt := source_line_text(line).strip()) == "fragile" or opt.startswith("fragile=")
        for part in options.split(",")
        for line in part.splitlines()
    )


def inverse(
    pdf: Path,
    source: Path,
    word_points: dict[int, list[WordPoint]],
    best_guess: bool = False,
) -> Iterator[dict]:
    points = []
    for page, words in word_points.items():
        for word, box, page_height in words:
            x = (box[0] + box[2]) / 2
            y = (box[1] + box[3]) / 2
            points.append({"page": page, "x": x, "y": y, "word": word, "box": box, "page_height": page_height})
    if not points:
        return
    extra = ["--synctex-best-guess"] if best_guess else []
    result = subprocess.run(
        [str(pdfterm_bin()), str(pdf), "--synctex-edit-batch", *extra],
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
    source_map = read_source_map(pdf)
    known_inputs = set(source_map["inputs"].values())
    sources = {}

    def source_info(path: Path) -> tuple[str, list[str], list[tuple[int, int]], list[int], list[tuple[int, int]]]:
        if path not in sources:
            text = path.read_text()
            starts, at = [], 0
            for row in text.splitlines(keepends=True):
                starts.append(at)
                at += len(row)
            sources[path] = (text, text.splitlines(), nonprinting_ranges(text), starts, metadata_spans(text))
        return sources[path]

    def candidate_anchor(page: int, raw: tuple[int, str]):
        raw_file = input_path(raw[1], pdf, source)
        expected_file, expected_anchor = raw_file, raw[0]
        bounds = None
        remapped = False
        generated = raw_file.suffix in {".vrb", ".toc", ".nav", ".out", ".aux", ".snm"} and raw_file.stem == pdf.stem
        if generated:
            origin = source_map["origins"].get(page)
            generated_on_page = any(
                source_map["inputs"].get(tag) == raw_file
                for tag in source_map["page_inputs"].get(page, set())
            )
            if origin and generated_on_page and origin[0] in source_map["inputs"]:
                original = source_map["inputs"][origin[0]]
                original_text, original_lines, _, original_starts, _ = source_info(original)
                original_bounds = frame_bounds(original_text, origin[1])
                if raw_file.suffix != ".vrb":
                    if 1 <= origin[1] <= len(original_lines):
                        expected_file, expected_anchor = original, origin[1]
                        bounds, remapped = original_bounds, True
                elif (
                    original_bounds
                    and origin[1] == original_bounds[1] + 1
                    and fragile_frame(original_text, original_starts, original_lines, original_bounds)
                ):
                    expected_file, expected_anchor = original, origin[1]
                    bounds, remapped = original_bounds, True
        else:
            raw_text = source_info(raw_file)[0]
            bounds = caption_bounds(raw_text, raw[0]) or frame_bounds(raw_text, raw[0])
        return raw, expected_file, expected_anchor, bounds, remapped, generated

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
        # Warnings/coarse results do not exempt file, line or remap identity.
        try:
            raw_records = raw_inverse(pdf, point["x"], point["y"], page)
        except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
            entry["error"] = f"raw SyncTeX edit failed: {error}"
            yield entry
            continue
        if not raw_records:
            entry["error"] = "raw SyncTeX edit returned no match"
            yield entry
            continue
        candidates = [candidate_anchor(page, item) for item in raw_records]
        raw, expected_file, expected_anchor, bounds, remapped, generated = candidates[0]
        entry["raw_anchor"] = {"line": raw[0], "file": raw[1]}
        if not batch.get("ok"):
            error = batch.get("error", "batch returned no result")
            if "no text near point" in error or "hit-test failed" in error:
                entry.update(abstained=True, reason="pdfium_hit_unsupported", error=error)
            elif not best_guess and "ambiguous inverse search:" in error and len(candidates) > 1:
                entry.update(abstained=True, reason="ambiguous_inverse_anchors", error=error)
            else:
                entry["error"] = error
            yield entry
            continue
        location = batch.get("location")
        if isinstance(location, dict) and location.get("precise") is True and len(candidates) > 1:
            resolved_file = Path(location["file"]).resolve()
            resolved_line = int(location["line"])
            matching = [
                cand
                for cand in candidates
                if cand[1] == resolved_file
                and (
                    (cand[3] and cand[3][0] + 1 <= resolved_line <= cand[3][1] + 1)
                    or abs(resolved_line - cand[2]) <= 4
                )
            ]
            if matching:
                raw, expected_file, expected_anchor, bounds, remapped, generated = min(
                    matching, key=lambda cand: abs(resolved_line - cand[2])
                )
                entry["raw_anchor"] = {"line": raw[0], "file": raw[1]}
        entry["expected_anchor"] = {"file": str(expected_file), "line": expected_anchor, "remapped": remapped}
        if not (generated and not remapped) and not 1 <= expected_anchor <= len(source_info(expected_file)[1]):
            entry["error"] = "page-proven source anchor is outside its source file"
            yield entry
            continue
        if not isinstance(location, dict):
            entry["error"] = "successful batch has no source location"
            yield entry
            continue
        resolved_file = Path(location["file"]).resolve()
        resolved_line = int(location["line"])
        precise = location["precise"]
        warning = batch.get("warning") or ""
        if best_guess:
            for baseline in (
                "best-guess navigation: compiled source history unavailable; assuming saved source is unchanged since compilation",
                "best-guess navigation: no literal source position was qualified; using the compiler line anchor",
            ):
                warning = warning.replace(baseline, "")
            warning = warning.strip("; ")
        if precise is False:
            if resolved_file != expected_file or resolved_line != expected_anchor:
                entry["error"] = "coarse inverse result changed the source file or page-proven anchor"
                yield entry
                continue
            entry["mapping_ok"] = True
            entry.update(abstained=True, reason="resolver_warning" if warning else "coarse_source_refinement")
            yield entry
            continue
        if generated and not remapped:
            entry["error"] = "precise fragile result has no original frame provenance"
            yield entry
            continue
        if resolved_file not in known_inputs:
            entry["error"] = "inverse result selected a file absent from the source map"
            yield entry
            continue
        _, resolved_lines, hidden, starts, meta_spans = source_info(resolved_file)
        if not 1 <= resolved_line <= len(resolved_lines):
            entry["error"] = "SyncTeX source line is invalid"
            yield entry
            continue
        resolved_row = resolved_lines[resolved_line - 1]
        byte = int(location["byte_column"])
        try:
            if byte < 0 or byte > len(resolved_row.encode("utf-8")):
                raise ValueError("source byte column is outside its line")
            prefix = resolved_row.encode("utf-8")[:byte].decode("utf-8")
            if location["column"] != len(prefix.encode("utf-16-le")) // 2 + 1 or location["column_char"] != len(prefix) + 1:
                raise ValueError("source Unicode columns disagree")
        except (UnicodeError, ValueError) as error:
            entry["error"] = str(error)
            yield entry
            continue
        absolute_column = starts[resolved_line - 1] + len(prefix)
        if any(start <= absolute_column < end for start, end in hidden):
            entry["error"] = "precise inverse result selected a known nonprinting argument or definition"
            yield entry
            continue
        pdf_word = batch.get("pdf_word")
        entry["pdfium_word"] = pdf_word
        selected_word = pdf_word or word
        metadata = (
            any(start <= absolute_column < end for start, end in meta_spans)
            and line_has_word(resolved_row, selected_word)
        )
        if resolved_file != expected_file and not metadata:
            entry["error"] = "inverse result changed the page-proven source file"
            yield entry
            continue
        if bounds and not metadata and not bounds[0] + 1 <= resolved_line <= bounds[1] + 1:
            entry["error"] = "inverse result escaped the page-proven original frame"
            yield entry
            continue
        entry["mapping_ok"] = True
        if warning:
            entry.update(abstained=True, reason="resolver_warning")
        elif not pdf_word:
            entry.update(abstained=True, reason="pdfium_selected_no_word")
        elif normalized_words(pdf_word) != normalized_words(word):
            entry.update(abstained=True, reason="pdfium_word_mismatch")
        elif line_has_word(resolved_row, pdf_word):
            literal = []
            for char in resolved_row[len(prefix):]:
                if char.isalnum() or literal and unicodedata.combining(char):
                    literal.append(char)
                else:
                    break
            enclosing = []
            for char in reversed(prefix):
                if char.isalnum() or unicodedata.combining(char):
                    enclosing.append(char)
                else:
                    break
            enclosing = list(reversed(enclosing)) + literal
            alpha_word = len(pdf_word) >= 2 and pdf_word.isascii() and pdf_word.isalpha()
            if normalized_words("".join(literal)) != normalized_words(pdf_word) and (
                alpha_word or normalized_words("".join(enclosing)) != normalized_words(pdf_word)
            ):
                entry["error"] = "precise inverse column does not select the PDF word"
            else:
                entry.update(ok=True, match="document_metadata" if metadata else "literal_frame_word" if bounds else "literal_source")
        else:
            expected_lines = source_info(expected_file)[1]
            eligible = expected_lines[bounds[0]:bounds[1] + 1] if bounds else expected_lines
            selected_token = re.match(
                r"\\[A-Za-z@]+\*?(?:\s*[_^]\s*(?:\{[^{}]*\}|\\[A-Za-z@]+\*?|[A-Za-z0-9]))*|[A-Za-z0-9]+",
                resolved_row[len(prefix):],
            )
            anchor_row = (
                source_line_text(expected_lines[expected_anchor - 1])
                if 1 <= expected_anchor <= len(expected_lines)
                else ""
            )
            if any(line_has_word(row, pdf_word) for row in eligible):
                entry["error"] = "inverse refinement missed a literal printed word in its original source scope"
            elif (
                not bounds
                and resolved_line != expected_anchor
                and selected_token
                and not re.match(r"\s*\\(?:end\b|caption\b)", anchor_row)
                and selected_token.group(0) in anchor_row
            ):
                entry["error"] = "inverse refinement jumped away from raw anchor line containing the same math token"
            else:
                entry.update(abstained=True, reason="precision_not_literal_math_or_expansion")
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
    if PDFTERM_BINARY.is_file():
        return PDFTERM_BINARY
    raise SystemExit(f"missing {PDFTERM_BINARY}; build pdfterm or pass --binary PATH")


def main() -> None:
    global PDFTERM_BINARY
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pdf", type=Path)
    parser.add_argument("--binary", type=Path, default=PDFTERM_BINARY, help="pdfterm executable to probe")
    parser.add_argument(
        "--source",
        type=Path,
        help="source file to sample (default: main input recorded in SyncTeX)",
    )
    parser.add_argument("--page", type=int, action="append", help="probe this one-based page; repeatable")
    parser.add_argument("--lines", type=int, default=100, help="source lines to sample")
    parser.add_argument("--pages", type=int, help="seed-selected inverse page cap; default probes all pages")
    parser.add_argument("--words", type=int, default=0, help="max words per selected page; 0 means all")
    parser.add_argument("--seed", type=int, default=20260922)
    parser.add_argument("--allow-stale", action="store_true", help="accept an older PDF/SyncTeX pair")
    parser.add_argument("--best-guess", action="store_true", help="pass --synctex-best-guess to pdfterm")
    args = parser.parse_args()
    PDFTERM_BINARY = args.binary.resolve()
    if (
        min(args.lines, args.words) < 0
        or (args.pages is not None and args.pages < 0)
        or (args.page is not None and (args.pages is not None or min(args.page) < 1 or len(set(args.page)) != len(args.page)))
    ):
        parser.error("counts must be nonnegative; --page values must be unique and cannot combine with --pages")

    pdf = args.pdf.resolve()
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
    source_file = (args.source or read_source_map(pdf)["inputs"][1]).resolve()
    if not source_file.is_file():
        raise SystemExit(f"missing source {source_file}; pass --source PATH")
    if (
        not args.allow_stale
        and min(pdf.stat().st_mtime_ns, companion.stat().st_mtime_ns)
        < source_file.stat().st_mtime_ns
    ):
        raise SystemExit(
            "PDF/SyncTeX pair predates the TeX source; rebuild or pass --allow-stale"
        )
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

    results = list(forward(pdf, source_file, source, forward_positions, page_count, visible_words, args.best_guess))
    batch_error = None
    try:
        results.extend(inverse(pdf, source_file, word_points, args.best_guess))
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
        "mapping_checked": sum(bool(probe.get("mapping_ok")) for probe in inverse_results),
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
