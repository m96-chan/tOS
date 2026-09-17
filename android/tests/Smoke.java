package io.github.m96chan.tos.tests;

import android.app.Activity;
import android.app.Instrumentation;
import android.os.Bundle;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.Map;
import java.util.concurrent.Executors;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

/** Device tests run as the target app UID; no screen unlock or root required. */
public final class Smoke extends Instrumentation {
    private File home, prefix;
    private int failed;
    @Override public void onCreate(Bundle args) { super.onCreate(args); start(); }
    private void check(String name, String command) {
        Process process = null;
        ExecutorService reader = Executors.newSingleThreadExecutor();
        try {
            ProcessBuilder builder = new ProcessBuilder(new File(prefix, "bin/bash").getPath(), "--noprofile", "--norc", "-c",
                "export TOS_MOTD_SHOWN=1; . \"$PREFIX/etc/tos/bashrc\"; " + command);
            builder.directory(home).redirectErrorStream(true);
            Map<String, String> env = builder.environment();
            env.put("HOME", home.getPath()); env.put("TMPDIR", new File(home, "tmp").getPath());
            env.put("PREFIX", prefix.getPath()); env.put("PATH", new File(prefix, "bin").getPath() + ":/system/bin");
            env.put("LD_LIBRARY_PATH", new File(prefix, "lib").getPath());
            env.put("LD_PRELOAD", new File(prefix, "lib/libtos_paths.so").getPath());
            env.put("LANG", "C.UTF-8");
            process = builder.start();
            final InputStream input = process.getInputStream();
            Future<String> output = reader.submit(() -> {
                ByteArrayOutputStream bytes = new ByteArrayOutputStream();
                byte[] buffer = new byte[4096]; int n;
                while ((n = input.read(buffer)) != -1) {
                    if (bytes.size() < 32768) bytes.write(buffer, 0, Math.min(n, 32768 - bytes.size()));
                }
                return new String(bytes.toByteArray(), StandardCharsets.UTF_8);
            });
            String text = output.get(40, TimeUnit.SECONDS);
            int status = process.waitFor();
            Bundle result = new Bundle();
            result.putString("stream", (status == 0 ? "PASS " : "FAIL ") + name + " (" + status + ")\n" + text + "\n");
            sendStatus(0, result);
            if (status != 0) failed++;
        } catch (Exception error) {
            failed++;
            Bundle result = new Bundle(); result.putString("stream", "FAIL " + name + ": " + error + "\n"); sendStatus(0, result);
        } finally {
            if (process != null) process.destroyForcibly();
            reader.shutdownNow();
        }
    }
    @Override public void onStart() {
        home = new File(getTargetContext().getCacheDir(), "userland-smoke-" + System.nanoTime());
        new File(home, "tmp").mkdirs();
        prefix = new File(getTargetContext().getFilesDir(), "usr");
        check("bash", "bash --version");
        check("git", "git --version && git init -q && printf 'hello\\n' > example && git add example && git -c user.name=tOS -c user.email=test@example.invalid commit -qm test && git log -1 --format=%s");
        check("curl", "curl --version && test -s \"$SSL_CERT_FILE\"");
        check("neovim", "nvim --headless -u NONE -i NONE +'lua print(\"TOS_NVIM_OK\")' +qa");
        check("ripgrep", "rg --version && rg hello example");
        check("fzf", "printf 'hello\\nother\\n' | fzf --filter hello");
        check("openssh", "ssh -V && ssh-keygen -q -t ed25519 -N '' -f key && ssh-keygen -lf key.pub");
        check("rsync", "rsync --version && rsync example copy && cmp example copy");
        check("unzip/file", "unzip -v && file example");
        check("manuals", "MANPAGER=cat man bash > manual.txt && test -s manual.txt");
        check("yazi", "yazi --version");
        check("htop", "htop --version");
        check("writable script", "printf '#!/data/data/com.termux/files/usr/bin/bash\\nprintf SCRIPT_OK\\n' > probe && chmod +x probe && ./probe");
        check("TLS", "curl --fail --max-time 25 --silent --show-error -o /dev/null https://example.com");
        Bundle result = new Bundle();
        result.putString("stream", "tOS device smoke tests: " + failed + " failures\n");
        finish(failed == 0 ? Activity.RESULT_OK : Activity.RESULT_CANCELED, result);
    }
}
