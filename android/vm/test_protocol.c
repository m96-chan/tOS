/* Exercise stream fragmentation, binary payloads, bounds and truncated peers.
 */
#include "protocol.h"
#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/wait.h>

static void fragmented(void) {
  int pair[2];
  assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
  pid_t pid = fork();
  assert(pid >= 0);
  if (!pid) {
    close(pair[0]);
    const unsigned char header[] = {'D', 0x12, 0x34, 0x56, 0x78, 0, 1, 0, 0};
    for (size_t i = 0; i < sizeof(header); i++) {
      assert(write(pair[1], header + i, 1) == 1);
      usleep(1000);
    }
    for (unsigned i = 0; i < VM_MAX_FRAME; i += 256) {
      unsigned char chunk[256];
      for (unsigned j = 0; j < 256; j++)
        chunk[j] = (unsigned char)j;
      assert(io_all(pair[1], chunk, sizeof(chunk), 1) == 0);
    }
    assert(frame_send(pair[1], 'E', 0x12345678, NULL, 0) == 0);
    close(pair[1]);
    _exit(0);
  }
  close(pair[1]);
  unsigned char data[VM_MAX_FRAME], type;
  uint32_t id, len;
  assert(frame_read(pair[0], &type, &id, data, &len) == 0);
  assert(type == 'D' && id == 0x12345678 && len == VM_MAX_FRAME);
  for (unsigned i = 0; i < len; i++)
    assert(data[i] == (unsigned char)i);
  assert(frame_read(pair[0], &type, &id, data, &len) == 0);
  assert(type == 'E' && id == 0x12345678 && len == 0);
  assert(frame_read(pair[0], &type, &id, data, &len) == -1);
  close(pair[0]);
  int status;
  assert(waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
         WEXITSTATUS(status) == 0);
}

static void rejected(const unsigned char *bytes, size_t length) {
  int pair[2];
  assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
  assert(io_all(pair[1], (void *)bytes, length, 1) == 0);
  close(pair[1]);
  unsigned char data[VM_MAX_FRAME], type;
  uint32_t id, len;
  assert(frame_read(pair[0], &type, &id, data, &len) == -1);
  close(pair[0]);
}

int main(void) {
  signal(SIGPIPE, SIG_IGN);
  fragmented();
  const unsigned char oversize[] = {'D', 0, 0, 0, 1, 0, 1, 0, 1};
  const unsigned char short_body[] = {'D', 0, 0, 0, 1, 0, 0, 0, 2, 0};
  rejected(oversize, sizeof(oversize));
  rejected(short_body, sizeof(short_body));
  rejected(short_body, 5);
  puts("VM protocol: fragmented binary stream, maximum frame, EOF and invalid "
       "lengths passed");
  return 0;
}
