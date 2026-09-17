/* The same pixel artwork as the desktop greeting, sized to this pane. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

static void base64(const unsigned char *p) {
    const char *alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    size_t n = strlen((const char *)p);
    for (size_t i = 0; i < n; i += 3) {
        unsigned value = (unsigned)p[i] << 16;
        if (i + 1 < n) value |= (unsigned)p[i + 1] << 8;
        if (i + 2 < n) value |= p[i + 2];
        putchar(alphabet[value >> 18]); putchar(alphabet[(value >> 12) & 63]);
        putchar(i + 1 < n ? alphabet[(value >> 6) & 63] : '=');
        putchar(i + 2 < n ? alphabet[value & 63] : '=');
    }
}
int main(void) {
    struct winsize size = {0};
    const char *prefix = getenv("PREFIX");
    if (prefix && ioctl(STDOUT_FILENO, TIOCGWINSZ, &size) == 0 && size.ws_col && size.ws_row) {
        unsigned cw = size.ws_xpixel / size.ws_col, ch = size.ws_ypixel / size.ws_row;
        if (!cw) cw = 12;
        if (!ch) ch = 24;
        unsigned cols = (512 + cw - 1) / cw, rows = (170 + ch - 1) / ch;
        if (cols > size.ws_col) { rows = rows * size.ws_col / cols; cols = size.ws_col; }
        unsigned limit = size.ws_row / 3;
        if (rows > limit) { cols = cols * limit / rows; rows = limit; }
        if (cols >= 12 && rows >= 2) {
            char path[4096];
            if (snprintf(path, sizeof(path), "%s/share/tos/splash.png", prefix) >= (int)sizeof(path)) return 1;
            putchar('\r');
            for (unsigned i = 0; i < rows; i++) putchar('\n');
            printf("\033[%uA\033_Ga=T,f=100,t=f,c=%u,r=%u,C=1,q=2;", rows, cols, rows);
            base64((const unsigned char *)path);
            printf("\033\\\033[%uB\r", rows);
        }
    }
    puts("\033[38;2;145;180;135mtOS\033[0m — the terminal is the desktop");
    puts("git · curl · nvim · rg · fzf · ssh · yazi");
    putchar('\n');
    return 0;
}
