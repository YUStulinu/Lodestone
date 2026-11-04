"""Summarises a `lodestone slt --json` run into results/sqllogictest.json.

    python scripts/slt_summary.py data/slt-all.json
"""
import collections
import json
import re
import sys

runs = json.load(open(sys.argv[1]))


def group(f):
    p = f.split('data/slt/')[1]
    if p.startswith('select'):
        return 'select1-5'
    parts = p.split('/')
    return parts[0] + '/' + parts[1] if parts[0] in ('random', 'index') else parts[0]


def category(file, head):
    if 'GROUP BY clause' in head:
        return 'bare column outside GROUP BY: SQLite allows it, PostgreSQL (and Lodestone) reject it'
    if 'division by zero' in head:
        return 'division by zero: SQLite gives NULL, PostgreSQL (and Lodestone) an error'
    if 'TRIGGER' in head.upper():
        return 'CREATE/DROP TRIGGER: not supported'
    if 'replace' in file:
        return 'INSERT OR REPLACE / REPLACE INTO: not supported (and the queries that read its rows)'
    if "'303132'" in head:
        return "blob literal x'...': no BLOB type"
    return 'other: ' + re.sub(r'line \d+: ', '', head)[:100]


groups = collections.OrderedDict()
for r in sorted(runs, key=lambda r: group(r['file'])):
    g = groups.setdefault(group(r['file']), dict(files=0, records=0, passed=0, skipped=0))
    n = r['statements'] + r['queries']
    g['files'] += 1
    g['records'] += n
    g['passed'] += n - r['statements_failed'] - r['queries_failed']
    g['skipped'] += r['skipped']
cats = collections.Counter(category(r['file'], h) for r in runs for h in r['failure_heads'])
total = sum(g['records'] for g in groups.values())
passed = sum(g['passed'] for g in groups.values())
out = dict(files=len(runs), records=total, passed=passed, failed=total - passed, pass_rate=round(passed / total, 5),
           skipped_records=sum(g['skipped'] for g in groups.values()),
           skipped_note='records guarded by onlyif <another engine>: results only SQLite, MySQL, ... give',
           groups=groups, failure_categories=dict(cats.most_common()))
json.dump(out, open('results/sqllogictest.json', 'w'), indent=2)
print(json.dumps(out['failure_categories'], indent=2))
print(f"{passed} / {total} passed ({100 * passed / total:.2f}%)")
