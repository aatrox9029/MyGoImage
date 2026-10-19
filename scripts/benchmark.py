"""Three new-db and three reopened-db runs. Requires psutil. No photo/UI claims."""
import datetime, json, pathlib, platform, subprocess, time
import psutil

root=pathlib.Path(__file__).resolve().parents[1]
out=root/'test-output'
out.mkdir(exist_ok=True)
exe=root/'target'/'release'/'examples'/'benchmark.exe'
subprocess.run(['cargo','build','--locked','--release','--workspace','--examples'],cwd=root,check=True)
runs=[]
for i in range(3):
    # Unique db files: cold application index, NOT a flushed Windows filesystem cache.
    db=out/f'benchmark-{time.time_ns()}.sqlite3'
    for kind in ['new-index','reopened-index']:
        start=time.perf_counter()
        p=subprocess.Popen([str(exe),str(db)],stdout=subprocess.PIPE,text=True)
        proc=psutil.Process(p.pid); peak=0; cpu=0; reads=0
        while p.poll() is None:
            try:
                peak=max(peak,proc.memory_info().rss)
                c=proc.cpu_times();cpu=c.user+c.system
                reads=proc.io_counters().read_bytes
            except psutil.NoSuchProcess:
                pass
            time.sleep(.01)
        stdout,_=p.communicate()
        if p.returncode:raise RuntimeError(stdout)
        runs.append(dict(run=i+1,kind=kind,wall_ms=(time.perf_counter()-start)*1000,peak_rss_bytes=peak,sampled_cpu_s=cpu,sampled_read_bytes=reads,**json.loads(stdout)))
report=dict(date=datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat(),platform=platform.platform(),cpu=platform.processor(),ram_bytes=psutil.virtual_memory().total,scope='Synthetic SQLite metadata only; release build; OS filesystem cache not flushed; GPU/UI/photos/SD card not measured',runs=runs)
fixtures=out/'scan-10000'
fixtures.mkdir(exist_ok=True)
for i in range(10000):
    (fixtures/f'{i:06}.jpg').touch(exist_ok=True)
scan=[]
for i in range(6):
    result=json.loads(subprocess.check_output([str(root/'target/release/examples/scan_benchmark.exe'),str(fixtures)],text=True))
    scan.append(dict(run=i//2+1,kind='new-process' if i%2==0 else 'repeated-process',**result))
report['scan_10000_empty_files']=scan
report['scan_note']='Real directory metadata enumeration; empty JPG placeholders; no image decode; Windows filesystem cache not flushed.'
(out/'benchmark.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
print(json.dumps(report,indent=2))
