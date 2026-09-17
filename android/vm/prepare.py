#!/usr/bin/env python3
"""Build the small AVF initramfs and expose host networking headers/library."""
from pathlib import Path
import struct
import zipfile
import shutil

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'dist/android/vm'
MACHINE_AARCH64 = 183
PT_TLS = 7
TLS_ALIGNMENT = 64


# Bionic lays an ARM64 thread's TLS block against a 64-byte-aligned control
# block, and refuses to start an executable whose own TLS segment is aligned
# more loosely than that. NDK r27's static libc contributes an 8-byte-aligned
# one, and the guest's PID 1 dies on exec before it can say anything: the
# kernel panics on it, the CLI reports `VM ended: Reboot`, and the APK carries
# a Debian that never initialises (v0.0.8, built on a runner whose default NDK
# was r27 rather than the one the phone was tested with). These two binaries
# are the guest's init and agent, so read them through here.
def guest_executable(path):
    data = path.read_bytes()
    if data[:7] != b'\x7fELF\x02\x01\x01':
        raise ValueError(f'{path.name} is not a 64-bit little-endian ELF')
    machine, = struct.unpack_from('<H', data, 0x12)
    if machine != MACHINE_AARCH64:
        raise ValueError(f'{path.name} is not an ARM64 binary (e_machine {machine})')
    offset, = struct.unpack_from('<Q', data, 0x20)
    size, count = struct.unpack_from('<HH', data, 0x36)
    for index in range(count):
        header = offset + index * size
        kind, = struct.unpack_from('<I', data, header)
        alignment, = struct.unpack_from('<Q', data, header + 0x30)
        if kind == PT_TLS and alignment < TLS_ALIGNMENT:
            raise ValueError(
                f'{path.name} has a {alignment}-byte-aligned TLS segment, and '
                f'Bionic needs {TLS_ALIGNMENT}: the guest would panic on PID 1. '
                'Build it with the NDK android/README.md pins, not an older one.')
    return data


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
    entry('init', guest_executable(OUT / 'init'), 0o100755)
    entry('tos', mode=0o040755)
    entry('tos/agent', guest_executable(OUT / 'agent'), 0o100755)
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
