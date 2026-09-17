package io.github.m96chan.tos;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.os.Build;
import android.os.IBinder;
import android.os.Process;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.security.MessageDigest;
import java.util.Map;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.zip.GZIPInputStream;
import org.json.JSONArray;
import org.json.JSONObject;

/** Owns the Debian disk, AVF process and per-pane guest sessions. */
public final class VmService extends Service {
    static final String SOCKET = "io.github.m96chan.tos.vm";
    static volatile String lastFailure;
    private static final String CHANNEL = "debian";
    private static final String STOP = "io.github.m96chan.tos.STOP_VM";
    private static final int MAX_FRAME = 65536;
    private final ExecutorService workers = Executors.newCachedThreadPool();
    private final ConcurrentHashMap<Integer, Client> clients = new ConcurrentHashMap<>();
    private final AtomicInteger nextId = new AtomicInteger(1);
    private final CompletableFuture<Void> ready = new CompletableFuture<>();
    private final ArrayBlockingQueue<Frame> networkPackets = new ArrayBlockingQueue<>(256);
    private volatile boolean stopping, ended;
    private volatile String status = "Starting Debian…";
    private LocalServerSocket server;
    private java.lang.Process vm, network;
    private DataOutputStream guestInput;
    private final Object guestWrite = new Object();

    static String unavailable(Context context) {
        if (Build.VERSION.SDK_INT < 35 || !context.getPackageManager().hasSystemFeature("android.software.virtualization_framework"))
            return "This version of tOS requires an AVF-capable Android 15+ device.";
        for (String name : new String[]{"MANAGE_VIRTUAL_MACHINE", "USE_CUSTOM_VIRTUAL_MACHINE"})
            if (context.checkSelfPermission("android.permission." + name) != PackageManager.PERMISSION_GRANTED)
                return "One-time setup from your computer is required:\n\nadb shell pm grant io.github.m96chan.tos android.permission.MANAGE_VIRTUAL_MACHINE\n\nadb shell pm grant io.github.m96chan.tos android.permission.USE_CUSTOM_VIRTUAL_MACHINE\n\nThen reopen tOS.";
        return null;
    }
    static void start(Context context) {
        Intent intent = new Intent(context, VmService.class);
        if (Build.VERSION.SDK_INT >= 26) context.startForegroundService(intent); else context.startService(intent);
    }
    static void shutdown(Context context) { context.startService(new Intent(context, VmService.class).setAction(STOP)); }

    @Override public void onCreate() {
        super.onCreate();
        lastFailure = null;
        NotificationManager manager = getSystemService(NotificationManager.class);
        manager.createNotificationChannel(new NotificationChannel(CHANNEL, "Debian VM", NotificationManager.IMPORTANCE_LOW));
        startForeground(1, notification());
        try {
            server = new LocalServerSocket(SOCKET);
            workers.execute(this::acceptClients);
            workers.execute(this::launch);
        } catch (Exception error) { fail(error); }
    }
    @Override public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && STOP.equals(intent.getAction())) {
            stopping = true; update("Shutting down Debian…");
            workers.execute(() -> {
                try {
                    ready.get(5, TimeUnit.MINUTES); send(new Frame('Q', 0, new byte[0]));
                    if (!vm.waitFor(45, TimeUnit.SECONDS)) fail(new IOException("Debian did not shut down within 45 seconds"));
                }
                catch (Exception error) { fail(error); }
            });
        }
        return START_NOT_STICKY;
    }
    @Override public IBinder onBind(Intent intent) { return null; }
    private Notification notification() {
        PendingIntent open = PendingIntent.getActivity(this, 0, new Intent(this, MainActivity.class), PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        PendingIntent stop = PendingIntent.getService(this, 1, new Intent(this, VmService.class).setAction(STOP), PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        return new Notification.Builder(this, CHANNEL).setSmallIcon(android.R.drawable.sym_def_app_icon)
            .setContentTitle("tOS · Debian").setContentText(status).setContentIntent(open).setOngoing(true)
            .addAction(new Notification.Action.Builder(null, "Shut down", stop).build()).build();
    }
    private void update(String text) {
        status = text;
        getSystemService(NotificationManager.class).notify(1, notification());
    }
    private void acceptClients() {
        try {
            while (!ended) {
                LocalSocket socket = server.accept();
                if (ended) { socket.close(); break; }
                if (socket.getPeerCredentials().getUid() != Process.myUid()) { socket.close(); continue; }
                Client client = new Client(nextId.getAndIncrement(), socket);
                clients.put(client.id, client); workers.execute(client::serve); workers.execute(client::write);
            }
        } catch (Exception error) { if (!ended) fail(error); }
    }
    private void copyAsset(String name, File target) throws Exception {
        File temporary = new File(target.getPath() + ".new");
        try (InputStream in = getAssets().open(name); OutputStream out = new FileOutputStream(temporary)) {
            byte[] b = new byte[65536]; int n; while ((n = in.read(b)) != -1) out.write(b, 0, n);
        }
        android.system.Os.rename(temporary.getPath(), target.getPath());
    }
    private File prepareDisk() throws Exception {
        File directory = new File(getFilesDir(), "debian"); directory.mkdirs();
        File disk = new File(directory, "rootfs.img");
        if (!disk.exists()) {
            if (directory.getUsableSpace() < 3L * 1024 * 1024 * 1024 + 64L * 1024 * 1024)
                throw new IOException("At least 3.1 GiB of free storage is required for Debian");
            update("Preparing Debian disk…");
            File temporary = new File(directory, "rootfs.img.new");
            MessageDigest digest = MessageDigest.getInstance("SHA-256"); long total = 0;
            try (InputStream in = new GZIPInputStream(getAssets().open("vm/debian.img.gz"), 65536);
                 FileOutputStream out = new FileOutputStream(temporary)) {
                byte[] b = new byte[262144]; int n;
                while ((n = in.read(b)) != -1) {
                    total += n; if (total > 3L * 1024 * 1024 * 1024) throw new IOException("Debian image exceeds limit");
                    digest.update(b, 0, n); out.write(b, 0, n);
                }
                out.getFD().sync();
            }
            StringBuilder hash = new StringBuilder(); for (byte b : digest.digest()) hash.append(String.format("%02x", b & 255));
            String expected;
            try (BufferedReader r = new BufferedReader(new InputStreamReader(getAssets().open("vm/debian.sha256"), StandardCharsets.UTF_8))) { expected = r.readLine(); }
            if (!hash.toString().equals(expected) || total != 3L * 1024 * 1024 * 1024) { temporary.delete(); throw new IOException("Debian image checksum mismatch"); }
            android.system.Os.rename(temporary.getPath(), disk.getPath());
        }
        copyAsset("vm/initrd.cpio", new File(directory, "initrd.cpio"));
        copyAsset("vm/vmlinuz", new File(directory, "vmlinuz"));
        return directory;
    }
    private void launch() {
        try {
            String issue = unavailable(this); if (issue != null) throw new IOException(issue);
            File prefix = Userland.prepare(this);
            File directory = prepareDisk();
            JSONObject config = new JSONObject().put("kernel", new File(directory, "vmlinuz").getPath())
                .put("initrd", new File(directory, "initrd.cpio").getPath())
                .put("params", "console=hvc2 earlycon=uart8250,mmio,0x3f8 arm64.nompam 8250.nr_uarts=4 rdinit=/init panic=-1 quiet systemd.show_status=false")
                .put("memory_mib", 1024).put("platform_version", "~1.0").put("console_input_device", "hvc0")
                .put("disks", new JSONArray().put(new JSONObject().put("image", new File(directory, "rootfs.img").getPath()).put("writable", true)));
            File json = new File(directory, "vm.json");
            Files.write(json.toPath(), config.toString().getBytes(StandardCharsets.UTF_8));
            ProcessBuilder net = new ProcessBuilder(getApplicationInfo().nativeLibraryDir + "/libtos_vmnet.so");
            Map<String, String> environment = net.environment();
            environment.put("PREFIX", prefix.getPath()); environment.put("TMPDIR", getCacheDir().getPath());
            environment.put("LD_LIBRARY_PATH", new File(prefix, "lib").getPath());
            environment.put("LD_PRELOAD", new File(prefix, "lib/libtos_paths.so").getPath());
            net.redirectError(new File(directory, "network.log")); network = net.start();
            workers.execute(() -> networkOutput(network.getInputStream()));
            workers.execute(() -> {
                try (DataOutputStream out = new DataOutputStream(network.getOutputStream())) {
                    while (!ended) networkPackets.take().write(out);
                } catch (Exception error) { if (!ended) fail(error); }
            });
            update("Booting Debian…");
            // Keep vm's own lifecycle text off the binary guest console. fd 3
            // retains the stdout pipe while the CLI's stdout goes to a log.
            ProcessBuilder launch = new ProcessBuilder("/system/bin/sh", "-c",
                "exec \"$@\" 3>&1 1>\"$TOS_VM_CLI_LOG\"", "tos-vm",
                "/apex/com.android.virt/bin/vm", "run", "--name", "tOS-Debian",
                "--console", "/proc/self/fd/3", "--log", new File(directory, "hypervisor.log").getPath(), json.getPath());
            launch.environment().put("TOS_VM_CLI_LOG", new File(directory, "vm-cli.log").getPath());
            vm = launch.redirectError(new File(directory, "vm-error.log")).start();
            guestInput = new DataOutputStream(vm.getOutputStream());
            workers.execute(() -> {
                try {
                    int code = vm.waitFor();
                    if (!ended) fail(new IOException(stopping && code == 0 ? "Debian shut down" : "Debian stopped (" + code + "). See files/debian/vm-error.log."));
                } catch (InterruptedException error) { Thread.currentThread().interrupt(); }
            });
            DataInputStream input = new DataInputStream(vm.getInputStream());
            try (FileOutputStream boot = new FileOutputStream(new File(directory, "boot.log"))) {
                ByteArrayOutputStream line = new ByteArrayOutputStream();
                while (!ended) {
                    int b = input.read(); if (b < 0) throw new EOFException("Debian exited during startup");
                    boot.write(b);
                    if (b == '\n') {
                        String text = new String(line.toByteArray(), StandardCharsets.UTF_8).trim(); line.reset();
                        if (text.equals("TOS_VM_READY")) break;
                    } else if (line.size() < 8192) line.write(b);
                }
            }
            update("Debian running"); ready.complete(null);
            while (!ended) {
                Frame frame = Frame.read(input);
                if (frame.type == 'N' && frame.id == 0) networkPackets.offer(frame);
                else {
                    Client client = clients.get(frame.id);
                    if (client != null) client.offer(frame);
                }
            }
        } catch (Exception error) { if (!ended && !(stopping && error instanceof EOFException)) fail(error); }
    }
    private void networkOutput(InputStream stream) {
        try (DataInputStream in = new DataInputStream(stream)) {
            while (!ended) { Frame frame = Frame.read(in); if (frame.type == 'N' && frame.id == 0) { ready.get(); send(frame); } }
        } catch (Exception error) { if (!ended) fail(error); }
    }
    private void send(Frame frame) throws IOException {
        synchronized (guestWrite) { if (guestInput == null || ended) throw new IOException("Debian is not running"); frame.write(guestInput); }
    }
    private synchronized void fail(Exception error) {
        if (ended) return;
        ended = true; ready.completeExceptionally(error);
        boolean normal = stopping && "Debian shut down".equals(error.getMessage());
        if (normal) android.util.Log.i("tOS-VM", "Debian shut down");
        else {
            lastFailure = error.getMessage(); android.util.Log.e("tOS-VM", error.toString());
            for (String name : new String[]{"boot.log", "vm-cli.log", "vm-error.log", "hypervisor.log"}) {
                try (RandomAccessFile log = new RandomAccessFile(new File(getFilesDir(), "debian/" + name), "r")) {
                    int length = (int)Math.min(8000, log.length()); log.seek(log.length() - length);
                    byte[] bytes = new byte[length]; log.readFully(bytes);
                    String text = name + ":\n" + new String(bytes, StandardCharsets.UTF_8);
                    for (int i = 0; i < text.length(); i += 3000)
                        android.util.Log.e("tOS-VM", text.substring(i, Math.min(i + 3000, text.length())));
                } catch (IOException ignored) {}
            }
        }
        for (Client client : clients.values()) client.close();
        try { if (server != null) android.system.Os.shutdown(server.getFileDescriptor(), android.system.OsConstants.SHUT_RDWR); }
        catch (android.system.ErrnoException ignored) {}
        try { if (server != null) server.close(); } catch (IOException ignored) {}
        if (vm != null) vm.destroy(); if (network != null) network.destroy();
        stopSelf();
    }
    @Override public void onDestroy() {
        fail(new IOException("VM service stopped")); workers.shutdownNow(); stopForeground(STOP_FOREGROUND_REMOVE); super.onDestroy();
    }
    static final class Frame {
        final int type, id; final byte[] bytes;
        Frame(int type, int id, byte[] bytes) { this.type = type; this.id = id; this.bytes = bytes; }
        static Frame read(DataInputStream in) throws IOException {
            int type = in.readUnsignedByte(), id = in.readInt(), length = in.readInt();
            if (length < 0 || length > MAX_FRAME) throw new IOException("Invalid VM frame length");
            byte[] data = new byte[length]; in.readFully(data); return new Frame(type, id, data);
        }
        void write(DataOutputStream out) throws IOException { out.writeByte(type); out.writeInt(id); out.writeInt(bytes.length); out.write(bytes); out.flush(); }
    }
    private final class Client {
        final int id; final LocalSocket socket;
        final ArrayBlockingQueue<Frame> output = new ArrayBlockingQueue<>(128);
        volatile boolean closed;
        Client(int id, LocalSocket socket) { this.id = id; this.socket = socket; }
        void offer(Frame frame) { if (!closed && !output.offer(frame)) close(); }
        void write() {
            try (DataOutputStream out = new DataOutputStream(socket.getOutputStream())) {
                while (!closed) { Frame f = output.take(); f.write(out); if (f.type == 'E') break; }
            } catch (Exception ignored) {} finally { close(); }
        }
        void serve() {
            try (DataInputStream in = new DataInputStream(socket.getInputStream())) {
                if (!ready.isDone()) offer(new Frame('D', id, ("\r\n" + status + "\r\n").getBytes(StandardCharsets.UTF_8)));
                ready.get(10, TimeUnit.MINUTES);
                while (!closed) {
                    Frame f = Frame.read(in);
                    if (f.type != 'D' && f.type != 'O' && f.type != 'R' && f.type != 'C') throw new IOException("Invalid pane message");
                    if ((f.type == 'O' || f.type == 'R') && f.bytes.length != 8) throw new IOException("Invalid pane dimensions");
                    send(new Frame(f.type, id, f.bytes));
                }
            } catch (Exception ignored) {} finally { close(); }
        }
        synchronized void close() {
            if (closed) return; closed = true; clients.remove(id);
            try { socket.shutdownInput(); } catch (IOException ignored) {}
            try { socket.shutdownOutput(); } catch (IOException ignored) {}
            try { socket.close(); } catch (IOException ignored) {}
            output.offer(new Frame('E', id, new byte[0]));
            if (!ended && ready.isDone() && !ready.isCompletedExceptionally()) {
                try { workers.execute(() -> { try { send(new Frame('C', id, new byte[0])); } catch (IOException ignored) {} }); }
                catch (RejectedExecutionException ignored) {}
            }
        }
    }
}
