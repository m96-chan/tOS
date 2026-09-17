#!/usr/bin/env python3
"""Build the small AVF initramfs and expose host networking headers/library."""
from pathlib import Path
import zipfile
import shutil

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'dist/android/vm'


def main():
    assets = OUT / 'assets/vm'
    assets.mkdir(parents=True, exist_ok=True)
    archive = bytearray()

    def entry(name, data=b'', mode=0o100644, major=0, minor=0):
        fields = [1, mode, 0, 0, 1, 0, len(data), 0, 0, major, minor, len(name) + 1, 0]
        archive.extend(('070701' + ''.join(f'{v:08x}' for v in fields)).encode())
        archive.extend(name.encode() + b'\0')
        archive.extend(b'\0' * (-len(archive) % 4))
        archive.extend(data)
        archive.extend(b'\0' * (-len(archive) % 4))

    entry('dev', mode=0o040755)
    entry('dev/console', mode=0o020600, major=5, minor=1)
    entry('init', (OUT / 'init').read_bytes(), 0o100755)
    entry('tos', mode=0o040755)
    entry('tos/agent', (OUT / 'agent').read_bytes(), 0o100755)
    for name in ('boot.sh', 'bashrc'):
        entry('tos/' + name, (ROOT / 'android/vm' / name).read_bytes())
    entry('TRAILER!!!', mode=0)
    (assets / 'initrd.cpio').write_bytes(archive)
    shutil.copyfile(OUT / 'kernel/vmlinuz', assets / 'vmlinuz')
    (assets / 'debian.sha256').write_text((OUT / 'debian.img.sha256').read_text().split()[0] + '\n')
    with zipfile.ZipFile(ROOT / 'dist/android/userland/assets/userland.zip') as bundle:
        for name in bundle.namelist():
            if name.startswith('include/slirp/') and name.endswith('.h'):
                target = OUT / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(bundle.read(name))
        for line in bundle.read('share/tos/links.tsv').decode().splitlines():
            kind, name, target = line.split('\t')
            if kind == 'N' and name.startswith('lib/libslirp.so'):
                (OUT / 'slirp-library').write_text(str(ROOT / 'dist/android/userland/lib/arm64-v8a' / target))
                break
        else:
            raise ValueError('libslirp is missing')


if __name__ == '__main__':
    main()
