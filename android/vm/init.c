/* Minimal initramfs: mount the app-owned Debian disk and enter its userspace.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>
static void install(const char *name, mode_t mode) {
  char source[256], destination[256];
  snprintf(source, sizeof(source), "/tos/%s", name);
  snprintf(destination, sizeof(destination), "/root/usr/lib/tos/%s", name);
  int in = open(source, O_RDONLY),
      out = open(destination, O_WRONLY | O_CREAT | O_TRUNC, mode);
  if (in < 0 || out < 0) {
    perror("update guest support");
    exit(1);
  }
  char buf[8192];
  ssize_t n;
  while ((n = read(in, buf, sizeof(buf))) > 0) {
    ssize_t at = 0;
    while (at < n) {
      ssize_t w = write(out, buf + at, (size_t)(n - at));
      if (w <= 0)
        exit(1);
      at += w;
    }
  }
  fchmod(out, mode);
  close(in);
  close(out);
}
static void node(const char *path, unsigned major, unsigned minor) {
  mknod(path, S_IFCHR | 0600, makedev(major, minor));
}
int main(void) {
  mkdir("/dev", 0755);
  if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) < 0 &&
      mount("tmpfs", "/dev", "tmpfs", 0, NULL) < 0) {
    perror("mount dev");
    return 1;
  }
  node("/dev/console", 5, 1);
  node("/dev/null", 1, 3);
  node("/dev/zero", 1, 5);
  node("/dev/random", 1, 8);
  node("/dev/urandom", 1, 9);
  node("/dev/tty", 5, 0);
  node("/dev/ptmx", 5, 2);
  node("/dev/hvc0", 229, 0);
  mkdir("/dev/net", 0755);
  node("/dev/net/tun", 10, 200);
  mkdir("/sys", 0755);
  mount("sysfs", "/sys", "sysfs", 0, NULL);
  // Initial package setup logs use the bridge before its READY handshake.
  // /dev/console may not be available yet when its hvc driver probes late.
  int console = -1;
  for (int i = 0; i < 100 && console < 0; i++) {
    console = open("/dev/hvc0", O_RDWR);
    if (console < 0)
      usleep(100000);
  }
  if (console < 0)
    return 1;
  dup2(console, 0);
  dup2(console, 1);
  dup2(console, 2);
  if (console > 2)
    close(console);
  puts("tOS: mounting Debian disk");
  mkdir("/root", 0755);
  for (int i = 0; i < 100; i++) {
    FILE *dev = fopen("/sys/class/block/vda/dev", "r");
    unsigned major, minor;
    if (dev) {
      if (fscanf(dev, "%u:%u", &major, &minor) == 2)
        mknod("/dev/vda", S_IFBLK | 0600, makedev(major, minor));
      fclose(dev);
    }
    if (mount("/dev/vda", "/root", "ext4", 0, NULL) == 0)
      break;
    if (i == 99) {
      perror("tOS: mount Debian disk");
      return 1;
    }
    usleep(100000);
  }
  mkdir("/root/dev", 0755);
  install("agent", 0755);
  install("boot.sh", 0755);
  install("bashrc", 0644);
  if (mount("/dev", "/root/dev", NULL, MS_MOVE, NULL) < 0) {
    perror("move dev");
    return 1;
  }
  if (chdir("/root") || chroot(".")) {
    perror("enter Debian");
    return 1;
  }
  chdir("/");
  execl("/bin/bash", "bash", "/usr/lib/tos/boot.sh", (char *)NULL);
  perror("start Debian");
  return 1;
}
