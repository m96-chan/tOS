/* User-mode guest networking. No Android VPN, root, or host TAP permission. */
#define _GNU_SOURCE
#include "protocol.h"
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <slirp/libslirp.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static struct pollfd fds[4096];
static unsigned count;
struct timer {
  SlirpTimerCb callback;
  void *data;
  int64_t expires;
  struct timer *next;
};
static struct timer *timers;
static int64_t clock_ns(void *opaque) {
  (void)opaque;
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}
static ssize_t packet(const void *buf, size_t len, void *opaque) {
  (void)opaque;
  return frame_send(1, 'N', 0, buf, (uint32_t)len) ? -1 : (ssize_t)len;
}
static void error(const char *msg, void *opaque) {
  (void)opaque;
  fprintf(stderr, "slirp: %s\n", msg);
}
static void *timer_new(SlirpTimerCb cb, void *data, void *opaque) {
  (void)opaque;
  struct timer *t = calloc(1, sizeof(*t));
  if (!t)
    exit(1);
  t->callback = cb;
  t->data = data;
  t->expires = -1;
  t->next = timers;
  timers = t;
  return t;
}
static void timer_free(void *ptr, void *opaque) {
  (void)opaque;
  struct timer **p = &timers;
  while (*p && *p != ptr)
    p = &(*p)->next;
  if (*p) {
    struct timer *t = *p;
    *p = t->next;
    free(t);
  }
}
static void timer_mod(void *ptr, int64_t expires, void *opaque) {
  (void)opaque;
  ((struct timer *)ptr)->expires = expires;
}
static void register_fd(int fd, void *opaque) {
  (void)fd;
  (void)opaque;
}
static void notify(void *opaque) { (void)opaque; }
static int add_poll(int fd, int events, void *opaque) {
  (void)opaque;
  if (count >= 4096)
    return -1;
  short e = 0;
  if (events & SLIRP_POLL_IN)
    e |= POLLIN;
  if (events & SLIRP_POLL_OUT)
    e |= POLLOUT;
  if (events & SLIRP_POLL_PRI)
    e |= POLLPRI;
  unsigned i = count++;
  fds[i] = (struct pollfd){.fd = fd, .events = e};
  return (int)i;
}
static int get_events(int i, void *opaque) {
  (void)opaque;
  if (i < 0 || (unsigned)i >= count)
    return SLIRP_POLL_ERR;
  short e = fds[i].revents;
  int r = 0;
  if (e & POLLIN)
    r |= SLIRP_POLL_IN;
  if (e & POLLOUT)
    r |= SLIRP_POLL_OUT;
  if (e & POLLPRI)
    r |= SLIRP_POLL_PRI;
  if (e & (POLLERR | POLLNVAL))
    r |= SLIRP_POLL_ERR;
  if (e & POLLHUP)
    r |= SLIRP_POLL_HUP;
  return r;
}
int main(void) {
  signal(SIGPIPE, SIG_IGN);
  SlirpConfig cfg = {.version = 4,
                     .in_enabled = true,
                     .disable_host_loopback = true,
                     .if_mtu = 1500,
                     .if_mru = 1500,
                     .vhostname = "tOS"};
  inet_pton(AF_INET, "10.0.2.0", &cfg.vnetwork);
  inet_pton(AF_INET, "255.255.255.0", &cfg.vnetmask);
  inet_pton(AF_INET, "10.0.2.2", &cfg.vhost);
  inet_pton(AF_INET, "10.0.2.3", &cfg.vnameserver);
  inet_pton(AF_INET, "10.0.2.15", &cfg.vdhcp_start);
  SlirpCb cb = {.send_packet = packet,
                .guest_error = error,
                .clock_get_ns = clock_ns,
                .timer_new = timer_new,
                .timer_free = timer_free,
                .timer_mod = timer_mod,
                .register_poll_fd = register_fd,
                .unregister_poll_fd = register_fd,
                .notify = notify};
  Slirp *s = slirp_new(&cfg, &cb, NULL);
  if (!s)
    return 1;
  unsigned char data[VM_MAX_FRAME];
  for (;;) {
    count = 1;
    fds[0] = (struct pollfd){.fd = 0, .events = POLLIN};
    uint32_t timeout = 1000;
    slirp_pollfds_fill(s, &timeout, add_poll, NULL);
    int64_t now = clock_ns(NULL) / 1000000;
    for (struct timer *t = timers; t; t = t->next)
      if (t->expires >= 0) {
        int64_t left = t->expires - now;
        if (left < 0)
          left = 0;
        if (left < timeout)
          timeout = (uint32_t)left;
      }
    int result = poll(fds, count, timeout > INT_MAX ? INT_MAX : (int)timeout);
    slirp_pollfds_poll(s, result < 0, get_events, NULL);
    if (fds[0].revents & POLLIN) {
      unsigned char type;
      uint32_t id, len;
      if (frame_read(0, &type, &id, data, &len))
        break;
      if (type == 'N' && !id && len <= 65535)
        slirp_input(s, data, (int)len);
    }
    if (fds[0].revents & (POLLHUP | POLLERR | POLLNVAL))
      break;
    now = clock_ns(NULL) / 1000000;
    for (struct timer *t = timers, *next; t; t = next) {
      next = t->next;
      if (t->expires >= 0 && t->expires <= now) {
        t->expires = -1;
        t->callback(t->data);
      }
    }
  }
  slirp_cleanup(s);
  return 0;
}
