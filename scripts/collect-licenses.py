"""Generate lockfile-resolved dependency notices and retain packaged license texts."""
import json, pathlib, shutil, subprocess

root = pathlib.Path(__file__).resolve().parents[1]
metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version', '1'], cwd=root))
out = root / 'THIRD_PARTY_LICENSES'
out.mkdir(exist_ok=True)
rows = []
for p in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
    if p['source'] is None:
        continue
    folder = pathlib.Path(p['manifest_path']).parent
    dest = out / (p['name'] + '-' + p['version'])
    dest.mkdir(exist_ok=True)
    copied = []
    for pattern in ('LICENSE*', 'COPYING*', 'NOTICE*', 'license*'):
        for src in folder.glob(pattern):
            if src.is_file():
                shutil.copy2(src, dest / src.name)
                copied.append(src.name)
            elif src.is_dir():
                shutil.copytree(src, dest / src.name, dirs_exist_ok=True)
                copied.extend(str(f.relative_to(folder)) for f in src.rglob('*') if f.is_file())
    if p.get('license_file'):
        src=folder / p['license_file']
        if src.is_file():
            shutil.copy2(src,dest/src.name)
            copied.append(src.name)
    rows.append({'name': p['name'], 'version': p['version'], 'license': p.get('license'), 'repository': p.get('repository'), 'texts': sorted(set(copied))})
(out / 'dependencies.json').write_text(json.dumps(rows, indent=2, ensure_ascii=False), encoding='utf-8')
(out / 'INDEX.md').write_text('# Cargo.lock third-party inventory\n\nIncludes target-specific and build-only dependencies.\n\n| Crate | Version | License |\n|---|---|---|\n' + '\n'.join(f"| {p['name']} | {p['version']} | {p['license']} |" for p in rows), encoding='utf-8')
print(f'Collected {len(rows)} dependency records into {out}')
