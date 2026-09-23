package io.github.m96chan.tos.tests;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Intent;
import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.os.Bundle;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** Exercises Debian through the same app-private bridge as a visible pane. */
public final class Smoke extends Instrumentation {
    private LocalSocket socket;
    private DataInputStream input;
    private DataOutputStream output;
    private int failed;
    private boolean fresh;
    private boolean previousLogs;
    private int reboots;
    private final String marker = UUID.randomUUID().toString();
    @Override public void onCreate(Bundle args) {
        super.onCreate(args);
        fresh = "yes".equals(args.getString("fresh"));
        previousLogs = "yes".equals(args.getString("previous_logs"));
        reboots = Math.max(1, Math.min(10, Integer.parseInt(args.getString("reboots", "1"))));
        start();
    }
    private void log(String text) { Bundle b = new Bundle(); b.putString("stream", text + "\n"); sendStatus(0, b); }
    private void send(int type, byte[] data) throws IOException {
        output.writeByte(type); output.writeInt(0); output.writeInt(data.length); output.write(data); output.flush();
    }
    private void open() throws Exception {
        Intent intent = new Intent().setClassName("io.github.m96chan.tos", "io.github.m96chan.tos.VmService");
        getTargetContext().startForegroundService(intent);
        IOException last = null;
        for (int i = 0; i < 100; i++) {
            socket = new LocalSocket();
            try { socket.connect(new LocalSocketAddress("io.github.m96chan.tos.vm")); last = null; break; }
            catch (IOException e) { last = e; socket.close(); Thread.sleep(100); }
        }
        if (last != null) throw last;
        socket.setSoTimeout(300000);
        input = new DataInputStream(socket.getInputStream()); output = new DataOutputStream(socket.getOutputStream());
        send('O', new byte[]{0,80,0,24,3,(byte)192,2,64});
    }
    private boolean check(String name, String command) {
        log("RUN " + name);
        try {
            String script = "{ " + command + "; }; tos_result=$?; printf '\\036TOS_RESULT=%s\\037' \"$tos_result\"\n";
            send('D', script.getBytes(StandardCharsets.UTF_8));
            StringBuilder text = new StringBuilder();
            Pattern result = Pattern.compile("\u001eTOS_RESULT=(\\d+)\u001f");
            while (true) {
                int type = input.readUnsignedByte(); input.readInt(); int n = input.readInt();
                if (n < 0 || n > 65536) throw new IOException("Invalid frame length");
                byte[] bytes = new byte[n]; input.readFully(bytes);
                if (type == 'E') throw new EOFException("Guest shell exited");
                if (type != 'D') continue;
                text.append(new String(bytes, StandardCharsets.UTF_8));
                if (text.length() > 1048576) text.delete(0, text.length() - 65536);
                Matcher match = result.matcher(text);
                if (match.find()) {
                    boolean pass = match.group(1).equals("0");
                    if (!pass) failed++;
                    String clean = text.toString().replaceAll("\u001b_[\\s\\S]*?\u001b\\\\", "[image]");
                    log((pass ? "PASS " : "FAIL ") + name + "\n" + clean.substring(Math.max(0, clean.length() - 3000)));
                    return pass;
                }
            }
        } catch (Exception e) { failed++; log("FAIL " + name + ": " + e); return false; }
    }
    private void diagnostics() {
        for (String name : new String[]{"boot.log", "vm-cli.log", "vm-error.log", "network.log", "hypervisor.log"}) {
            try (RandomAccessFile f = new RandomAccessFile(new File(getTargetContext().getFilesDir(), "debian/" + name), "r")) {
                int n = (int)Math.min(12000, f.length()); f.seek(f.length() - n); byte[] bytes = new byte[n]; f.readFully(bytes);
                log(name + ":\n" + new String(bytes, StandardCharsets.UTF_8));
            } catch (Exception e) { log(name + ": " + e); }
        }
    }
    @Override public void onStart() {
        try {
            if (previousLogs) diagnostics();
            if (fresh) {
                File disk = new File(getTargetContext().getFilesDir(), "debian/rootfs.img");
                File backup = new File(disk.getPath() + ".backup-" + System.currentTimeMillis());
                if (disk.exists() && !disk.renameTo(backup)) throw new IOException("Could not preserve previous Debian disk");
                log("Fresh installation; previous disk preserved at " + backup);
            }
            open();
            if (!check("Debian boot", "cat /etc/os-release; getconf GNU_LIBC_VERSION; uname -r; test -f /var/lib/tos-ready")) throw new IOException("Debian did not become ready");
            check("20 GiB disk", "bytes=$(df -B1 --output=size / | tail -n1 | tr -d ' '); test \"$bytes\" -ge 21000000000 && df -h /");
            check("fixture", "cd \"$(mktemp -d /tmp/tos-smoke.XXXXXX)\" && printf 'hello\\n' > example");
            check("Git", "git init -q && git add example && git -c user.name=tOS -c user.email=test@example.invalid commit -qm test && git --no-pager log -1 --format=%s");
            check("Neovim", "nvim --headless -u NONE -i NONE +'lua print(\"TOS_NVIM_OK\")' +qa");
            check("ripgrep/fzf", "rg hello example && printf 'hello\\nother\\n' | fzf --filter hello");
            check("SSH", "ssh -V && ssh-keygen -q -t ed25519 -N '' -f key && ssh-keygen -lf key.pub");
            check("rsync", "rsync example copy && cmp example copy");
            check("manuals", "MANPAGER=cat man bash > manual && test -s manual");
            check("btop/Yazi", "btop --version && yazi --version");
            send('R', new byte[]{0,92,0,31,4,80,2,(byte)232});
            check("PTY resize", "test \"$(stty size)\" = '31 92'");
            LocalSocket first = socket; DataInputStream firstIn = input; DataOutputStream firstOut = output;
            open();
            check("independent second pane", "test \"$PWD\" = /root && test \"$(stty size)\" = '24 80'");
            socket.close(); socket = first; input = firstIn; output = firstOut;
            check("first pane survives second pane close", "test -f example && test \"$(stty size)\" = '31 92'");
            check("HTTPS", "curl --fail --max-time 30 --silent --show-error -o /dev/null https://example.com");
            check("apt update", "apt-get -o APT::Update::Error-Mode=any -o Acquire::Retries=0 -o Acquire::http::Timeout=25 update");
            check("apt install", "apt-get -y --no-install-recommends -o Acquire::http::Timeout=25 install jq && printf '{\"ok\":true}' | jq -e .ok");
            check("persistence write", "printf '%s' '" + marker + "' > /root/.tos-vm-smoke-persist && sync");
            for (int cycle = 0; cycle < reboots; cycle++) {
                getTargetContext().startService(new Intent().setClassName("io.github.m96chan.tos", "io.github.m96chan.tos.VmService").setAction("io.github.m96chan.tos.STOP_VM"));
                socket.setSoTimeout(45000);
                try { while (input.read() != -1) {} } catch (EOFException ignored) {}
                socket.close(); Thread.sleep(2000);
                String shutdown = new String(java.nio.file.Files.readAllBytes(new File(getTargetContext().getFilesDir(), "debian/vm-cli.log").toPath()), StandardCharsets.UTF_8);
                if (!shutdown.contains("VM ended: Shutdown")) throw new IOException("VM did not confirm a clean shutdown: " + shutdown);
                log("PASS clean VM shutdown");
                open();
                check("persistence after VM reboot", "test \"$(cat /root/.tos-vm-smoke-persist)\" = '" + marker + "' && jq --version");
            }
        } catch (Exception e) { failed++; log("FAIL VM test: " + e); }
        finally {
            try { if (socket != null) socket.close(); } catch (Exception ignored) {}
            if (failed > 0) diagnostics();
        }
        Bundle result = new Bundle(); result.putString("stream", "tOS device smoke tests: " + failed + " failures\n");
        finish(failed == 0 ? Activity.RESULT_OK : Activity.RESULT_CANCELED, result);
    }
}
