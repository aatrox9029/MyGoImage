"""Reuse historical traces and summarize the bounded October 18 follow-up."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT/'test-output/targeted-20261018'
OLD = ROOT/'test-output/optimization-next-20261018'


def read(path):
    return json.loads(path.read_text(encoding='utf-8'))


def stages(rows):
    events = ['startup-ui-create', 'startup-settings-load', 'startup-index-open',
              'source-prepare', 'image-total', 'startup-first-ui-pump',
              'startup-renderer-ready', 'startup-first-after-render',
              'ui-main-apply', 'request-to-after-render']
    return {event: dict(at_ms=row['monotonic_us']/1000, duration_ms=row['duration_us']/1000)
            for event in events if (row := next((r for r in rows if r['event'] == event), None))}


def main():
    report = dict(historical_slow_samples=[], startup_order_checks=[], paging={}, sd={})
    for directory in ['single-delivery', 'release-smoke']:
        old = read(OLD/directory/'results.json')
        records = old.get('records', [dict(pair=0, variant='release', result=old)])
        for record in records:
            for run in record['result']['runs']:
                if run['samples'][0]['ms'] < 1000:
                    continue
                prefix = (f"{record['pair']-1}-{record['variant']}-" if record['pair'] else '')
                trace = OLD/directory/f"{prefix}0-{run['cache']}.diagnostics.json"
                report['historical_slow_samples'].append(dict(
                    dataset=directory, pair=record['pair'], variant=record['variant'],
                    cache=run['cache'], presented_pixel_ms=run['samples'][0]['ms'],
                    startup_timing=run['startup_timing'], stages=stages(read(trace))))
    paired = OUT/'startup-paired/results.json'
    if paired.exists():
        startup = read(paired)
        report['startup_summary'] = startup['summary']
        report['startup_measured_sha256'] = startup['after_sha256']
        for path in sorted(paired.parent.glob('*after-*.diagnostics.json')):
            timings = stages(read(path))
            assert timings['ui-main-apply']['at_ms'] < timings['startup-first-after-render']['at_ms'], path
            report['startup_order_checks'].append(dict(trace=path.name, stages=timings,
                first_image_applied_before_first_after_render=True))
    for label, path in [('historical', OLD/'multi12-large-delivery/results.json'),
                        ('baseline_followup', OUT/'multi-baseline/results.json'),
                        ('final_followup', OUT/'multi-final/results.json')]:
        if path.exists():
            data = read(path)
            assert all(r['dropped_diagnostics'] == 0 and all(s['matched'] == 12 for s in r['samples'])
                       for r in data['records'])
            report['paging'][label] = data['summary']
            report['paging'][label]['measured_after_sha256'] = data['after_sha256']
    for label in ['sd-125', 'sd-200']:
        path = OUT/label/'results.json'
        if path.exists():
            data = read(path)
            assert data['exit_code'] == 0 and data['originals_unchanged']
            assert round(data['scale']*1000) in data['observed_scale_milli']
            report['sd'][label] = {k: data[k] for k in ['scale', 'observed_scale_milli',
                'duration_seconds', 'navigation_commands', 'median_rss_bands',
                'peak_sampled_rss_bytes', 'diagnostic_tail_max', 'dropped_diagnostics']}
            report['sd'][label]['content_checks'] = len(data['content_checks'])
            report['sd'][label]['max_mean_channel_error'] = max(max(c['errors']) for c in data['content_checks'])
            report['sd'][label]['photos'] = len(data['manifest'])
            report['sd'][label]['measured_sha256'] = data['exe_sha256']
    files = [ROOT/'crates/app/src/main.rs', ROOT/'ui/main.slint', ROOT/'target/release/mygoimage.exe',
             OUT/'before.exe', OUT/'after.exe', OUT/'startup-fix.exe', Path(__file__),
             ROOT/'scripts/check-real-photo-session.py']
    report['sha256'] = {p.relative_to(ROOT).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
    report['final_executable_matches_measured'] = report['sha256']['target/release/mygoimage.exe'] == report['sha256']['test-output/targeted-20261018/after.exe']
    (OUT/'summary.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
