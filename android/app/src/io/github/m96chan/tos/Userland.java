package io.github.m96chan.tos;

import android.content.Context;
import android.system.Os;
import java.io.BufferedReader;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;

/** Install signed assets as data, and link ELF tools to Android's native library directory. */
final class Userland {
    private Userland() {}
    private static String read(InputStream input) throws Exception {
        try (InputStream stream = input) {
            java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
            byte[] buffer = new byte[8192];
            int length;
            while ((length = stream.read(buffer)) != -1) bytes.write(buffer, 0, length);
            return new String(bytes.toByteArray(), StandardCharsets.UTF_8);
        }
    }
    private static File child(File root, String path) throws Exception {
        if (path.isEmpty() || path.startsWith("/")) throw new java.io.IOException("Invalid bundled path");
        File file = new File(root, path);
        if (!file.getCanonicalPath().startsWith(root.getCanonicalPath() + "/"))
            throw new java.io.IOException("Bundled path escapes prefix: " + path);
        return file;
    }
    private static void link(String target, File path) throws Exception {
        path.getParentFile().mkdirs();
        File temp = new File(path.getPath() + ".tos-link");
        temp.delete();
        Os.symlink(target, temp.getPath());
        Os.rename(temp.getPath(), path.getPath());
    }
    static File prepare(Context context) throws Exception {
        String id = read(context.getAssets().open("userland.id")).trim();
        if (!id.matches("[a-f0-9]{64}")) throw new java.io.IOException("Invalid bundle version");
        File root = new File(context.getFilesDir(), "runtime/" + id);
        File ready = new File(root, ".complete");
        if (!ready.isFile()) {
            root.mkdirs();
            // Reject traversal and bound expansion, even though assets are signed.
            long total = 0;
            try (ZipInputStream zip = new ZipInputStream(context.getAssets().open("userland.zip"))) {
                ZipEntry entry;
                byte[] buffer = new byte[65536];
                while ((entry = zip.getNextEntry()) != null) {
                    File target = child(root, entry.getName());
                    if (entry.isDirectory()) { target.mkdirs(); continue; }
                    target.getParentFile().mkdirs();
                    try (FileOutputStream out = new FileOutputStream(target)) {
                        int count;
                        while ((count = zip.read(buffer)) != -1) {
                            total += count;
                            if (total > 512L * 1024 * 1024) throw new java.io.IOException("Bundle exceeds budget");
                            out.write(buffer, 0, count);
                        }
                    }
                    Os.chmod(target.getPath(), entry.getName().startsWith("bin/") || entry.getName().startsWith("libexec/") ? 0755 : 0644);
                }
            }
        }
        String nativeDir = context.getApplicationInfo().nativeLibraryDir;
        // Android changes nativeLibraryDir on update. Refresh links on every start,
        // including updates that do not change the data archive.
        try (BufferedReader lines = new BufferedReader(new InputStreamReader(
                new FileInputStream(new File(root, "share/tos/links.tsv")), StandardCharsets.UTF_8))) {
            String line;
            while ((line = lines.readLine()) != null) {
                String[] parts = line.split("\t", -1);
                if (parts.length != 3) throw new java.io.IOException("Invalid bundle link");
                // Validate lexically: an existing ELF symlink intentionally resolves
                // outside root into Android's read-only nativeLibraryDir.
                if (parts[1].startsWith("/") || parts[1].contains("../")) throw new java.io.IOException("Invalid link path");
                File destination = new File(root, parts[1]);
                String target;
                if (parts[0].equals("N") && parts[2].matches("lib[a-zA-Z0-9_]+\\.so")) {
                    target = nativeDir + "/" + parts[2];
                    if (!new File(target).isFile()) throw new java.io.IOException("Missing bundled executable: " + parts[1]);
                } else if (parts[0].equals("S")) {
                    target = parts[2].replace("@PREFIX@", root.getAbsolutePath());
                    if (!target.equals("/system/bin/sh")) {
                        File resolved = target.startsWith("/") ? new File(target) : new File(destination.getParentFile(), target);
                        // Normalize without following other package symlinks.
                        String normalized = resolved.toURI().normalize().getPath();
                        if (!normalized.startsWith(root.getAbsolutePath() + "/")) throw new java.io.IOException("Link escapes prefix");
                    }
                } else throw new java.io.IOException("Invalid bundle link type");
                link(target, destination);
            }
        }
        ready.createNewFile();
        File prefix = new File(context.getFilesDir(), "usr");
        link(root.getAbsolutePath(), prefix);
        File home = new File(context.getFilesDir(), "home");
        new File(home, "tmp").mkdirs();
        File bashrc = new File(home, ".bashrc");
        if (!bashrc.exists()) {
            try (FileOutputStream out = new FileOutputStream(bashrc)) {
                out.write("# tOS defaults; add your settings below.\n. \"$PREFIX/etc/tos/bashrc\"\n".getBytes(StandardCharsets.UTF_8));
            }
        }
        return prefix;
    }
}
