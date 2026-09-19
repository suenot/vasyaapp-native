#!/usr/bin/env python3
"""Copy ffmpeg and non-system dylibs into a relocatable macOS app resource dir."""
import pathlib, shutil, subprocess, sys
resources = pathlib.Path(sys.argv[1]).resolve()
source = shutil.which('ffmpeg')
if not source:
    raise SystemExit('ffmpeg is required for audio conversion; install it before packaging')
libs = resources / 'lib'
libs.mkdir(parents=True, exist_ok=True)
queue = [(pathlib.Path(source).resolve(), resources / 'ffmpeg')]
seen = set()
while queue:
    origin, target = queue.pop(0)
    if origin in seen:
        continue
    seen.add(origin)
    shutil.copy2(origin, target)
    target.chmod(0o755)
    deps = subprocess.check_output(['otool', '-L', str(origin)], text=True).splitlines()[1:]
    for entry in deps:
        dependency = entry.strip().split(' (', 1)[0]
        if not dependency.startswith('/') or dependency.startswith(('/usr/lib/', '/System/Library/')):
            continue
        dep = pathlib.Path(dependency).resolve()
        if dep == origin:
            continue
        new = '@loader_path/' + ('lib/' if target.parent == resources else '') + dep.name
        subprocess.run(['install_name_tool', '-change', dependency, new, str(target)], check=True)
        queue.append((dep, libs / dep.name))
    if target.suffix == '.dylib':
        subprocess.run(['install_name_tool', '-id', '@loader_path/' + target.name, str(target)], check=True)
    subprocess.run(['codesign', '--force', '--sign', '-', str(target)], check=True, capture_output=True)
(resources / 'ffmpeg-source.txt').write_text('FFmpeg and its linked Homebrew libraries.\nBuild/source: https://formulae.brew.sh/formula/ffmpeg\nFFmpeg source and license: https://ffmpeg.org/download.html\n' + '\n'.join(str(p) for p in sorted(seen)) + '\n')
