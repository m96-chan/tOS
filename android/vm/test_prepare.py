#!/usr/bin/env python3
"""The guest binaries the initramfs carries have to be ones Bionic will exec."""
import struct
import unittest
from pathlib import Path
import tempfile

import prepare


def elf(machine=prepare.MACHINE_AARCH64, segments=((prepare.PT_TLS, 64),), magic=b'\x7fELF\x02\x01\x01'):
    header = bytearray(64 + 56 * len(segments))
    header[0:7] = magic
    struct.pack_into('<H', header, 0x12, machine)
    struct.pack_into('<Q', header, 0x20, 64)
    struct.pack_into('<HH', header, 0x36, 56, len(segments))
    for index, (kind, alignment) in enumerate(segments):
        at = 64 + 56 * index
        struct.pack_into('<I', header, at, kind)
        struct.pack_into('<Q', header, at + 0x30, alignment)
    return bytes(header)


class GuestExecutable(unittest.TestCase):
    def check(self, data):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'init'
            path.write_bytes(data)
            return prepare.guest_executable(path)

    def test_a_64_byte_aligned_tls_segment_is_the_binary_back(self):
        data = elf()
        self.assertEqual(self.check(data), data)

    def test_no_tls_segment_at_all_is_fine(self):
        # What the pinned NDK actually produces for init.c and agent.c.
        data = elf(segments=((1, 8),))
        self.assertEqual(self.check(data), data)

    def test_an_8_byte_aligned_tls_segment_is_rejected(self):
        # NDK r27's static libc. v0.0.8 shipped this and the guest kernel
        # panicked on PID 1 before Debian was ever reached.
        with self.assertRaises(ValueError) as caught:
            self.check(elf(segments=((prepare.PT_TLS, 8),)))
        self.assertIn('8-byte-aligned TLS segment', str(caught.exception))

    def test_a_host_binary_is_rejected(self):
        with self.assertRaises(ValueError) as caught:
            self.check(elf(machine=62))
        self.assertIn('not an ARM64 binary', str(caught.exception))

    def test_something_that_is_not_an_elf_is_rejected(self):
        with self.assertRaises(ValueError) as caught:
            self.check(elf(magic=b'#!/bin/'))
        self.assertIn('not a 64-bit little-endian ELF', str(caught.exception))


if __name__ == '__main__':
    unittest.main()
