"""Alternate two index benchmark executables on one reopened synthetic index."""
import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--entries', type=int, default=300000)
    parser.add_argument('--pairs', type=int, default=5)
    args = parser.parse_args()
    assert args.entries >= 1000 and args.pairs > 0
    args.output.mkdir(parents=True, exist_ok=False)
    database = args.output / 'index.sqlite3'

    def run(variant):
        result = subprocess.run([str(getattr(args, variant).resolve()), str(database.resolve()), str(args.entries)], check=True, capture_output=True, text=True)
        row = json.loads(result.stdout)
        assert row['entries'] == args.entries
        return row

    warmup = {variant: run(variant) for variant in ['before', 'after']}
    records = []
    for pair in range(args.pairs):
        for variant in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
            row = dict(pair=pair + 1, variant=variant, result=run(variant))
            records.append(row)
            print(json.dumps(row), flush=True)
    summary = {variant: {metric: statistics.median(row['result'][metric] for row in records if row['variant'] == variant)
                         for metric in ['position_p95_ms', 'page_p95_ms', 'cursor_p95_ms']}
               for variant in ['before', 'after']}
    report = dict(scope='Reopened synthetic index; alternating process order; one warmup per executable excluded; medians of per-run p95, not pooled percentiles; no UI latency claim; OS cache not flushed',
                  entries=args.entries, pairs=args.pairs,
                  sha256={variant: hashlib.sha256(getattr(args, variant).read_bytes()).hexdigest() for variant in ['before', 'after']},
                  warmup=warmup, records=records, summary=summary)
    (args.output / 'results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
