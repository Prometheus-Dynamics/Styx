#!/usr/bin/env bash
# Runs a command and names every GPU device node any process of it opens: /dev/dri/*,
# /dev/nvidia* and /dev/kfd, by executable and node. Fails (exit 3) when there is one, so it proves
# a gate keeps off the desktop's GPU:
#
#   scripts/gpu-opens.sh scripts/gate.sh
#   scripts/gpu-opens.sh cargo nextest run -p styx-daedalus --all-features
#
# The command's own exit status is returned when nothing was opened. --plant runs `vulkaninfo
# --summary` with every Vulkan driver instead, which must show opens: proof the logging works here.
#
# How: a small LD_PRELOAD library, built here with cc, wraps open, open64, openat, openat64,
# __open_2 and __open64_2 and appends "pid exe node" to a log for each GPU node. Children inherit
# it, so it sees every process the command starts. A static binary, or one that makes the syscall
# itself, is not seen; Rust test binaries and the Vulkan and DRM libraries go through libc.
set -euo pipefail

plant=0
if [ "${1:-}" = --plant ]; then
  plant=1
  shift
fi
if [ "$plant" = 0 ] && [ $# = 0 ]; then
  echo "usage: $0 <command> [args...] | $0 --plant" >&2
  exit 2
fi

dir="$(mktemp -d "${TMPDIR:-/tmp}/gpu-opens.XXXXXX")"
trap 'rm -rf -- "${dir:?}"' EXIT
cat > "$dir/shim.c" <<'EOF'
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int (*real_open)(const char *, int, ...);

static void note(const char *path) {
  if (!path) return;
  if (strncmp(path, "/dev/dri/", 9) && strncmp(path, "/dev/nvidia", 11) && strcmp(path, "/dev/kfd"))
    return;
  const char *log = getenv("GPU_OPENS_LOG");
  if (!log) return;
  if (!real_open) real_open = dlsym(RTLD_NEXT, "open");
  char exe[512] = {0};
  if (readlink("/proc/self/exe", exe, sizeof exe - 1) < 0) strcpy(exe, "?");
  char line[1200];
  int len = snprintf(line, sizeof line, "%d %s %s\n", getpid(), exe, path);
  int fd = real_open(log, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644);
  if (fd >= 0) {
    if (write(fd, line, (size_t)len) < 0) {}
    close(fd);
  }
}

#define MODE                                                             \
  va_list ap;                                                            \
  va_start(ap, flags);                                                   \
  mode_t mode = (flags & (O_CREAT | O_TMPFILE)) ? va_arg(ap, int) : 0;   \
  va_end(ap);

int open(const char *p, int flags, ...) {
  MODE note(p);
  int (*r)(const char *, int, ...) = dlsym(RTLD_NEXT, "open");
  return r(p, flags, mode);
}
int open64(const char *p, int flags, ...) {
  MODE note(p);
  int (*r)(const char *, int, ...) = dlsym(RTLD_NEXT, "open64");
  return r(p, flags, mode);
}
int openat(int d, const char *p, int flags, ...) {
  MODE note(p);
  int (*r)(int, const char *, int, ...) = dlsym(RTLD_NEXT, "openat");
  return r(d, p, flags, mode);
}
int openat64(int d, const char *p, int flags, ...) {
  MODE note(p);
  int (*r)(int, const char *, int, ...) = dlsym(RTLD_NEXT, "openat64");
  return r(d, p, flags, mode);
}
int __open_2(const char *p, int flags) {
  note(p);
  int (*r)(const char *, int) = dlsym(RTLD_NEXT, "__open_2");
  return r(p, flags);
}
int __open64_2(const char *p, int flags) {
  note(p);
  int (*r)(const char *, int) = dlsym(RTLD_NEXT, "__open64_2");
  return r(p, flags);
}
EOF
cc -shared -fPIC -O2 -o "$dir/shim.so" "$dir/shim.c" -ldl

log="$dir/opens.log"
status=0
if [ "$plant" = 1 ]; then
  env -u VK_DRIVER_FILES -u VK_ICD_FILENAMES GPU_OPENS_LOG="$log" LD_PRELOAD="$dir/shim.so${LD_PRELOAD:+:$LD_PRELOAD}" \
    vulkaninfo --summary > /dev/null 2>&1 || status=$?
else
  GPU_OPENS_LOG="$log" LD_PRELOAD="$dir/shim.so${LD_PRELOAD:+:$LD_PRELOAD}" "$@" || status=$?
fi

if [ ! -s "$log" ]; then
  echo "gpu-opens: no GPU device node opened"
  if [ "$plant" = 1 ]; then
    echo "gpu-opens: --plant saw nothing: the logging does not work here (or vulkaninfo is missing)" >&2
    exit 4
  fi
  exit "$status"
fi
echo "gpu-opens: GPU device nodes opened (count, executable, node):"
awk '{ print $2, $3 }' "$log" | sort | uniq -c | sed 's/^/  /'
[ "$plant" = 1 ] && exit 0
exit 3
