"""Compare one-batch and budgeted scan consumption through the actual controller."""
import argparse, json, os, pathlib, subprocess, time, psutil
root=pathlib.Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser()
parser.add_argument('--output',type=pathlib.Path,required=True)
parser.add_argument('--exe',type=pathlib.Path,default=root/'target/release/mygoimage.exe')
a=parser.parse_args();out=a.output;out.mkdir(parents=True,exist_ok=True)
runs=[]
for count in [10000,100000]:
    photos=out/f'empty-files-{count}';photos.mkdir(exist_ok=True)
    for index in range(count):
        path=photos/f'{index:06}.jpg'
        if not path.exists():path.touch()
    for run in range(3):
        for mode in ['legacy','budgeted']:
            result=out/f'{count}-{run}-{mode}.json'
            command='--benchmark-scan-legacy' if mode=='legacy' else '--benchmark-scan'
            p=subprocess.Popen([str(a.exe.resolve()),command,str(photos),str(result)])
            proc=psutil.Process(p.pid);peak=0;cpu=0;reads=0
            while p.poll() is None:
                try:
                    peak=max(peak,proc.memory_info().rss);c=proc.cpu_times();cpu=c.user+c.system;reads=proc.io_counters().read_bytes
                except psutil.Error:pass
                time.sleep(.01)
            assert p.returncode==0,(mode,p.returncode)
            record=dict(run=run+1,mode=mode,peak_rss_bytes=peak,cpu_seconds=cpu,read_bytes=reads,**json.loads(result.read_text()))
            assert record['entries']==count,record
            runs.append(record);print(json.dumps(record),flush=True)
(out/'results.json').write_text(json.dumps(dict(scope='Empty JPG placeholders; actual controller/SQLite/event-consumer pipeline; each run a fresh index; OS cache not flushed; not photo/GUI timing',runs=runs),indent=2),encoding='utf-8')
