/* Relocate packaged tools' build-time paths into this app's private prefix.
 * Loaded only by shell children, never by the Android VM/compositor.
 * Native code stays in Android's signed, read-only nativeLibraryDir; scripts
 * are read by their packaged interpreter, not execve'd from writable storage.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static const char *relocate(const char *path, char out[PATH_MAX]) {
    if (!path) return path;
    const char *base = NULL, *tail = NULL;
    const char *old = "/data/data/com.termux/files/usr";
    size_t n = strlen(old);
    if (!strncmp(path, old, n) && (path[n] == '/' || path[n] == 0)) {
        base = getenv("PREFIX"); tail = path + n;
    } else if (!strncmp(path, "/usr/bin/", 9)) {
        base = getenv("PREFIX"); tail = path + 4;
    } else if (!strncmp(path, "/bin/", 5)) {
        base = getenv("PREFIX"); tail = path;
    } else if (!strncmp(path, "/tmp", 4) && (path[4] == '/' || path[4] == 0)) {
        base = getenv("TMPDIR"); tail = path + 4;
    }
    if (!base || !tail) return path;
    if (snprintf(out, PATH_MAX, "%s%s", base, tail) >= PATH_MAX) {
        errno = ENAMETOOLONG; return NULL;
    }
    return out;
}
#define REAL(name) __typeof__(&name) real = (__typeof__(&name))dlsym(RTLD_NEXT, #name)
#define PATH(name) char name##_buffer[PATH_MAX]; name = relocate(name, name##_buffer)

int open(const char *path, int flags, ...) {
    mode_t mode = 0;
    if ((flags & O_CREAT) || ((flags & O_TMPFILE) == O_TMPFILE)) {
        va_list ap; va_start(ap, flags); mode = va_arg(ap, int); va_end(ap);
    }
    REAL(open); PATH(path); return path ? real(path, flags, mode) : -1;
}
int openat(int dir, const char *path, int flags, ...) {
    mode_t mode = 0;
    if ((flags & O_CREAT) || ((flags & O_TMPFILE) == O_TMPFILE)) {
        va_list ap; va_start(ap, flags); mode = va_arg(ap, int); va_end(ap);
    }
    REAL(openat); PATH(path); return path ? real(dir, path, flags, mode) : -1;
}
FILE *fopen(const char *path, const char *mode) { REAL(fopen); PATH(path); return path ? real(path, mode) : NULL; }
FILE *freopen(const char *path, const char *mode, FILE *stream) { REAL(freopen); PATH(path); return real(path, mode, stream); }
DIR *opendir(const char *path) { REAL(opendir); PATH(path); return path ? real(path) : NULL; }
int stat(const char *path, struct stat *buf) { REAL(stat); PATH(path); return path ? real(path, buf) : -1; }
int lstat(const char *path, struct stat *buf) { REAL(lstat); PATH(path); return path ? real(path, buf) : -1; }
int access(const char *path, int mode) { REAL(access); PATH(path); return path ? real(path, mode) : -1; }
int chdir(const char *path) { REAL(chdir); PATH(path); return path ? real(path) : -1; }
int mkdir(const char *path, mode_t mode) { REAL(mkdir); PATH(path); return path ? real(path, mode) : -1; }
int unlink(const char *path) { REAL(unlink); PATH(path); return path ? real(path) : -1; }
int rmdir(const char *path) { REAL(rmdir); PATH(path); return path ? real(path) : -1; }
int rename(const char *from, const char *to) { REAL(rename); PATH(from); PATH(to); return from && to ? real(from, to) : -1; }
ssize_t readlink(const char *path, char *out, size_t size) { REAL(readlink); PATH(path); return path ? real(path, out, size) : -1; }
char *realpath(const char *path, char *out) { REAL(realpath); PATH(path); return path ? real(path, out) : NULL; }
void *dlopen(const char *path, int flags) { REAL(dlopen); PATH(path); return real(path, flags); }

/* Supply the interpreter ourselves for writable scripts. ELF files are only
 * passed to the ordinary execve path; no executable memory is synthesized. */
int execve(const char *path, char *const argv[], char *const env[]) {
    REAL(execve); PATH(path);
    if (!path) return -1;
    char header[1024];
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    ssize_t length = fd < 0 ? -1 : read(fd, header, sizeof(header) - 1);
    if (fd >= 0) close(fd);
    if (length < 2 || header[0] != '#' || header[1] != '!') return real(path, argv, env);
    header[length] = 0;
    char *line = strchr(header, '\n');
    if (!line) { errno = ENOEXEC; return -1; }
    *line = 0;
    char *interpreter = header + 2;
    while (*interpreter == ' ' || *interpreter == '\t') interpreter++;
    char *arg = interpreter;
    while (*arg && *arg != ' ' && *arg != '\t' && *arg != '\r') arg++;
    if (*arg) *arg++ = 0;
    while (*arg == ' ' || *arg == '\t') arg++;
    char *end = arg + strlen(arg);
    while (end > arg && (end[-1] == '\r' || end[-1] == ' ' || end[-1] == '\t')) *--end = 0;
    char interpreter_buffer[PATH_MAX];
    const char *mapped_interpreter = relocate(interpreter, interpreter_buffer);
    if (!mapped_interpreter || !*mapped_interpreter) { errno = ENOEXEC; return -1; }
    size_t count = 0;
    while (argv[count]) { if (++count > 4096) { errno = E2BIG; return -1; } }
    char **args = calloc(count + 4, sizeof(char *));
    if (!args) { errno = ENOMEM; return -1; }
    size_t at = 0; args[at++] = (char *)mapped_interpreter;
    if (*arg) args[at++] = arg;
    args[at++] = (char *)path;
    for (size_t i = 1; i < count; i++) args[at++] = argv[i];
    int result = real(mapped_interpreter, args, env);
    int saved = errno; free(args); errno = saved; return result;
}
extern char **environ;
int execv(const char *path, char *const argv[]) { return execve(path, argv, environ); }
int execvpe(const char *file, char *const argv[], char *const env[]) {
    if (strchr(file, '/')) return execve(file, argv, env);
    const char *search = getenv("PATH");
    if (!search) search = "/system/bin";
    bool denied = false;
    do {
        const char *end = strchr(search, ':');
        size_t size = end ? (size_t)(end - search) : strlen(search);
        char path[PATH_MAX];
        int written = size ? snprintf(path, sizeof(path), "%.*s/%s", (int)size, search, file)
                           : snprintf(path, sizeof(path), "%s", file);
        if (written < 0 || written >= (int)sizeof(path)) { errno = ENAMETOOLONG; return -1; }
        execve(path, argv, env);
        if (errno == EACCES) denied = true;
        else if (errno != ENOENT && errno != ENOTDIR) return -1;
        if (!end) break;
        search = end + 1;
    } while (true);
    errno = denied ? EACCES : ENOENT; return -1;
}
int execvp(const char *file, char *const argv[]) { return execvpe(file, argv, environ); }
