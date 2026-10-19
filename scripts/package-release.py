"""Fresh, verified portable staging; existing portable directories are never touched."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import zipfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--target', required=True)
args = parser.parse_args()
metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'], cwd=root))
version = next(p['version'] for p in metadata['packages'] if p['name'] == 'mygoimage')
assert re.fullmatch(r'\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?', version)
dist = (root / 'dist').resolve()
dist.mkdir(exist_ok=True)
digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
with tempfile.TemporaryDirectory(prefix='staging-', dir=dist) as temporary:
    stage = Path(temporary).resolve()
    assert stage.parent == dist and stage.name.startswith('staging-'), 'Unsafe staging path'
    shutil.copy2(args.binary, stage / 'mygoimage.exe')
    for name in ['README.md', 'README.zh-TW.md', 'LICENSE', 'Cargo.lock']:
        shutil.copy2(root / name, stage / name)
    source = root / 'THIRD_PARTY_LICENSES'
    notices = stage / 'THIRD_PARTY_LICENSES'
    notices.mkdir()
    for name in ['dependencies.json', 'INDEX.md', 'UPSTREAM-NOTICES.md']:
        shutil.copy2(source / name, notices / name)
    dependencies = json.loads((source / 'dependencies.json').read_text(encoding='utf-8'))
    tree = subprocess.check_output(['cargo', 'tree', '--locked', '--target', args.target,
                                    '--edges', 'normal,build', '--prefix', 'none', '--format', '{p}'], cwd=root, text=True)
    target_dependencies = {(match[1], match[2]) for line in tree.splitlines()
                           if (match := re.match(r'^(\S+) v(\S+)', line))}
    for dep in dependencies:
        assert dep['texts'] or (dep['name'], dep['version']) not in target_dependencies, f"Missing target license text: {dep['name']} {dep['version']}"
        name = dep['name'] + '-' + dep['version']
        shutil.copytree(source / name, notices / name)
    manifest = dict(version=version, target=args.target,
                    non_target_license_omissions=[dict(name=p['name'],version=p['version']) for p in dependencies if not p['texts']],
                    rustc=subprocess.check_output(['rustc', '-vV'], text=True),
                    compiler=subprocess.check_output(['gcc', '--version'], text=True).splitlines()[0] if shutil.which('gcc') else 'MSVC: see CI environment artifact',
                    files={str(p.relative_to(stage)).replace('\\', '/'): digest(p) for p in stage.rglob('*') if p.is_file()})
    (stage / 'build-manifest.json').write_text(json.dumps(manifest, indent=2), encoding='utf-8')
    archive = dist / f'MyGoImage-{version}-windows-x64.zip'
    with tempfile.NamedTemporaryFile(prefix='pending-', suffix='.zip', dir=dist, delete=False) as pending:
        pending_path = Path(pending.name).resolve()
    assert pending_path.parent == dist, 'Unsafe pending archive path'
    try:
        with zipfile.ZipFile(pending_path, 'w', zipfile.ZIP_DEFLATED, strict_timestamps=False) as out:
            for path in sorted(stage.rglob('*')):
                if path.is_file():
                    out.write(path, str(path.relative_to(stage)).replace('\\', '/'))
        with zipfile.ZipFile(pending_path) as result:
            assert result.testzip() is None
            assert len(result.namelist()) == len(set(result.namelist()))
            for name, sha in manifest['files'].items():
                assert hashlib.sha256(result.read(name)).hexdigest() == sha, name
        os.replace(pending_path, archive)
        sha = digest(archive)
        Path(str(archive) + '.sha256').write_text(f'{sha}  {archive.name}\n', encoding='ascii')
        print(json.dumps(dict(archive=str(archive), sha256=sha, files=len(manifest['files']) + 1)))
    finally:
        pending_path.unlink(missing_ok=True)
