#!/usr/bin/env python3
"""Fuzz SyncTeX navigation in both directions for a PDF and report mismatches.

Forward: sample source lines of the companion .tex, resolve with the pdfterm
CLI, and compare the resolved page against the raw `synctex view` mapping. A
line inside a literal Beamer frame must map to the frame's closing-line pages
exactly; any other line must match its own raw mapping. Inverse: sample points
on sampled pages, run `synctex edit`, and require a match whose line lies
inside the enclosing frame when the raw result is a frame closing line.
Output is JSON, one object per probe. Breaks (nonzero exit) after --max-fails.

Usage: synctex_fuzz.py PDF [--lines N] [--points N] [--pages N] [--max-fails N]
"""

import argparse
import gzip
import json
import random
import re
import subprocess
import sys
from pathlib import Path

FRAME_BEGIN = re.compile(r"\\begin\s*\{frame\}")


def tex_path(pdf: Path) -> Path:
    return pdf.with_suffix(".tex")


def raw_pages(pdf: Path, line: int) -> set[int]:
    stdout = subprocess.run(
        ["synctex", "view", "-i", f"{line}:1:{tex_path(pdf)}", "-o", str(pdf)],
        capture_output=True,
        text=True,
        timeout=30,
    ).stdout
    return set(int(p) for p in re.findall(r"^Page:(\d+)$", stdout, re.M))


def raw_inverse(pdf: Path, x: float, y: float, page: int) -> tuple[int, str] | None:
    stdout = subprocess.run(
        ["synctex", "edit", "-o", f"{page}:{x:.2f}:{y:.2f}:{pdf}"],
        capture_output=True,
        text=True,
        timeout=30,
    ).stdout
    match = re.search(r"^Input:(.*)$\n^Line:(\d+)", stdout, re.M)
    return (int(match.group(2)), match.group(1)) if match else None


def frame_bounds(source: str, line: int) -> tuple[int, int] | None:
    start = None
    for index, text in enumerate(source.splitlines(), 1):
        stripped = text.lstrip()
        if FRAME_BEGIN.match(stripped):
            start = index
        elif stripped.startswith("\\end{frame}"):
            if start is not None and start <= line <= index:
                return (start, index)
            start = None
    return None


def word_column(source: str, line: int) -> int:
    row = source.splitlines()[line - 1] if line <= len(source.splitlines()) else ""
    match = re.search(r"[A-Za-z]{2,}", row)
    return match.start() + 1 if match else 1


def forward(pdf: Path, lines: list[int], source: str) -> list[dict]:
    results = []
    for line in lines:
        inside = frame_bounds(source, line)
        # A frame-interior line must resolve to the closing line's pages only;
        # that is the Beamer refinement this project implements.
        expected = raw_pages(pdf, inside[1]) if inside else raw_pages(pdf, line)
        column = word_column(source, line)
        stdout = subprocess.run(
            [
                str(pdfterm_bin()),
                str(pdf),
                "--synctex-view",
                str(tex_path(pdf)),
                "--line",
                str(line),
                "--column",
                str(column),
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if stdout.returncode != 0:
            results.append(
                {
                    "direction": "forward",
                    "line": line,
                    "ok": False,
                    "error": stdout.stderr.strip()[:200],
                }
            )
            continue
        got = json.loads(stdout.stdout)
        results.append(
            {
                "direction": "forward",
                "line": line,
                "ok": got["page"] in expected,
                "resolved_page": got["page"],
                "raw_pages": sorted(expected),
            }
        )
    return results


def page_frames(pdf: Path, source: str, pages: list[int]) -> dict[int, list[tuple[int, int]]]:
    """Frame ranges whose closing line maps to each sampled page."""
    owners: dict[int, list[tuple[int, int]]] = {page: [] for page in pages}
    closing: list[int] = []
    lines = source.splitlines()
    start = None
    for index, text in enumerate(lines, 1):
        if FRAME_BEGIN.match(text.lstrip()):
            start = index
        elif text.lstrip().startswith("\\end{frame}"):
            if start is not None:
                closing.append(index)
            start = None
    for line in closing:
        inside = frame_bounds(source, line)
        if inside is None:
            continue
        for page in raw_pages(pdf, line):
            if page in owners:
                owners[page].append(inside)
    return owners


def inverse(
    pdf: Path, pages: list[int], n: int, source: str, owners: dict[int, list[tuple[int, int]]]
) -> list[dict]:
    results = []
    info = pdfinfo(pdf)
    for page in pages:
        width, height = info[page]
        for point in sample_points(width, height, n):
            raw = raw_inverse(pdf, point[0], point[1], page)
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
            elif raw[1].endswith(".tex"):
                frames = owners.get(page) or []
                if frames:
                    # The raw line must belong to a frame that owns this page:
                    # a click on a page whose frames span 4708..4730 mapping to
                    # the preceding frame's closing line 4703 is the bug.
                    entry["ok"] = any(
                        begin <= raw[0] <= end + 1 for begin, end in frames
                    )
                    entry["frames"] = [list(f) for f in frames]
            results.append(entry)
    return results


def sample_points(width: float, height: float, n: int) -> list[tuple[float, float]]:
    columns = (0.1, 0.35, 0.6, 0.85)
    rows = (0.1, 0.3, 0.5, 0.7, 0.9)
    points = [(width * fx, height * fy) for fy in rows for fx in columns]
    return points[:n]


def pdfinfo(pdf: Path) -> dict[int, tuple[float, float]]:
    sizes = {}
    total = None
    output = subprocess.run(
        ["pdfinfo", "-l", "1000000", str(pdf)], capture_output=True, text=True
    ).stdout
    for row in output.splitlines():
        if match := re.match(r"Page\s+(\d+) size:\s+([\d.]+) x ([\d.]+)", row):
            sizes[int(match.group(1))] = (float(match.group(2)), float(match.group(3)))
        elif total := re.match(r"Pages:\s+(\d+)", row):
            total = int(total.group(1))
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
    parser.add_argument("--lines", type=int, default=40, help="forward source lines to sample")
    parser.add_argument("--points", type=int, default=20, help="inverse points per sampled page")
    parser.add_argument("--pages", type=int, default=12, help="inverse pages to sample")
    parser.add_argument("--max-fails", type=int, default=10)
    parser.add_argument("--seed", type=int, default=20260922)
    args = parser.parse_args()

    pdf = args.pdf.resolve()
    tex = tex_path(pdf)
    if not tex.exists():
        raise SystemExit(f"missing companion {tex}")
    companion = pdf.with_suffix(".synctex.gz")
    if companion.exists():
        gzip.decompress(companion.read_bytes())
    source = tex.read_text()

    rng = random.Random(args.seed)
    text_lines = [
        i + 1
        for i, row in enumerate(source.splitlines())
        if row.strip() and not row.lstrip().startswith("%")
    ]
    forward_lines = sorted(rng.sample(text_lines, min(args.lines, len(text_lines))))
    pages = sorted(rng.sample(list(pdfinfo(pdf)), min(args.pages, len(pdfinfo(pdf)))))

    results = forward(pdf, forward_lines, source)
    owners = page_frames(pdf, source, pages)
    results += inverse(pdf, pages, args.points, source, owners)
    fails = [p for p in results if not p["ok"]]
    stopped = len(fails) >= args.max_fails
    if stopped:
        shown = results[: [i for i, p in enumerate(results) if not p["ok"]][args.max_fails - 1] + 1]
    else:
        shown = results
    print(
        json.dumps(
            {
                "pdf": str(pdf),
                "probes": len(results),
                "fails": len(fails),
                "stopped_early": stopped,
                "results": shown,
            },
            indent=2,
        )
    )
    sys.exit(1 if stopped else 0)


if __name__ == "__main__":
    main()
