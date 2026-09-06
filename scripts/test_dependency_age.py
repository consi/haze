import unittest
from datetime import datetime, timedelta, timezone
from check_dependency_age import violation, timestamp
from check_dependency_age import locked_packages, fetch
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch
import json


class AgePolicyTests(unittest.TestCase):
    def setUp(self):
        self.now = datetime(2026, 9, 6, tzinfo=timezone.utc)

    def test_exact_boundary_is_allowed(self):
        self.assertIsNone(violation({'published': (self.now - timedelta(days=7)).isoformat()}, self.now))

    def test_one_second_too_young_is_rejected(self):
        self.assertIsNotNone(violation({'published': (self.now - timedelta(days=7) + timedelta(seconds=1)).isoformat()}, self.now))

    def test_missing_and_yanked_are_rejected(self):
        for info in [None, {}, {'published': '2020-01-01T00:00:00Z', 'yanked': True}]:
            self.assertIsNotNone(violation(info, self.now))

    def test_future_publication_is_rejected(self):
        self.assertIsNotNone(violation({'published': '2027-01-01T00:00:00Z'}, self.now))

    def test_timezone_required(self):
        with self.assertRaises(ValueError):
            timestamp('2026-09-06T00:00:00')

    def test_nested_optional_and_dev_packages_are_included(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'Cargo.lock').write_text('[[package]]\nname="local"\nversion="0.1.0"\n')
            (root / 'frontend').mkdir()
            (root / 'frontend/package-lock.json').write_text(json.dumps({'packages': {
                '': {'name': 'app'},
                'node_modules/direct/node_modules/transitive': {
                    'version': '1.0.0', 'dev': True, 'optional': True,
                    'resolved': 'https://registry.npmjs.org/transitive/-/transitive-1.0.0.tgz',
                },
            }}))
            self.assertEqual(locked_packages(root), [('npm', 'transitive', '1.0.0')])

    def test_unknown_source_is_rejected(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'Cargo.lock').write_text('[[package]]\nname="unknown"\nversion="1.0.0"\nsource="git+https://example.com/repo"\n')
            with self.assertRaisesRegex(ValueError, 'Unapproved Cargo source'):
                locked_packages(root)

    def test_registry_outage_retries_then_fails(self):
        with patch('urllib.request.urlopen', side_effect=OSError('offline')) as request, patch('time.sleep'):
            with self.assertRaises(OSError):
                fetch('https://registry.npmjs.org/example')
            self.assertEqual(request.call_count, 3)


if __name__ == '__main__':
    unittest.main()
