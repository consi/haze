#!/usr/bin/env python3
"""Require reviewed publication evidence for immutable build/CI tool pins."""
import json
from pathlib import Path
import re
from datetime import datetime, timezone
from check_dependency_age import ROOT, violation


def check(root=ROOT):
    evidence = json.loads((root / 'docs/tooling-vetting.json').read_text())
    now = datetime.now(timezone.utc)
    errors = []
    for category in ('actions', 'cargo', 'runtime', 'containers'):
        for name, entry in evidence[category].items():
            problem = violation(entry, now)
            if problem or not entry.get('source', '').startswith('https://'):
                errors.append(f'{category}:{name}: {problem or "missing source"}')
    workflows = '\n'.join(p.read_text() for p in (root / '.github/workflows').glob('*.yml'))
    for repo, revision in re.findall(r'uses:\s*([^@\s]+)@([^\s]+)', workflows):
        if revision != evidence['actions'].get(repo, {}).get('revision'):
            errors.append(f'Unvetted action: {repo}@{revision}')
    commands = workflows + '\n' + (root / 'justfile').read_text()
    for tool, version in re.findall(r'cargo install --locked ([\w-]+)(?: --version ([\w.]+))?', commands):
        if version != evidence['cargo'].get(tool, {}).get('version'):
            errors.append(f'Unvetted cargo tool: {tool}@{version}')
    for runtime, pattern in [('rust', r'toolchain:\s*([^\s]+)'), ('node', r'node-version:\s*([^\s]+)')]:
        for version in re.findall(pattern, workflows):
            if version != evidence['runtime'][runtime]['version']:
                errors.append(f'Unvetted {runtime}: {version}')
    import tomllib
    if tomllib.loads((root / 'rust-toolchain.toml').read_text())['toolchain']['channel'] != evidence['runtime']['rust']['version']:
        errors.append('Rust toolchain file disagrees with vetting record')
    if (root / 'frontend/.node-version').read_text().strip() != evidence['runtime']['node']['version']:
        errors.append('Node version file disagrees with vetting record')
    images = {name.removeprefix('library/') + '@' + item['revision']
              for name, item in evidence['containers'].items()}
    for file in ('Dockerfile', 'Dockerfile.dev'):
        text = (root / file).read_text()
        for image in re.findall(r'^FROM\s+(?:--platform=\S+\s+)?(\S+)', text, re.M):
            if image not in images:
                errors.append(f'Unvetted container: {image}')
        syntax = re.search(r'^# syntax=(\S+)', text)
        if syntax and syntax[1] not in images:
            errors.append(f'Unvetted Dockerfile frontend: {syntax[1]}')
    return errors


if __name__ == '__main__':
    errors = check()
    print('\n'.join(errors) if errors else 'Build tooling pins and publication evidence verified.')
    raise SystemExit(bool(errors))
