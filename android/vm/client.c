/* A PTY child for the Android compositor, connected to one guest PTY. */
#define _GNU_SOURCE
#include "protocol.h"
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <termios.h>
static volatile sig_atomic_t resized = 1;
static void resize_signal(int sig) {
  (void)sig;
  resized = 1;
}
static void size_packet(unsigned char data[8]) {
  struct winsize w = {.ws_col = 80, .ws_row = 24};
  ioctl(0, TIOCGWINSZ, &w);
  uint16_t v = htons(w.ws_col);
  memcpy(data, &v, 2);
  v = htons(w.ws_row);
  memcpy(data + 2, &v, 2);
  v = htons(w.ws_xpixel);
  memcpy(data + 4, &v, 2);
  v = htons(w.ws_ypixel);
  memcpy(data + 6, &v, 2);
}
int main(int argc, char **argv) {
  if (argc != 2)
    return 2;
  struct sockaddr_un addr = {.sun_family = AF_UNIX};
  size_t n = strlen(argv[1]);
  if (n + 1 >= sizeof(addr.sun_path))
    return 2;
  memcpy(addr.sun_path + 1, argv[1], n);
  int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
  if (fd < 0)
    return 1;
  int connected = 0;
  for (int attempt = 0; attempt < 100; attempt++) {
    if (connect(fd, (struct sockaddr *)&addr,
                (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + n)) ==
        0) {
      connected = 1;
      break;
    }
    usleep(100000);
  }
  if (!connected) {
    perror("tOS Debian connection");
    close(fd);
    return 1;
  }
  signal(SIGPIPE, SIG_IGN);
  signal(SIGWINCH, resize_signal);
  struct termios original, raw;
  int have = tcgetattr(0, &original) == 0;
  if (have) {
    raw = original;
    cfmakeraw(&raw);
    tcsetattr(0, TCSANOW, &raw);
  }
  unsigned char data[VM_MAX_FRAME];
  size_packet(data);
  frame_send(fd, 'O', 0, data, 8);
  for (;;) {
    if (resized) {
      resized = 0;
      size_packet(data);
      if (frame_send(fd, 'R', 0, data, 8))
        break;
    }
    struct pollfd p[2] = {{.fd = 0, .events = POLLIN},
                          {.fd = fd, .events = POLLIN}};
    if (poll(p, 2, -1) < 0) {
      if (errno == EINTR)
        continue;
      break;
    }
    if (p[0].revents & POLLIN) {
      ssize_t got = read(0, data, 16384);
      if (got <= 0 || frame_send(fd, 'D', 0, data, (uint32_t)got))
        break;
    }
    if (p[1].revents & POLLIN) {
      unsigned char type;
      uint32_t id, len;
      if (frame_read(fd, &type, &id, data, &len))
        break;
      if (type == 'E')
        break;
      if (type == 'D' && io_all(1, data, len, 1))
        break;
    }
    if ((p[0].revents | p[1].revents) & (POLLHUP | POLLERR | POLLNVAL))
      break;
  }
  frame_send(fd, 'C', 0, NULL, 0);
  close(fd);
  if (have)
    tcsetattr(0, TCSANOW, &original);
  return 0;
}
