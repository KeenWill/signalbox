"""Resolve a review inventory's explicit citations in tracked head source."""

import re
import subprocess
from pathlib import PurePosixPath
from urllib.parse import unquote

PATH = re.compile(
    r"(?<![\w/])(?P<path>(?:[\w.@+-]+/)*[\w.@+-]+\.[A-Za-z][\w-]*)"
    r"(?:(?::(?:L)?|\#L)(?P<line>[1-9][0-9]*)(?:-(?:L)?(?P<end>[1-9][0-9]*))?)?"
)
IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z_0-9]*(?:(?:::|\.)[A-Za-z_][A-Za-z_0-9]*)*")
CODE = re.compile(r"`+([^`\n]+)`+")
NAMED = re.compile(
    r"\b(?:[A-Za-z_][A-Za-z_0-9]*(?:(?:::|\.)[A-Za-z_][A-Za-z_0-9]*)+"
    r"|[a-zA-Z][a-zA-Z0-9]*_[a-zA-Z_0-9]+|[A-Z][a-z]+[A-Z][A-Za-z0-9]*"
    r"|[A-Za-z_][A-Za-z_0-9]*(?=\())\b"
)


def git(checkout, *arguments):
    return subprocess.run(
        ["git", "-C", str(checkout), *arguments], check=True,
        capture_output=True,
    ).stdout


def citations(finding):
    """Extract the location, source paths and explicitly written identifiers."""
    text = "\n".join(str(finding.get(key) or "") for key in
                     ("title", "body", "finding_text", "recommended_fix"))
    references = []
    path = finding.get("file_path", finding.get("path"))
    line = finding.get("line_start", finding.get("line"))
    if path:
        references.append({"kind": "path", "value": path,
                           "line": int(line) if line else None,
                           "end_line": int(finding.get("line_end") or line) if line else None})
    # GitHub blob links contain a revision prefix, not part of the repository path.
    text = re.sub(r"https?://[^\s)]+?/blob/[^/]+/([^\s)]+)",
                  lambda match: unquote(match[1]), text)
    spans = []
    for match in PATH.finditer(text):
        value = match['path']
        # Method/member expressions are identifiers rather than file names.
        if '/' not in value and not re.search(r'\.(?:rs|py|sql|swift|md|toml|json|ya?ml|tsx?|jsx?|sh|bzl|c|h|cpp|hpp)$', value):
            continue
        references.append({"kind": "path", "value": value,
                           "line": int(match['line']) if match['line'] else None,
                           "end_line": int(match['end'] or match['line']) if match['line'] else None})
        spans.append(match.span())
    chars = list(text)
    for start, end in spans:
        chars[start:end] = ' ' * (end-start)
    identifiers = ''.join(chars)
    for match in CODE.finditer(identifiers):
        value = match[1].strip().removesuffix('()')
        if IDENTIFIER.fullmatch(value):
            references.append({"kind": "identifier", "value": value})
    references.extend({"kind": "identifier", "value": match[0]}
                      for match in NAMED.finditer(identifiers))
    unique = []
    for reference in references:
        if reference not in unique:
            unique.append(reference)
    return unique


def resolve_findings(checkout, head, findings):
    """Attach source evidence before the caller starts the judgment pass.

    Found means present in tracked source at this revision, not a claim that a
    historical migration's declaration still exists in a running database.
    """
    actual = git(checkout, "rev-parse", "HEAD").decode().strip()
    if actual != head:
        raise ValueError("citation checkout differs from the review head")
    paths = [path.decode() for path in git(checkout, "ls-tree", "-rz", "--name-only", head).split(b'\0') if path]
    inputs = [citations(finding) for finding in findings]
    names = {reference['value'].split('::')[-1].split('.')[-1]
             for refs in inputs for reference in refs if reference['kind'] == 'identifier'}
    occurrences = {name: [] for name in names}
    if names:
        arguments = ["git", "-C", str(checkout), "grep", "-n", "-z", "-I", "-w", "-F"]
        for name in sorted(names):
            arguments.extend(['-e', name])
        searched = subprocess.run([*arguments, head, '--'], capture_output=True)
        if searched.returncode not in (0, 1):
            raise subprocess.CalledProcessError(searched.returncode, arguments, searched.stdout, searched.stderr)
        for record in searched.stdout.splitlines():
            path, line, content = record.split(b'\0', 2)
            path = path.decode().removeprefix(head + ':')
            text = content.decode('utf-8', errors='replace')
            for name in set(re.findall(r'[A-Za-z_][A-Za-z_0-9]*', text)) & names:
                occurrences[name].append({'path': path, 'line': int(line), 'text': text})
    resolved = []
    for finding, references in zip(findings, inputs):
        evidence = []
        cited_paths = {ref['value'] for ref in references if ref['kind'] == 'path'}
        primary_line = int(finding.get('line_start') or finding.get('line') or 1)
        for reference in references:
            if reference['kind'] == 'path':
                value = reference['value']
                matches = [value] if value in paths else [p for p in paths if '/' not in value and PurePosixPath(p).name == value]
                locations = []
                for path in matches:
                    source = git(checkout, 'show', head + ':' + path).decode('utf-8', errors='replace').splitlines()
                    first = reference['line'] or 1
                    last = reference['end_line'] or first
                    if 1 <= first <= last <= len(source):
                        locations.append({'path': path, 'line': first, 'end_line': last,
                                          'text': '\n'.join(source[first-1:last])})
            else:
                name = reference['value'].split('::')[-1].split('.')[-1]
                locations = occurrences[name]
                # A representative source occurrence keeps common identifiers
                # from duplicating entire generated API inventories in context.
                locations = sorted(locations, key=lambda item: (
                    item['path'] not in cited_paths,
                    abs(item['line'] - primary_line) if item['path'] in cited_paths else 0,
                    item['path'], item['line'],
                ))
            evidence.append({'citation': reference, 'status': 'found_at_head' if locations else 'absent_at_head',
                             'source_match_count': len(locations), 'matches': locations[:1]})
        resolved.append({**finding, 'citation_resolution': {
            'head_sha': head, 'scope': 'tracked_source', 'references': evidence,
        }})
    return resolved
