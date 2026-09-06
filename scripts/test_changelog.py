#!/usr/bin/env python3
"""Exercise first/latest/unreleased changelogs in a disposable Git history."""
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
CLIFF = os.environ.get('GIT_CLIFF', 'git-cliff')

with tempfile.TemporaryDirectory() as directory:
    def run(*args):
        return subprocess.check_output(args, cwd=directory, text=True, stderr=subprocess.STDOUT)

    run('git', 'init', '-q')
    run('git', 'config', 'user.name', 'Changelog fixture')
    run('git', 'config', 'user.email', 'fixture@example.invalid')
    run('git', 'commit', '--allow-empty', '-qm', 'Add initial monitoring')
    run('git', 'tag', 'v0.1.0')
    first = run(CLIFF, '--config', str(ROOT / 'cliff.toml'), '--latest')
    assert 'Add initial monitoring' in first
    run('git', 'commit', '--allow-empty', '-qm', 'fix(history): retain partial paths', '-m', 'Keep responding transit hops when the deadline expires.')
    run('git', 'commit', '--allow-empty', '-qm', 'feat!: change configuration', '-m', 'BREAKING CHANGE: Rename the configuration key.')
    unreleased = run(CLIFF, '--config', str(ROOT / 'cliff.toml'), '--unreleased')
    assert 'Unreleased' in unreleased and 'initial monitoring' not in unreleased
    assert 'Keep responding transit hops' in unreleased and '**Breaking:**' in unreleased
    run('git', 'tag', 'v0.2.0')
    latest = run(CLIFF, '--config', str(ROOT / 'cliff.toml'), '--latest', '--strip', 'header')
    assert 'v0.2.0' in latest and 'initial monitoring' not in latest
    assert 'v0.1.0...v0.2.0' in latest
print('Changelog first/latest/unreleased, historical subjects, bodies and breaking changes passed.')
