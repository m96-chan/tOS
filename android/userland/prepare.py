#!/usr/bin/env python3
"""Pin Android packages and package their ELF code separately from writable data."""
import argparse
import concurrent.futures
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import posixpath
import shutil
import struct
import subprocess
import tarfile
import zipfile

ROOT = Path(__file__).resolve().parents[2]
LOCK = ROOT / 'android/userland/packages.lock.json'
REPO = 'https://packages.termux.dev/apt/termux-main/'
PREFIX = 'data/data/com.termux/files/usr/'
SEEDS = ['bash', 'git', 'curl', 'less', 'neovim', 'ripgrep', 'fzf', 'openssh',
         'rsync', 'unzip', 'file', 'mandoc', 'yazi', 'htop', 'coreutils',
         'findutils', 'grep', 'sed', 'gawk', 'tar', 'gzip', 'bzip2', 'xz-utils',
         'ca-certificates', 'procps', 'util-linux', 'diffutils', 'libslirp']
# This package sets up the Termux app and package manager, not a library.
# tOS supplies its own installation, shell setup and executable locations.
REPLACED = {'termux-tools'}


def parse_index(text):
    packages = {}
    for stanza in text.split('\n\n'):
        fields = {}
        for line in stanza.splitlines():
            if ': ' in line and not line.startswith(' '):
                key, value = line.split(': ', 1)
                fields[key] = value
        if 'Package' in fields:
            packages[fields['Package']] = fields
    return packages


def pin(index, recipe_commit):
    packages = parse_index(index.read_text())
    selected = {}
    pending = list(SEEDS)
    while pending:
        name = pending.pop()
        if name in selected or name in REPLACED:
            continue
        if name not in packages:
            raise ValueError(f'Unresolved package: {name}')
        info = packages[name]
        selected[name] = {key: info.get(key, '') for key in
                          ['Package', 'Version', 'Architecture', 'Filename', 'SHA256',
                           'Size', 'Homepage', 'Depends', 'Description']}
        for relation in filter(None, (info.get('Pre-Depends', '') + ',' + info.get('Depends', '')).split(',')):
            alternatives = [re.split(r'[ (:]', part.strip())[0] for part in relation.split('|')]
            choice = next((n for n in alternatives if n in selected), None)
            choice = choice or next((n for n in alternatives if n in packages or n in REPLACED), None)
            if choice is None:
                raise ValueError(f'{name}: unresolved dependency {relation}')
            pending.append(choice)
    font = json.loads(LOCK.read_text())['font']
    LOCK.write_text(json.dumps({'repository': REPO, 'architecture': 'aarch64',
        'recipe_commit': recipe_commit,
        'requested': SEEDS, 'replaced': sorted(REPLACED),
        'packages': [selected[n] for n in sorted(selected)], 'font': font}, indent=2) + '\n')
    print(f'Pinned {len(selected)} packages, {sum(int(p["Size"]) for p in selected.values()) / 1048576:.1f} MiB compressed')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def download(package, repository, cache):
    target = cache / (package['SHA256'] + '.deb')
    if target.exists() and digest(target) == package['SHA256']:
        return target
    url = repository + package['Filename']
    temp = target.with_suffix('.part')
    subprocess.run(['curl', '--fail', '--location', '--retry', '3', '--connect-timeout', '20',
                    '--silent', '--show-error', '--output', str(temp), url], check=True)
    if digest(temp) != package['SHA256']:
        temp.unlink()
        raise ValueError(f'Checksum mismatch: {package["Package"]}')
    temp.replace(target)
    return target


def safe_name(name):
    path = PurePosixPath(name)
    if path.is_absolute() or '..' in path.parts or not path.parts or any(c in name for c in '\t\r\n\0'):
        raise ValueError(f'Unsafe archive path: {name}')
    return path.as_posix()


def validate_elf(data, name):
    if len(data) < 64 or data[4:6] != b'\x02\x01' or int.from_bytes(data[18:20], 'little') != 183:
        raise ValueError(f'Not an ARM64 little-endian ELF: {name}')
    offset = struct.unpack_from('<Q', data, 32)[0]
    size, count = struct.unpack_from('<HH', data, 54)
    if size < 56 or not count or offset + size * count > len(data):
        raise ValueError(f'Invalid ELF program headers: {name}')
    for i in range(count):
        header = offset + i * size
        if struct.unpack_from('<I', data, header)[0] == 1:
            alignment = struct.unpack_from('<Q', data, header + 48)[0]
            if alignment < 16384:
                raise ValueError(f'ELF lacks 16 KiB alignment: {name}')


def members(deb):
    """Read the data tar from a Debian ar archive; never execute maintainer scripts."""
    listing = subprocess.check_output(['ar', 't', str(deb)], text=True).splitlines()
    data = next(n for n in listing if n.startswith('data.tar'))
    stream = io.BytesIO(subprocess.check_output(['ar', 'p', str(deb), data]))
    return tarfile.open(fileobj=stream, mode='r:*')


def assemble(lock, archives, font_archive, output):
    if output.exists():
        shutil.rmtree(output)
    native = output / 'lib/arm64-v8a'
    assets = output / 'assets'
    native.mkdir(parents=True)
    assets.mkdir()
    files, links = {}, {}
    # Links are recorded, never followed while unpacking untrusted archives.
    for package, deb in zip(lock['packages'], archives):
        with members(deb) as archive:
            for entry in archive:
                name = entry.name.removeprefix('./')
                if not name.startswith(PREFIX):
                    continue
                relative = name[len(PREFIX):].rstrip('/')
                if not relative or entry.isdir():
                    continue
                relative = safe_name(relative)
                if entry.issym() or entry.islnk():
                    target = entry.linkname
                    if any(c in target for c in '\t\r\n\0'):
                        raise ValueError(f'Invalid symlink target: {relative}')
                    if entry.islnk():
                        target = target.removeprefix('./')
                        if target.startswith(PREFIX):
                            target = '/' + target
                    if target.startswith('/' + PREFIX):
                        target = '@PREFIX@/' + target[len(PREFIX) + 1:]
                        safe_name(target[len('@PREFIX@/'):])
                    elif target.startswith('/') and target != '/system/bin/sh':
                        raise ValueError(f'Unexpected absolute symlink: {relative} -> {target}')
                    if not target.startswith(('/', '@PREFIX@/')):
                        resolved = posixpath.normpath(posixpath.join(posixpath.dirname(relative), target))
                        if resolved == '..' or resolved.startswith('../'):
                            raise ValueError(f'Symlink escapes prefix: {relative} -> {target}')
                    links[relative] = ('S', target)
                    files.pop(relative, None)
                elif entry.isfile():
                    if entry.size > 256 * 1024 * 1024:
                        raise ValueError(f'Oversize entry: {relative}')
                    files[relative] = archive.extractfile(entry).read()
                    links.pop(relative, None)
                else:
                    raise ValueError(f'Unsupported archive entry: {relative}')
    with zipfile.ZipFile(assets / 'userland.zip', 'w', compression=zipfile.ZIP_DEFLATED) as bundle:
        def write(name, data):
            entry = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            entry.compress_type = zipfile.ZIP_DEFLATED
            entry.external_attr = 0o100644 << 16
            bundle.writestr(entry, data)
        for name, data in sorted(files.items()):
            if name in ('bin/nvim', 'share/man/mandoc.db'):
                continue  # Replaced with a relocatable LuaJIT launcher below.
            if data.startswith(b'\x7fELF'):
                if len(data) >= 18 and int.from_bytes(data[16:18], 'little') == 1:
                    continue  # Build-time relocatable objects are not runtime programs.
                validate_elf(data, name)
                filename = 'libtos_pkg_' + hashlib.sha256(name.encode()).hexdigest()[:24] + '.so'
                (native / filename).write_bytes(data)
                links[name] = ('N', filename)
            else:
                write(name, data)
        write('share/tos/splash.png', (ROOT / 'compositor/tos-compositor/assets/splash.png').read_bytes())
        write('share/tos/motd_ascii', (ROOT / '.motd_ascii').read_bytes())
        write('share/tos/packages.json', json.dumps(lock, indent=2))
        write('etc/tos/bashrc', (ROOT / 'android/userland/bashrc').read_bytes())
        with zipfile.ZipFile(font_archive) as font:
            write('share/fonts/HackGenConsoleNF-Regular.ttf', font.read(lock['font']['member']))
        write('share/doc/hackgen/LICENSE', (ROOT / 'android/userland/licenses/HackGen.txt').read_bytes())
        # The upstream launcher preloads LuaJIT using its original absolute
        # prefix, before any libc path hooks can run in the new process.
        write('bin/nvim', '#!/system/bin/sh\nLD_PRELOAD="$LD_PRELOAD:$PREFIX/lib/libluajit.so" exec "$PREFIX/libexec/nvim/nvim" "$@"\n')
        provenance = '\n'.join(f'{p["Package"]} {p["Version"]}\n{p["Homepage"]}\n'
            f'https://github.com/termux/termux-packages/tree/{lock["recipe_commit"]}/packages/{p["Package"]}\n'
            for p in lock['packages'])
        write('share/doc/tos/PACKAGE-SOURCES.txt', provenance)
        # These programs are built by build-apk.sh and signed alongside the packages.
        links['bin/tos-motd'] = ('N', 'libtos_motd.so')
        links['lib/libtos_paths.so'] = ('N', 'libtos_paths.so')
        links['bin/tos-vmclient'] = ('N', 'libtos_vmclient.so')
        if 'bin/sh' not in links and 'bin/sh' not in files:
            links['bin/sh'] = ('S', 'bash')
        write('share/tos/links.tsv', ''.join(
            f'{kind}\t{name}\t{target}\n' for name, (kind, target) in sorted(links.items())))
    (assets / 'userland.id').write_text(digest(assets / 'userland.zip') + '\n')
    print(f'Prepared {len(links)} links and {len(files)} files')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pin', type=Path, metavar='PACKAGES_INDEX')
    parser.add_argument('--recipe-commit', help='Termux recipe commit corresponding to the new package index')
    parser.add_argument('--output', type=Path, default=ROOT / 'dist/android/userland')
    args = parser.parse_args()
    if args.pin:
        if not args.recipe_commit or not re.fullmatch(r'[a-f0-9]{40}', args.recipe_commit):
            parser.error('--pin requires --recipe-commit with a full source commit hash')
        pin(args.pin, args.recipe_commit)
        return
    lock = json.loads(LOCK.read_text())
    cache = ROOT / 'dist/android/package-cache'
    cache.mkdir(parents=True, exist_ok=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
        archives = list(pool.map(lambda p: download(p, lock['repository'], cache), lock['packages']))
    font = cache / 'hackgen.zip'
    if not font.exists() or digest(font) != lock['font']['sha256']:
        subprocess.run(['curl', '--fail', '--location', '--retry', '3', '--silent', '--show-error',
                        '--output', str(font), lock['font']['url']], check=True)
    if digest(font) != lock['font']['sha256']:
        raise ValueError('HackGen checksum mismatch')
    assemble(lock, archives, font, args.output)


if __name__ == '__main__':
    main()
