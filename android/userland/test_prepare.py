"""Exercise the APK code/data boundary using small local Debian archives."""
import hashlib
import io
import json
from pathlib import Path
import struct
import subprocess
import tarfile
import tempfile
import unittest
import zipfile

import prepare


def elf(alignment=16384):
    data = bytearray(120)
    data[:6] = b'\x7fELF\x02\x01'
    struct.pack_into('<H', data, 18, 183)
    struct.pack_into('<Q', data, 32, 64)
    struct.pack_into('<HH', data, 54, 56, 1)
    struct.pack_into('<I', data, 64, 1)
    struct.pack_into('<Q', data, 112, alignment)
    return bytes(data)


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.lock = {'packages': [{'Package': 'fixture', 'Version': '1', 'Homepage': 'https://example.invalid'}],
                     'recipe_commit': '0' * 40, 'font': {'member': 'font.ttf'}}
        self.font = self.root / 'font.zip'
        with zipfile.ZipFile(self.font, 'w') as archive:
            archive.writestr('font.ttf', b'font fixture')

    def assemble(self, entries, output='out'):
        tar_path = self.root / 'data.tar.xz'
        with tarfile.open(tar_path, 'w:xz') as archive:
            for name, value in entries:
                member = tarfile.TarInfo(prepare.PREFIX + name)
                if isinstance(value, str):
                    member.type = tarfile.SYMTYPE
                    member.linkname = value
                    archive.addfile(member)
                else:
                    member.size = len(value)
                    archive.addfile(member, io.BytesIO(value))
        deb = self.root / 'fixture.deb'
        subprocess.run(['ar', 'rc', str(deb), str(tar_path)], check=True)
        result = self.root / output
        prepare.assemble(self.lock, [deb], self.font, result)
        return result

    def test_native_code_is_outside_writable_assets_and_bundle_is_reproducible(self):
        entries = [('bin/tool', elf()), ('bin/alias', 'tool'), ('share/data', b'hello')]
        first = self.assemble(entries)
        second = self.assemble(entries, 'second')
        bundle = first / 'assets/userland.zip'
        self.assertEqual(bundle.read_bytes(), (second / 'assets/userland.zip').read_bytes())
        self.assertEqual(hashlib.sha256(bundle.read_bytes()).hexdigest(), (first / 'assets/userland.id').read_text().strip())
        native = list((first / 'lib/arm64-v8a').glob('*.so'))
        self.assertEqual(len(native), 1)
        self.assertEqual(native[0].read_bytes(), elf())
        with zipfile.ZipFile(bundle) as archive:
            self.assertNotIn('bin/tool', archive.namelist())
            self.assertTrue(all(not archive.read(n).startswith(b'\x7fELF') for n in archive.namelist()))
            links = archive.read('share/tos/links.tsv').decode()
            self.assertIn(f'N\tbin/tool\t{native[0].name}\n', links)
            self.assertIn('S\tbin/alias\ttool\n', links)
            self.assertEqual(archive.read('share/data'), b'hello')
            self.assertEqual(json.loads(archive.read('share/tos/packages.json')), self.lock)

    def test_rejects_path_traversal_and_link_record_injection(self):
        for name in ('../escape', 'bin/../../escape', 'bin/tool\tN', 'bin/tool\nN'):
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.assemble([(name, b'data')])

    def test_build_time_elf_objects_are_not_installed_as_executables(self):
        relocatable = bytearray(elf())
        struct.pack_into('<H', relocatable, 16, 1)  # ET_REL has no load segments.
        struct.pack_into('<HH', relocatable, 54, 0, 0)
        result = self.assemble([('lib/build.o', bytes(relocatable)), ('lib/runtime.so', elf())])
        self.assertEqual(len(list((result / 'lib/arm64-v8a').glob('*.so'))), 1)
        with zipfile.ZipFile(result / 'assets/userland.zip') as archive:
            self.assertNotIn('lib/build.o', archive.namelist())
            self.assertNotIn('build.o', archive.read('share/tos/links.tsv').decode())

    def test_rejects_links_outside_prefix(self):
        for target in ('../../escape', '/etc/passwd', '/' + prepare.PREFIX + '../escape', 'tool\nN'):
            with self.subTest(target=target), self.assertRaises(ValueError):
                self.assemble([('bin/link', target)])

    def test_rejects_wrong_architecture_and_page_alignment(self):
        wrong_arch = bytearray(elf())
        struct.pack_into('<H', wrong_arch, 18, 62)
        for data in (b'\x7fELF', bytes(wrong_arch), elf(4096)):
            with self.subTest(data=data[:20]), self.assertRaises(ValueError):
                self.assemble([('bin/tool', data)])


if __name__ == '__main__':
    unittest.main()
