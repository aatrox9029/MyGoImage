"""Correlate source stages with displayed main requests; exclude prefetched work.

RenderingState::AfterRendering occurs before buffer presentation, not GPU fence
completion. Pixel observation remains in benchmark-viewer.py's results.json.
"""
import argparse
import json
from pathlib import Path


def key(record, generation=False):
    trace = record['trace']
    return (trace['request'], trace['key']) + ((trace['generation'],) if generation else ())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('viewer', type=Path)
    args = parser.parse_args()
    displayed = []
    source_rows = []
    controller_rows = []
    for path in sorted(args.viewer.glob('*.diagnostics.json')):
        records = json.loads(path.read_text(encoding='utf-8'))
        assert not any(r['dropped_total'] for r in records), 'Incomplete diagnostics'
        sources = {}
        rendered = {}
        buffers = {}
        after = {}
        cache_origins = {}
        for record in records:
            if record['event'] in {'index-position', 'index-position-background', 'source-prepare', 'source-probe', 'settings-save', 'startup-main-to-telemetry', 'startup-ui-create', 'startup-settings-load', 'startup-index-open'}:
                controller_rows.append(dict(session=path.name, event=record['event'], duration_us=record['duration_us'], ok=record['ok']))
            if not record.get('trace') or not record['ok']:
                continue
            event = record['event']
            if event in {'cache-ram-hit', 'cache-disk-hit', 'cache-source-miss', 'cache-disk-busy', 'cache-disk-invalid'}:
                cache_origins[key(record)] = event
            if event.startswith('image-'):
                sources.setdefault(key(record), {})[event] = record
            elif event == 'request-to-after-render':
                rendered[key(record, True)] = record
            elif event == 'ui-main-buffer-apply':
                buffers[key(record, True)] = record
            elif event == 'ui-apply-to-after-render':
                after[key(record, True)] = record
        for record in records:
            if record['event'] != 'ui-main-apply' or not record['ok']:
                continue
            request_key = key(record, True)
            source = sources.get(key(record), {})
            row = dict(session=path.name, request=request_key[0], generation=request_key[2], key=request_key[1])
            row['cache_lookup'] = cache_origins.get(key(record), 'unrecorded')
            displayed.append(row)
            if request_key not in rendered or request_key not in buffers or request_key not in after:
                row['classification'] = 'no-matched-render-callback'
                continue
            if 'image-total' not in source:
                row['classification'] = {'cache-ram-hit': 'ram-hit', 'cache-disk-hit': 'disk-hit'}.get(row['cache_lookup'], 'no-matched-source-work')
                continue
            expected = {'image-io', 'image-header', 'image-codec', 'image-orientation', 'image-resize', 'image-rgba', 'image-icc', 'image-other', 'image-total'}
            assert set(source) == expected, 'Incomplete source stages'
            enqueued = record['monotonic_us'] - record['duration_us']
            source_start = source['image-total']['monotonic_us'] - source['image-total']['duration_us']
            if source_start < enqueued:
                row['classification'] = 'source-started-before-subscription'
                continue
            buckets = {event: value['duration_us'] for event, value in source.items() if event != 'image-total'}
            buckets['ui-buffer-apply'] = buffers[request_key]['duration_us']
            buckets['apply-to-after-render'] = after[request_key]['duration_us']
            total = rendered[request_key]['duration_us']
            remaining = total - sum(buckets.values())
            if remaining < 0:
                row['classification'] = 'overlapping-timing-boundaries'
                continue
            buckets['queue-delivery-other'] = remaining
            row.update(classification='on-demand-source', total_us=total, buckets_us=buckets)
            source_rows.append(row)
    totals = {}
    for row in source_rows:
        for event, us in row['buckets_us'].items():
            totals[event] = totals.get(event, 0) + us
    total_us = sum(row['total_us'] for row in source_rows)
    report = dict(scope='Exclusive attribution only for displayed main requests whose source starts after that subscriber enqueue; cache hits and prefetch excluded. Shares use sums of matched durations, not sums of percentiles. AfterRendering is before presentation and does not measure GPU execution.', displayed_requests=len(displayed), on_demand_requests=len(source_rows), total_us=total_us, bucket_sum_us=totals, share_percent={event: us / total_us * 100 for event, us in totals.items()} if total_us else {}, requests=displayed)
    report['controller_samples'] = controller_rows
    with (args.viewer / 'stage-attribution.json').open('x', encoding='utf-8') as output:
        json.dump(report, output, indent=2)
    print(json.dumps({k: v for k, v in report.items() if k not in {'requests', 'controller_samples'}}, indent=2))


if __name__ == '__main__':
    main()
