"""Fill upstream-omitted license texts from exact published Git commit metadata."""
import concurrent.futures, json, pathlib, re, urllib.request

root=pathlib.Path(__file__).resolve().parents[1]
out=root/'THIRD_PARTY_LICENSES'
rows=json.loads((out/'dependencies.json').read_text(encoding='utf-8'))
registry=pathlib.Path.home()/'.cargo/registry/src'
groups={}
for p in rows:
    if p['texts']:continue
    repo=p['repository']
    if not repo:continue
    m=re.match(r'https?://github.com/([^/]+/[^/]+)',repo)
    if not m:continue
    repo=m[1].removesuffix('.git')
    folders=list(registry.glob(f"*/{p['name']}-{p['version']}"))
    vcs=folders[0]/'.cargo_vcs_info.json' if folders else None
    commit=json.loads(vcs.read_text()).get('git',{}).get('sha1','HEAD') if vcs and vcs.exists() else 'HEAD'
    groups.setdefault((repo,commit),[]).append(p)

def get(url):
    req=urllib.request.Request(url,headers={'User-Agent':'MyGoImage-license-notices'})
    with urllib.request.urlopen(req,timeout=25) as r:return r.read()
def fetch(group):
    (repo,commit),packages=group
    try:
        listing=json.loads(get(f'https://api.github.com/repos/{repo}/contents?ref={commit}'))
        files=[f for f in listing if f['type']=='file' and f['name'].lower().startswith(('license','copying','notice','copyright'))]
        for f in files:
            data=get(f['download_url'])
            for p in packages:
                dest=out/f"{p['name']}-{p['version']}";dest.mkdir(exist_ok=True)
                name='upstream-'+f['name'];(dest/name).write_bytes(data)
                p['texts'].append(name)
                p.setdefault('upstream_sources',[]).append(f['html_url'])
        return f'{repo}@{commit[:8]}: {len(files)} texts for {len(packages)} packages'
    except Exception as e:return f'{repo}: {e}'
with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
    for result in pool.map(fetch,groups.items()):print(result)
(out/'dependencies.json').write_text(json.dumps(rows,indent=2,ensure_ascii=False),encoding='utf-8')
missing=[p['name']+' '+p['version'] for p in rows if not p['texts']]
(out/'UPSTREAM-NOTICES.md').write_text('# Upstream license provenance\n\nSome published crates omit their license texts. This package retains their declared SPDX expression in INDEX.md. Supplemental texts are retrieved from each crate’s published .cargo_vcs_info.json commit when available; exact source URLs are in dependencies.json. HEAD is used only when no commit metadata was published.\n\nPackages still lacking upstream texts (may be build-only or non-Windows targets):\n\n'+('\n'.join('- '+n for n in missing) or 'None')+'\n',encoding='utf-8')
print('Remaining without texts:',len(missing))
