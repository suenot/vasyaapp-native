#!/usr/bin/env python3
"""Verify packaged binaries without real accounts; optionally measure offline load."""
import argparse, json, os, pathlib, platform, subprocess, tempfile, time
parser=argparse.ArgumentParser()
parser.add_argument('--stress',action='store_true')
args=parser.parse_args()
root=pathlib.Path(__file__).resolve().parent.parent
results={"platform":platform.platform(),"arch":platform.machine(),"date":time.strftime('%Y-%m-%d'),"note":"Programmatic input-to-snapshot and toolkit CPU work; not GPU presentation or physical keyboard latency.","applications":{}}
for gui,title in [('gpui','Vasya GPUI'),('iced','Vasya Iced')]:
    bundle=root/'dist'/f'{title}.app'
    binary=bundle/'Contents/MacOS'/f'vasyaapp-{gui}'
    subprocess.run(['codesign','--verify','--deep','--strict',str(bundle)],check=True)
    resources=bundle/'Contents/Resources'
    subprocess.run([str(resources/'ffmpeg'),'-version'],stdout=subprocess.DEVNULL,check=True)
    subprocess.run([str(resources/'stt-sidecar'),'--help'],stdout=subprocess.DEVNULL,check=True)
    with tempfile.TemporaryDirectory(prefix='vasya-smoke-') as profile:
        env=dict(os.environ,VASYA_NATIVE_DATA_DIR=profile)
        smoke=subprocess.run([str(binary),'--smoke-test'],env=env,text=True,capture_output=True,timeout=25)
        if smoke.returncode:
            raise SystemExit(f'{title} smoke failed: {smoke.stderr}')
        combined=smoke.stdout+smoke.stderr
        if not any(marker in combined for marker in ['VASYA_GPUI_WINDOW_READY','Vasya Iced native window startup: OK']):
            raise SystemExit(f'{title}: missing native window startup marker')
        print(f'{title}: signed package, bundled media tools and native window OK',flush=True)
    if args.stress:
        with tempfile.TemporaryFile(mode='w+') as output:
            process=subprocess.Popen([str(binary),'--stress-test'],stdout=output,stderr=subprocess.STDOUT,text=True)
            time.sleep(15)
            sample=subprocess.run(['ps','-o','rss=,%cpu=','-p',str(process.pid)],capture_output=True,text=True).stdout.strip().split()
            try: code=process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                process.terminate();process.wait(timeout=5);raise SystemExit(f'{title} stress timeout')
            output.seek(0);text=output.read()
        if code: raise SystemExit(f'{title} stress failed: {text}')
        objects=[]
        for line in text.splitlines():
            if '{' in line:
                try: objects.append(json.loads(line[line.index('{'):]))
                except json.JSONDecodeError: pass
        metrics=next((item for item in reversed(objects) if isinstance(item,dict) and ('input_to_snapshot_p95_ms' in item or 'metrics' in item or 'draft_acknowledgment_ms' in item or 'measurements' in item or 'input_to_snapshot' in str(item) or 'draft_ack' in str(item))),None)
        if metrics is None:
            metrics=next((item for item in reversed(objects) if isinstance(item,dict)),None)
        if metrics is None: raise SystemExit(f'{title}: no stress metrics: {text}')
        results['applications'][gui]={"metrics":metrics,"sample_at_seconds":15,"rss_kib":int(sample[0]) if sample else None,"cpu_percent":float(sample[1]) if sample else None}
        print(f'{title}: offline stress completed',flush=True)
if args.stress:
    path=root/'docs/performance.json';path.write_text(json.dumps(results,indent=2)+'\n')
    print(path)
