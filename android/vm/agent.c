/* Guest session multiplexer. Only the owning Android app sees this console. */
#define _GNU_SOURCE
#include "protocol.h"
#include <fcntl.h>
#include <linux/if_tun.h>
#include <net/if.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>

#define SESSIONS 64
struct session {
  uint32_t id;
  int fd;
  pid_t pid;
  unsigned char pending[VM_MAX_FRAME];
  size_t count;
};
static struct session sessions[SESSIONS];
static int console;
static void window(int fd, const unsigned char *p) {
  struct winsize w = {0};
  uint16_t v;
  memcpy(&v, p, 2);
  w.ws_col = ntohs(v);
  memcpy(&v, p + 2, 2);
  w.ws_row = ntohs(v);
  memcpy(&v, p + 4, 2);
  w.ws_xpixel = ntohs(v);
  memcpy(&v, p + 6, 2);
  w.ws_ypixel = ntohs(v);
  if (w.ws_col && w.ws_row)
    ioctl(fd, TIOCSWINSZ, &w);
}
static void end_session(struct session *s) {
  if (s->fd < 0)
    return;
  close(s->fd);
  kill(-s->pid, SIGHUP);
  kill(s->pid, SIGHUP);
  frame_send(console, 'E', s->id, NULL, 0);
  s->fd = -1;
  s->count = 0;
}
static void start_session(uint32_t id, const unsigned char *size) {
  struct session *s = NULL;
  for (int i = 0; i < SESSIONS; i++)
    if (sessions[i].fd >= 0 && sessions[i].id == id)
      return;
  for (int i = 0; i < SESSIONS; i++)
    if (sessions[i].fd < 0) {
      s = &sessions[i];
      break;
    }
  int fd = posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC);
  if (!s || fd < 0 || grantpt(fd) || unlockpt(fd)) {
    if (fd >= 0)
      close(fd);
    frame_send(console, 'E', id, NULL, 0);
    return;
  }
  char path[128];
  if (ptsname_r(fd, path, sizeof(path))) {
    close(fd);
    return;
  }
  window(fd, size);
  pid_t pid = fork();
  if (pid == 0) {
    setsid();
    int slave = open(path, O_RDWR);
    if (slave < 0)
      _exit(126);
    ioctl(slave, TIOCSCTTY, 0);
    dup2(slave, 0);
    dup2(slave, 1);
    dup2(slave, 2);
    if (slave > 2)
      close(slave);
    close(fd);
    signal(SIGPIPE, SIG_DFL);
    signal(SIGCHLD, SIG_DFL);
    clearenv();
    setenv("HOME", "/root", 1);
    setenv("USER", "root", 1);
    setenv("LOGNAME", "root", 1);
    setenv("SHELL", "/bin/bash", 1);
    setenv("PATH",
           "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", 1);
    setenv("LANG", "C.UTF-8", 1);
    setenv("TERM", "xterm-256color", 1);
    setenv("COLORTERM", "truecolor", 1);
    chdir("/root");
    execl("/bin/bash", "bash", "--noprofile", "--rcfile", "/root/.bashrc", "-i",
          (char *)NULL);
    _exit(127);
  }
  if (pid < 0) {
    close(fd);
    frame_send(console, 'E', id, NULL, 0);
    return;
  }
  fcntl(fd, F_SETFL, O_NONBLOCK);
  s->id = id;
  s->fd = fd;
  s->pid = pid;
  s->count = 0;
}
static int network(void) {
  int fd = open("/dev/net/tun", O_RDWR | O_CLOEXEC | O_NONBLOCK);
  if (fd < 0)
    return -1;
  struct ifreq req = {0};
  req.ifr_flags = IFF_TAP | IFF_NO_PI;
  strcpy(req.ifr_name, "tos0");
  if (ioctl(fd, TUNSETIFF, &req) < 0) {
    close(fd);
    return -1;
  }
  pid_t child = fork();
  if (child == 0) {
    execl("/bin/sh", "sh", "-c",
          "ip link set tos0 up && ip addr replace 10.0.2.15/24 dev tos0 && ip "
          "route replace default via 10.0.2.2",
          (char *)NULL);
    _exit(127);
  }
  if (child > 0)
    waitpid(child, NULL, 0);
  return fd;
}
int main(void) {
  signal(SIGPIPE, SIG_IGN);
  console = open("/dev/hvc0", O_RDWR | O_NOCTTY | O_CLOEXEC);
  if (console < 0)
    return 1;
  struct termios t;
  if (tcgetattr(console, &t))
    return 1;
  cfmakeraw(&t);
  tcsetattr(console, TCSANOW, &t);
  for (int i = 0; i < SESSIONS; i++)
    sessions[i].fd = -1;
  int tap = network();
  if (tap < 0)
    dprintf(console, "tOS: network interface unavailable: %s\n",
            strerror(errno));
  dprintf(console, "TOS_VM_READY\n");
  unsigned char data[VM_MAX_FRAME];
  for (;;) {
    struct pollfd p[SESSIONS + 2] = {{.fd = console, .events = POLLIN},
                                     {.fd = tap, .events = POLLIN}};
    for (int i = 0; i < SESSIONS; i++) {
      p[i + 2].fd = sessions[i].fd;
      p[i + 2].events = POLLIN | (sessions[i].count ? POLLOUT : 0);
    }
    if (poll(p, SESSIONS + 2, 1000) < 0 && errno != EINTR)
      break;
    if (p[0].revents & POLLIN) {
      unsigned char type;
      uint32_t id, len;
      if (frame_read(console, &type, &id, data, &len))
        break;
      if (type == 'O' && id && len == 8)
        start_session(id, data);
      else if (type == 'N' && !id && tap >= 0 && len <= 65535)
        write(tap, data, len);
      else if (type == 'Q' && !id) {
        pid_t c = fork();
        if (c == 0) {
          execl("/sbin/poweroff", "poweroff", (char *)NULL);
          _exit(1);
        }
      } else
        for (int i = 0; i < SESSIONS; i++)
          if (sessions[i].fd >= 0 && sessions[i].id == id) {
            struct session *s = &sessions[i];
            if (type == 'R' && len == 8)
              window(s->fd, data);
            else if (type == 'C')
              end_session(s);
            else if (type == 'D') {
              if (s->count + len > sizeof(s->pending))
                end_session(s);
              else {
                memcpy(s->pending + s->count, data, len);
                s->count += len;
              }
            }
            break;
          }
    }
    if (tap >= 0 && p[1].revents & POLLIN) {
      ssize_t n = read(tap, data, sizeof(data));
      if (n > 0 && frame_send(console, 'N', 0, data, (uint32_t)n))
        break;
    }
    for (int i = 0; i < SESSIONS; i++) {
      struct session *s = &sessions[i];
      if (s->fd < 0)
        continue;
      if (s->count && p[i + 2].revents & POLLOUT) {
        ssize_t n = write(s->fd, s->pending, s->count);
        if (n > 0) {
          s->count -= (size_t)n;
          memmove(s->pending, s->pending + n, s->count);
        }
      }
      if (p[i + 2].revents & POLLIN) {
        ssize_t n = read(s->fd, data, sizeof(data));
        if (n > 0)
          frame_send(console, 'D', s->id, data, (uint32_t)n);
        else if (n == 0 || errno == EIO)
          end_session(s);
      } else if (p[i + 2].revents & (POLLHUP | POLLERR | POLLNVAL))
        end_session(s);
    }
    while (waitpid(-1, NULL, WNOHANG) > 0) {
    }
  }
  for (int i = 0; i < SESSIONS; i++)
    end_session(&sessions[i]);
  return 1;
}
