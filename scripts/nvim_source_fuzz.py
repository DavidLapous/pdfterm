#!/usr/bin/env python3
"""Fuzz compiled/live source projection in real Neovim, with a separate edit oracle.

The oracle uses UTF-8 byte intervals transformed by recorded edits, never Neovim
extmarks, vim.diff, or mapper results. Every state probes all scalar boundaries,
line ends, and interior UTF-8 bytes. This is source-history coverage, not a PDF
printed-occurrence oracle. See README.md for invocation and scope.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


class InvalidTrace(ValueError):
    pass


def boundaries(text: bytes) -> list[int]:
    return [i for i in range(len(text) + 1) if i == len(text) or text[i] & 0xC0 != 0x80]


class History:
    """Occurrence intervals plus literal bytes; undo restores previous history."""

    def __init__(self, lines: list[str]):
        self.original = [line.encode('utf-8') for line in lines]
        self.text = b'\n'.join(self.original)
        offset = 0
        self.spans = []
        for line in self.original:
            self.spans.append([offset, offset + len(line), bool(line)])
            offset += len(line) + 1
        self.undo = []
        self.redo = []
        self.renamed = False
        self.disk = self.text + b'\n'
        self.live_disk = self.disk
        self.saved_text = self.text
        self._lines_cache = None
        self._precise_spans_cache = None

    def _invalidate_caches(self):
        self._lines_cache = None
        self._precise_spans_cache = None

    def lines(self) -> list[bytes]:
        if self._lines_cache is None:
            self._lines_cache = self.text.split(b'\n')
        return self._lines_cache

    def point(self, offset: int) -> tuple[int, int]:
        prefix = self.text[:offset]
        row = prefix.count(b'\n')
        return row, len(prefix.rsplit(b'\n', 1)[-1])

    def offset(self, point: list[int]) -> int:
        row, byte = point
        lines = self.lines()
        if not 0 <= row < len(lines) or byte not in boundaries(lines[row]):
            raise InvalidTrace(f'invalid edit coordinate {point}')
        return sum(len(line) + 1 for line in lines[:row]) + byte

    def snapshot(self):
        return self.text, copy.deepcopy(self.spans)

    def edit(self, start: int, end: int, replacement: bytes):
        if not 0 <= start <= end <= len(self.text):
            raise InvalidTrace('invalid edit range')
        if self.text[start:end] == replacement:
            raise InvalidTrace('no-op edit does not define an undo transaction')
        self.undo.append(self.snapshot())
        self.redo.clear()
        delta = len(replacement) - (end - start)

        def move(position, right):
            if position < start:
                return position
            if position > end or position == end and end > start:
                return position + delta
            return start + (len(replacement) if right else 0)

        for span in self.spans:
            left, right, alive = span
            if end > start and start <= left and end >= right:
                alive = False
            span[:] = [move(left, True), move(right, False), alive]
        self.text = self.text[:start] + replacement + self.text[end:]
        self._invalidate_caches()

    def apply(self, operation: dict):
        kind = operation['kind']
        if kind == 'text':
            self.edit(self.offset(operation['start']), self.offset(operation['end']),
                      '\n'.join(operation['replacement']).encode('utf-8'))
        elif kind == 'lines':
            lines = self.lines()
            start, end = operation['start'], operation['end']
            if not 0 <= start <= end <= len(lines):
                raise InvalidTrace('invalid line range')
            replacement = [line.encode('utf-8') for line in operation['replacement']]
            result = lines[:start] + replacement + lines[end:]
            if not result:
                result = [b'']
            begin = sum(len(line) + 1 for line in lines[:start])
            finish = sum(len(line) + 1 for line in lines[:end])
            insert = b'\n'.join(replacement)
            if end < len(lines):
                insert += b'\n' if replacement else b''
            elif start == len(lines):
                begin = finish = len(self.text)
                insert = b'\n' + insert
            elif start > 0:
                begin -= 1
                insert = b'\n' + insert if replacement else b''
            self.edit(min(begin, len(self.text)), min(finish, len(self.text)), insert)
            if self.text != b'\n'.join(result):
                raise InvalidTrace('line edit model did not produce requested buffer')
        elif kind == 'undo':
            if not self.undo:
                raise InvalidTrace('undo without history')
            self.redo.append(self.snapshot())
            self.text, self.spans = self.undo.pop()
            self._invalidate_caches()
        elif kind == 'redo':
            if not self.redo:
                raise InvalidTrace('redo without history')
            self.undo.append(self.snapshot())
            self.text, self.spans = self.redo.pop()
            self._invalidate_caches()
        elif kind in ('save', 'reload'):
            self.live_disk = self.text + b'\n'
            self.saved_text = self.text
            if not self.renamed:
                self.disk = self.live_disk
            if kind == 'reload':
                # Checkpointed source spans survive. Buffer undo history does not.
                self.undo.clear()
                self.redo.clear()
        elif kind == 'rename':
            if self.renamed:
                raise InvalidTrace('duplicate rename')
            self.renamed, self.live_disk = True, None
        else:
            raise InvalidTrace(f'unknown operation {kind}')

    def precise_spans(self):
        if self._precise_spans_cache is None:
            spans = []
            for original_row, (left, right, alive) in enumerate(self.spans):
                if (alive and left <= right and self.text[left:right] == self.original[original_row]
                        and b'\n' not in self.text[left:right]):
                    row, byte = self.point(left)
                    spans.append((original_row, left, right, row, byte))
            self._precise_spans_cache = spans
        return self._precise_spans_cache

    def queries(self) -> dict:
        # Include invalid interior bytes: they must clamp with a warning, not
        # silently certify a byte inside a Unicode scalar.
        delivery_line = min(2, len(self.original))
        delivery_text = self.original[delivery_line - 1]
        delivery_byte = max(byte for byte in boundaries(delivery_text)
                            if byte <= max(0, min(7, len(delivery_text) - 1)))
        return {
            'forward': [[row, byte] for row, line in enumerate(self.lines())
                        for byte in range(len(line) + 1)],
            'inverse': [[row + 1, byte] for row, line in enumerate(self.original)
                        for byte in range(len(line) + 1)],
            'delivery': [delivery_line, delivery_byte],
        }

    def expected_forward(self, row: int, byte: int):
        line = self.lines()[row]
        if byte not in boundaries(line):
            return None
        offset = self.offset([row, byte])
        for original_row, left, right, _, _ in self.precise_spans():
            if left <= offset < right:
                original_byte = offset - left
                return original_row + 1, len(self.original[original_row][:original_byte].decode('utf-8')) + 1
        return None

    def expected_inverse(self, line: int, byte: int):
        original = self.original[line - 1]
        if byte == len(original) or byte not in boundaries(original):
            return None
        for original_row, _, _, row, start_byte in self.precise_spans():
            if original_row == line - 1:
                return row + 1, start_byte + byte
        return None


def trace_states(trace: dict):
    model = History(trace['initial'])
    models = [copy.deepcopy(model)]
    for operation in trace['operations']:
        model.apply(operation)
        models.append(copy.deepcopy(model))
    return models


def generate(seed: int, index: int, steps: int) -> dict:
    rng = random.Random((seed << 16) + index)
    initial = [f'Header alpha {index}', 'Copper needle Silver', 'Repeat target',
               'Repeat target', 'Aé𝑠é 終点', '', 'Last target']
    trace = {'version': 1, 'seed': seed, 'case': index, 'initial': initial, 'operations': []}
    model = History(initial)
    kinds = ['prefix', 'suffix', 'inside', 'insert', 'duplicate', 'replace',
             'delete', 'split', 'join', 'undo', 'redo', 'save', 'reload', 'rename']
    pending_rename = False
    for step in range(steps):
        kind = 'rename' if pending_rename else (
            kinds[(step + index) % len(kinds)] if step < len(kinds) else rng.choice(kinds))
        pending_rename = False
        # Exercise both clean rename and rename of genuinely unsaved text.
        if kind == 'rename' and not model.renamed and index % 2 and model.text == model.saved_text:
            kind, pending_rename = 'prefix', True
        lines = model.lines()
        row = rng.randrange(len(lines))
        limits = boundaries(lines[row])
        payload = rng.choice(['λ ', 'é𝑠', 'new ', 'Repeat target', ''])
        if kind in ('prefix', 'suffix'):
            byte = 0 if kind == 'prefix' else len(lines[row])
            operation = {'kind': 'text', 'start': [row, byte], 'end': [row, byte],
                         'replacement': [payload or '++']}
        elif kind == 'inside' and len(limits) > 2:
            start = rng.choice(limits[1:-1])
            end = rng.choice([position for position in limits if position > start])
            operation = {'kind': 'text', 'start': [row, start], 'end': [row, end],
                         'replacement': [payload]}
        elif kind in ('insert', 'duplicate'):
            operation = {'kind': 'lines', 'start': row, 'end': row,
                         'replacement': [lines[row].decode() if kind == 'duplicate' else payload]}
        elif kind == 'replace':
            operation = {'kind': 'lines', 'start': row, 'end': row + 1,
                         'replacement': [payload]}
        elif kind == 'delete' and len(lines) > 1:
            operation = {'kind': 'lines', 'start': row, 'end': row + 1, 'replacement': []}
        elif kind == 'split':
            byte = rng.choice(limits)
            operation = {'kind': 'text', 'start': [row, byte], 'end': [row, byte],
                         'replacement': ['', '']}
        elif kind == 'join' and row < len(lines) - 1:
            operation = {'kind': 'text', 'start': [row, len(lines[row])],
                         'end': [row + 1, 0], 'replacement': ['']}
        elif kind == 'undo' and model.undo:
            operation = {'kind': 'undo'}
        elif kind == 'redo' and model.redo:
            operation = {'kind': 'redo'}
        elif kind in ('save', 'reload'):
            operation = {'kind': kind}
        elif kind == 'rename' and not model.renamed:
            operation = {'kind': 'rename'}
        else:
            kind = 'prefix'
            operation = {'kind': 'text', 'start': [row, 0], 'end': [row, 0], 'replacement': ['+']}
        try:
            model.apply(operation)
        except InvalidTrace:
            kind = 'prefix'
            operation = {'kind': 'text', 'start': [row, 0], 'end': [row, 0], 'replacement': ['+']}
            model.apply(operation)
        operation['category'] = kind
        trace['operations'].append(operation)
    return trace


def failure(kind: str, state: int, query, expected, actual):
    return {'kind': kind, 'state': state, 'query': query, 'expected': expected, 'actual': actual}


def failure_identity(problem):
    """Do not shrink false precision into lost precision or another owner."""
    owner = claimed = None
    if problem['kind'] in ('false_precision', 'lost_precision', 'wrong_precise_target'):
        direction, point = problem['query']
        owner = problem['expected'] if direction == 'forward' else point
        if direction == 'forward':
            claimed = (problem['actual'].get('line'), problem['actual'].get('column'))
    return problem['kind'], problem['query'], owner, claimed


def check(trace: dict, models: list[History], actual: dict):
    if actual.get('runtime_error'):
        return failure('worker_error', len(actual.get('states', [])), None, 'successful worker', actual['runtime_error'])
    states = actual.get('states', [])
    if len(states) != len(models):
        return failure('missing_states', 0, None, len(models), len(states))
    document = actual['document']
    original_file = document['source_file']
    renamed_file = str(Path(original_file).with_name('renamed.tex'))
    for state_index, (model, observed) in enumerate(zip(models, states)):
        if observed.get('complete') is not True:
            return failure('incomplete_state', state_index, None, True, observed.get('complete'))
        expected_lines = [line.decode() for line in model.lines()]
        if observed['lines'] != expected_lines:
            return failure('buffer_history', state_index, None, expected_lines, observed['lines'])
        for key in ('guard_preserved', 'snapshot_preserved', 'buffer_preserved', 'disk_preserved'):
            if observed.get(key) is not True:
                return failure(key, state_index, None, True, observed.get(key))
        if observed.get('clipboard_writes') != 0:
            return failure('clipboard_write', state_index, None, 0, observed.get('clipboard_writes'))
        for key, expected in [('disk', model.disk), ('live_disk', model.live_disk)]:
            expected = expected.decode() if expected is not None else None
            if observed.get(key) != expected:
                return failure('saved_bytes', state_index, key, expected, observed.get(key))
        live_file = renamed_file if model.renamed else original_file
        queries = model.queries()
        for direction in ('forward', 'inverse'):
            results = observed.get(direction, [])
            if len(results) != len(queries[direction]):
                return failure('missing_queries', state_index, direction, len(queries[direction]), len(results))
            for point, result in zip(queries[direction], results):
                expected = getattr(model, 'expected_' + direction)(*point)
                precise = result.get('coarse') is False if direction == 'forward' else not result.get('warning')
                if result.get('file') != (original_file if direction == 'forward' else live_file):
                    return failure('source_identity', state_index, [direction, point],
                                   original_file if direction == 'forward' else live_file, result)
                actual_point = (result.get('line'), result.get('column' if direction == 'forward' else 'byte_column'))
                if precise != (expected is not None):
                    return failure('false_precision' if precise else 'lost_precision',
                                   state_index, [direction, point], expected, result)
                if expected is not None and actual_point != expected:
                    return failure('wrong_precise_target', state_index, [direction, point], expected, result)
                if not precise and not result.get('warning'):
                    return failure('missing_warning', state_index, [direction, point], 'explicit warning', result)
                target_lines = model.original if direction == 'forward' else model.lines()
                line = result.get('line', 0)
                if not isinstance(line, int) or not 1 <= line <= len(target_lines):
                    return failure('target_bounds', state_index, [direction, point], 'valid line', result)
                text = target_lines[line - 1]
                column = actual_point[1]
                valid = (isinstance(column, int) and (1 <= column <= len(text.decode()) + 1
                         if direction == 'forward' else column in boundaries(text)))
                if not valid:
                    return failure('target_bounds', state_index, [direction, point], 'UTF-8/scalar boundary', result)
        delivered = observed['delivery']
        expected = model.expected_inverse(*queries['delivery'])
        if delivered.get('file') != live_file:
            return failure('delivery_identity', state_index, queries['delivery'], live_file, delivered)
        if expected is not None and (delivered.get('line'), delivered.get('byte_column')) != expected:
            return failure('delivery_projection', state_index, queries['delivery'], expected, delivered)
        if expected is None and not delivered.get('warnings'):
            return failure('delivery_warning', state_index, queries['delivery'], 'coarse warning', delivered)
        if model.text != model.saved_text and not observed.get('modified'):
            return failure('dirty_buffer_saved', state_index, None, True, observed.get('modified'))
    return None


def write_json(path: Path, value):
    temporary = path.with_name(path.name + '.tmp')
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')
    temporary.replace(path)


def run_case(trace, label, output, binary, nvim, timeout):
    models = trace_states(trace)
    if any((output / f'{label}-{suffix}.json').exists() for suffix in ('input', 'actual', 'execution')):
        raise InvalidTrace('case artifacts already exist; choose a new output directory')
    directory = Path(tempfile.mkdtemp(prefix='nf-', dir=ROOT / 'target'))
    input_path, result_path = output / f'{label}-input.json', output / f'{label}-actual.json'
    request = {'directory': str(directory), 'binary': str(binary), 'initial': trace['initial'],
               'operations': trace['operations'], 'states': [model.queries() for model in models]}
    write_json(input_path, request)
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(('KITTY_', 'WEZTERM_', 'GHOSTTY_', 'PDFTERM_')) or key in (
                'SSH_CONNECTION', 'SSH_TTY', 'TERM_PROGRAM', 'DISPLAY', 'WAYLAND_DISPLAY', 'NVIM'):
            env.pop(key, None)
    for key, suffix in [('HOME', 'home'), ('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache'),
                        ('XDG_STATE_HOME', 'state'), ('XDG_DATA_HOME', 'data'), ('TMPDIR', 'tmp')]:
        path = directory / suffix
        path.mkdir(mode=0o700)
        env[key] = str(path)
    command = [nvim, '--headless', '-u', 'NONE', '-i', 'NONE', '-n', '-l',
               str(ROOT / 'scripts/nvim_source_fuzz.lua'), str(input_path), str(result_path)]
    started = time.monotonic()
    process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
        try:
            if result_path.exists() and result_path.stat().st_size > 64 * 1024 * 1024:
                raise ValueError('worker result exceeds 64 MiB')
            actual = json.loads(result_path.read_text()) if result_path.exists() else {'runtime_error': 'worker wrote no result'}
            if not isinstance(actual, dict):
                raise TypeError('worker result must be an object')
            if not actual.get('runtime_error') and actual.get('document', {}).get('source_file') != str(directory / 'main.tex'):
                raise ValueError('worker result belongs to a different source fixture')
            problem = check(trace, models, actual)
        except (OSError, ValueError, KeyError, TypeError, IndexError, AttributeError) as error:
            diagnostic = f'{type(error).__name__}: {error}'
            actual = {'runtime_error': diagnostic}
            problem = failure('worker_result_invalid', 0, None, 'valid worker observations', diagnostic)
        if process.returncode and problem is None:
            problem = failure('worker_exit', 0, None, 0, process.returncode)
        execution = {'command': command, 'elapsed_seconds': time.monotonic() - started,
                     'returncode': process.returncode, 'stdout': stdout, 'stderr': stderr}
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
        actual, problem = {}, failure('worker_timeout', 0, None, f'completed in {timeout}s', None)
        execution = {'command': command, 'elapsed_seconds': time.monotonic() - started, 'timed_out': True}
    write_json(output / f'{label}-execution.json', execution)
    if problem is None:
        shutil.rmtree(directory)
    else:
        problem['retained_runtime'] = str(directory)
    return problem, actual, execution, models


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seed', type=int, default=20261011)
    parser.add_argument('--cases', type=int, default=1_000_000)
    parser.add_argument('--steps', type=int, default=32)
    parser.add_argument('--binary', type=Path, default=Path(shutil.which('pdfterm') or ROOT / 'target/release/pdfterm'))
    parser.add_argument('--nvim', default='nvim')
    parser.add_argument('--timeout', type=float, default=30)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--replay', type=Path, help='replay retained trace.json or minimized.json')
    parser.add_argument('--minimize', type=int, default=16, help='maximum shrink attempts after a semantic failure')
    parser.add_argument('--keep-passing', action='store_true', help='retain full artifacts for passing cases')
    args = parser.parse_args()
    if not 0 <= args.seed < 2**32 or not 1 <= args.cases <= 1_000_000 or not 1 <= args.steps <= 2000:
        parser.error('seed must be uint32; cases 1..1000000; steps 1..2000')
    if not 0 < args.timeout <= 300 or not 0 <= args.minimize <= 100:
        parser.error('timeout must be (0,300]; minimize 0..100')
    binary = args.binary.resolve()
    nvim = shutil.which(args.nvim)
    if not binary.is_file() or not os.access(binary, os.X_OK) or not nvim:
        parser.error('executable pdfterm and Neovim are required')
    nvim = str(Path(nvim).resolve())
    target = ROOT / 'target'
    target.mkdir(exist_ok=True)
    output = args.output.resolve() if args.output else Path(tempfile.mkdtemp(prefix='nvim-fuzz-', dir=target))
    if not output.is_relative_to(target):
        parser.error('output must be inside this repository target directory')
    output.mkdir(parents=True, exist_ok=True)
    if (output / 'summary.json').exists() or any(output.glob('case-*.json')):
        parser.error('output already contains campaign artifacts; choose a new directory')
    replay = None
    if args.replay:
        try:
            if args.replay.stat().st_size > 8 * 1024 * 1024:
                raise InvalidTrace('replay exceeds 8 MiB')
            replay = json.loads(args.replay.read_text())
            if not isinstance(replay, dict) or replay.get('version') != 1:
                raise InvalidTrace('expected a version-1 trace object')
            if (not isinstance(replay.get('initial'), list) or not replay['initial']
                    or not isinstance(replay.get('operations'), list) or len(replay['operations']) > 2000):
                raise InvalidTrace('expected nonempty initial lines and at most 2000 operations')
            if any(not isinstance(line, str) or '\n' in line or '\0' in line for line in replay['initial']):
                raise InvalidTrace('initial values must be UTF-8 text lines')
            model = History(replay['initial'])
            if len(model.text) > 2 * 1024 * 1024:
                raise InvalidTrace('captured source exceeds 2 MiB')
            for operation in replay['operations']:
                model.apply(operation)
        except (OSError, ValueError, KeyError, TypeError, IndexError) as error:
            parser.error(f'invalid replay: {error}')
    traces = [replay] if replay is not None else (generate(args.seed, index, args.steps) for index in range(args.cases))
    summary = {'seed': replay.get('seed') if replay is not None else args.seed,
               'replay': str(args.replay.resolve()) if args.replay else None,
               'binary': str(binary), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
               'nvim': nvim, 'cases_requested': 1 if replay is not None else args.cases, 'cases_completed': 0, 'states': 0,
               'forward_queries': 0, 'inverse_queries': 0, 'socket_deliveries': 0, 'operations': {},
               'dirty_renames': 0, 'clean_renames': 0,
               'keep_passing_artifacts': args.keep_passing,
               'driver_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
               'worker_sha256': hashlib.sha256((ROOT / 'scripts/nvim_source_fuzz.lua').read_bytes()).hexdigest(),
               'coverage': 'all UTF-8 byte positions and line ends at every generated state',
               'scope': 'real source mapper and plugin inverse listener; metadata PDF fixture, not rendered-text ownership',
               'failure': None, 'shrink_attempts': 0}
    started = time.monotonic()
    for index, trace in enumerate(traces):
        label = f'case-{index:04d}'
        write_json(output / f'{label}-trace.json', trace)
        problem, actual, _, models = run_case(trace, label, output, binary, nvim, args.timeout)
        completed_states = [state for state in actual.get('states', []) if state.get('complete') is True]
        summary['states'] += len(completed_states)
        for state in actual.get('states', []):
            for key in ('forward', 'inverse'):
                summary[key + '_queries'] += len(state.get(key, []))
            summary['socket_deliveries'] += int('delivery' in state)
        completed_operations = max(0, len(completed_states) - 1)
        for operation, model in zip(trace['operations'][:completed_operations], models):
            category = operation.get('category', operation['kind'])
            summary['operations'][category] = summary['operations'].get(category, 0) + 1
            if operation['kind'] == 'rename':
                summary['dirty_renames' if model.text != model.saved_text else 'clean_renames'] += 1
        if not actual.get('runtime_error') and len(completed_states) == len(trace['operations']) + 1:
            summary['cases_completed'] += 1
        if problem is not None:
            summary['failure'] = problem
            write_json(output / 'repro.json', trace)
            reduced = copy.deepcopy(trace)
            if not problem['kind'].startswith('worker_'):
                width = max(1, len(reduced['operations']) // 2)
                while width and summary['shrink_attempts'] < args.minimize:
                    changed = False
                    for begin in range(0, len(reduced['operations']), width):
                        if summary['shrink_attempts'] >= args.minimize:
                            break
                        candidate = copy.deepcopy(reduced)
                        del candidate['operations'][begin:begin + width]
                        summary['shrink_attempts'] += 1
                        try:
                            other, _, _, _ = run_case(candidate, f'shrink-{summary["shrink_attempts"]:03d}',
                                                      output, binary, nvim, args.timeout)
                        except InvalidTrace:
                            continue
                        if other is not None and failure_identity(other) == failure_identity(problem):
                            reduced, changed = candidate, True
                            break
                    if not changed:
                        width //= 2
            write_json(output / 'minimized.json', reduced)
            break
        if not args.keep_passing:
            for suffix in ('trace', 'input', 'actual', 'execution'):
                (output / f'{label}-{suffix}.json').unlink()
        if index % 100 == 0:
            summary['elapsed_seconds'] = time.monotonic() - started
            summary['status'] = 'running'
            write_json(output / 'summary.json', summary)
            print(json.dumps({'status': 'running', 'summary': str(output / 'summary.json'),
                              'cases': summary['cases_completed'], 'requested': summary['cases_requested']}), flush=True)
    summary['elapsed_seconds'] = time.monotonic() - started
    summary['status'] = 'failed' if summary['failure'] else 'passed'
    write_json(output / 'summary.json', summary)
    print(json.dumps({'status': summary['status'], 'summary': str(output / 'summary.json'),
                      'cases': summary['cases_completed'], 'states': summary['states'], 'failure': summary['failure']}))
    return int(summary['failure'] is not None)


if __name__ == '__main__':
    raise SystemExit(main())
