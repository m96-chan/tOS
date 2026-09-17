#ifndef TOS_VM_PROTOCOL_H
#define TOS_VM_PROTOCOL_H
#include <arpa/inet.h>
#include <errno.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#define VM_MAX_FRAME 65536
/* Type + session ID + length, all integers in network byte order. */
static int io_all(int fd, void *data, size_t len, int writing) {
  unsigned char *p = data;
  while (len) {
    ssize_t n = writing ? write(fd, p, len) : read(fd, p, len);
    if (n < 0 && errno == EINTR)
      continue;
    if (n <= 0)
      return -1;
    p += n;
    len -= (size_t)n;
  }
  return 0;
}
static int frame_send(int fd, unsigned char type, uint32_t id, const void *data,
                      uint32_t len) {
  unsigned char h[9];
  uint32_t v = htonl(id);
  h[0] = type;
  memcpy(h + 1, &v, 4);
  v = htonl(len);
  memcpy(h + 5, &v, 4);
  return io_all(fd, h, 9, 1) || (len && io_all(fd, (void *)data, len, 1)) ? -1
                                                                          : 0;
}
static int frame_read(int fd, unsigned char *type, uint32_t *id,
                      unsigned char *data, uint32_t *len) {
  unsigned char h[9];
  uint32_t v;
  if (io_all(fd, h, 9, 0))
    return -1;
  *type = h[0];
  memcpy(&v, h + 1, 4);
  *id = ntohl(v);
  memcpy(&v, h + 5, 4);
  *len = ntohl(v);
  if (*len > VM_MAX_FRAME)
    return -1;
  return *len ? io_all(fd, data, *len, 0) : 0;
}
#endif
