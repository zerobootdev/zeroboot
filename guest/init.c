/* Zeroboot guest agent v3
 * Minimal approach: C init listens on serial, executes CODE: via popen(python3)
 * No pre-forked Python — simpler, more reliable after snapshot restore.
 * Trade-off: each CODE: execution pays Python startup cost (~50ms on SSD rootfs)
 * but file I/O works correctly after fork+resume.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define SERIAL_DEV "/dev/ttyS0"
#define BUF_SIZE   65536

static int serial_fd = -1;

static void serial_write(const char *s) {
    if (!s) return;
    size_t len = strlen(s);
    while (len > 0) {
        ssize_t n = write(serial_fd, s, len);
        if (n <= 0) break;
        s += n; len -= n;
    }
}

static int serial_read_line(char *buf, int size) {
    int i = 0;
    while (i < size - 1) {
        char c;
        ssize_t n = read(serial_fd, &c, 1);
        if (n <= 0) continue;
        if (c == '\r') continue;
        if (c == '\n') {
            if (i == 0) continue;
            buf[i] = '\0';
            return i;
        }
        buf[i++] = c;
    }
    buf[i] = '\0';
    return i;
}

/* Execute python3 -c "<code>" and capture output */
static void run_python(const char *code, char *out, int out_size) {
    /* Write code to a temp file to avoid shell escaping issues */
    FILE *tmpf = fopen("/tmp/zb_code.py", "w");
    if (!tmpf) {
        snprintf(out, out_size, "error: cannot write temp file\n");
        return;
    }
    fprintf(tmpf, "import sys\nsys.path.insert(0,'/usr/local/lib/python3.10/dist-packages')\n");
    fprintf(tmpf, "%s\n", code);
    fclose(tmpf);

    /* Run python3 /tmp/zb_code.py */
    FILE *fp = popen("python3 /tmp/zb_code.py 2>&1", "r");
    if (!fp) {
        snprintf(out, out_size, "error: popen failed\n");
        return;
    }

    int pos = 0;
    int c;
    while (pos < out_size - 1 && (c = fgetc(fp)) != EOF) {
        out[pos++] = (char)c;
    }
    out[pos] = '\0';
    pclose(fp);
    unlink("/tmp/zb_code.py");
}

int main(void) {
    mount("proc",     "/proc", "proc",     0, 0);
    mount("sysfs",    "/sys",  "sysfs",    0, 0);
    mount("devtmpfs", "/dev",  "devtmpfs", 0, 0);

    serial_fd = open(SERIAL_DEV, O_RDWR | O_NOCTTY);
    if (serial_fd < 0) _exit(1);

    serial_write("READY\n");

    char cmd[BUF_SIZE];
    char out[BUF_SIZE];

    while (1) {
        int len = serial_read_line(cmd, sizeof(cmd));
        if (len <= 0) continue;

        if (strncmp(cmd, "CODE:", 5) == 0) {
            run_python(cmd + 5, out, sizeof(out));
            serial_write(out);
            if (out[0] && out[strlen(out)-1] != '\n')
                serial_write("\n");
        } else if (strncmp(cmd, "echo ", 5) == 0) {
            serial_write(cmd + 5);
            serial_write("\n");
        } else if (strncmp(cmd, "cat ", 4) == 0) {
            char buf[4096];
            int fd = open(cmd + 4, O_RDONLY);
            if (fd >= 0) {
                ssize_t n;
                while ((n = read(fd, buf, sizeof(buf) - 1)) > 0) {
                    buf[n] = '\0';
                    serial_write(buf);
                }
                close(fd);
            } else {
                serial_write("error: cannot open ");
                serial_write(cmd + 4);
                serial_write("\n");
            }
        } else {
            serial_write("unknown: ");
            serial_write(cmd);
            serial_write("\n");
        }

        serial_write("ZEROBOOT_DONE\n");
    }
}
