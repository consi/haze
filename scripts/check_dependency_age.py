#!/usr/bin/env python3
"""Verify every registry package in both lockfiles is at least seven days old.

Uses only the Python standard library. Registry responses are cached within a
run, never trusted from repository-controlled publication-date claims.
"""
import argparse
import concurrent.futures
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import time
import tomllib
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
AGE = timedelta(days=7)


def timestamp(value):
    result = datetime.fromisoformat(value.replace('Z', '+00:00'))
    if result.tzinfo is None:
        raise ValueError('Publication/check timestamps must include a timezone')
    return result.astimezone(timezone.utc)


def fetch(url):
    for attempt in range(3):
        try:
            request = urllib.request.Request(url, headers={
                'User-Agent': 'haze-dependency-vetting (https://github.com/consi/haze)',
                'Accept': 'application/json',
            })
            with urllib.request.urlopen(request, timeout=45) as response:
                return json.load(response)
        except Exception:
            if attempt == 2:
                raise
            time.sleep(2 ** attempt)


def metadata(ecosystem, name):
    encoded = urllib.parse.quote(name, safe='')
    if ecosystem == 'cargo':
        data = fetch(f'https://crates.io/api/v1/crates/{encoded}')
        return {v['num']: {'published': v['created_at'], 'yanked': v['yanked'],
                           'rust_version': v.get('rust_version')} for v in data['versions']}
    data = fetch(f'https://registry.npmjs.org/{encoded}')
    return {v: {'published': data.get('time', {}).get(v),
                'deprecated': bool(info.get('deprecated')),
                'engines': info.get('engines', {})}
            for v, info in data['versions'].items()}


def locked_packages(root):
    packages = set()
    for package in tomllib.loads((root / 'Cargo.lock').read_text())['package']:
        source = package.get('source')
        if source is None:
            continue  # Cargo workspace/path packages are not published dependencies.
        if source != 'registry+https://github.com/rust-lang/crates.io-index':
            raise ValueError(f'Unapproved Cargo source: {source}')
        packages.add(('cargo', package['name'], package['version']))
    lock = json.loads((root / 'frontend/package-lock.json').read_text())
    for path, package in lock['packages'].items():
        if not path:
            continue
        resolved = package.get('resolved', '')
        parsed = urllib.parse.urlparse(resolved)
        if parsed.scheme != 'https' or parsed.netloc != 'registry.npmjs.org':
            raise ValueError(f'Unapproved npm source for {path}: {resolved}')
        name = package.get('name') or path.rsplit('node_modules/', 1)[-1]
        packages.add(('npm', name, package['version']))
    return sorted(packages)


def violation(info, now):
    if not info or not info.get('published'):
        return 'missing publication metadata'
    if timestamp(info['published']) > now - AGE:
        return f"published {info['published']}; requires 168 hours"
    if info.get('yanked'):
        return 'release is yanked'
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--at', help='UTC reference time for reproducible local vetting')
    parser.add_argument('--report', type=Path)
    parser.add_argument('--metadata-out', type=Path, help='Export registry metadata for upgrade planning')
    args = parser.parse_args()
    now = timestamp(args.at) if args.at else datetime.now(timezone.utc)
    packages = locked_packages(ROOT)
    names = sorted({(eco, name) for eco, name, _ in packages})
    registry, errors = {}, []
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        futures = {pool.submit(metadata, *key): key for key in names}
        for future in concurrent.futures.as_completed(futures):
            key = futures[future]
            try:
                registry[key] = future.result()
            except Exception as error:
                errors.append(f'{key[0]}:{key[1]}: registry lookup failed: {error}')
    rows = []
    for eco, name, version in packages:
        info = registry.get((eco, name), {}).get(version)
        problem = violation(info, now)
        if problem:
            errors.append(f'{eco}:{name}@{version}: {problem}')
        rows.append({'ecosystem': eco, 'name': name, 'version': version,
                     'published': (info or {}).get('published'), 'error': problem})
    report = {'checked_at': now.isoformat(), 'cutoff': (now - AGE).isoformat(),
              'packages': rows, 'errors': errors}
    if args.report:
        args.report.write_text(json.dumps(report, indent=2) + '\n')
    if args.metadata_out:
        args.metadata_out.write_text(json.dumps({f'{e}:{n}': v for (e, n), v in registry.items()}))
    for error in errors:
        print(error)
    print(f'Checked {len(packages)} locked packages; {len(errors)} violations.')
    return bool(errors)


if __name__ == '__main__':
    raise SystemExit(main())
