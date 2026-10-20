/* wxpoint milestone-5 profiling shim (poor-man's profiler, no ptrace needed).
 *
 * This deployment machine has perf_event_paranoid=4 and yama ptrace_scope=1,
 * so perf/gdb/strace ATTACH sampling is impossible. Instead we LD_PRELOAD
 * this shim into the release server:
 *
 *   - every thread (main + tokio workers, reached via pthread_create
 *     interposition) enables ITIMER_PROF with a 1 ms CPU-time period;
 *   - the SIGPROF handler records the faulting instruction pointer (ucontext
 *     REG_RIP) -> classic leaf-function statistical sampling;
 *   - every 16384 samples the buffer is dumped atomically (tmp+rename) to
 *     WXPOINT_PROF_PATH so any kill signal still leaves a usable dump;
 *   - scripts/profile.sh symbolizes the raw RIP list offline against the
 *     release binary's .symtab (nm), which survives LTO inlining exactly as
 *     a poor-man's profiler should (samples attribute to the enclosing
 *     non-inlined function = the cost center).
 *
 * Build:  gcc -shared -fPIC -o <out>.so profshim.c -ldl
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <signal.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <sys/time.h>
#include <unistd.h>
#include <ucontext.h>
#include <time.h>

#define MAXS 400000ul
static unsigned long long buf[MAXS];
static volatile unsigned long nsamples;
static char path[512];

static void dump_atomic(void) {
    unsigned long cnt = nsamples;
    char tmp[544];
    snprintf(tmp, sizeof tmp, "%s.tmp", path);
    FILE *f = fopen(tmp, "wb");
    if (!f) return;
    fwrite(&cnt, 8, 1, f);
    fwrite(buf, 8, cnt, f);
    fclose(f);
    rename(tmp, path);
}

static void onprof(int sig, void *unused_act, void *uc) {
    (void)sig; (void)unused_act;
    ucontext_t *ctx = (ucontext_t *)uc;
    unsigned long n = nsamples;
    if (n < MAXS) {
        /* x86-64 glibc: uc_mcontext.__gregs[16] == REG_RIP */
        buf[n] = ctx->uc_mcontext.gregs[16];
        nsamples = n + 1;
        if ((n & 4095u) == 4095u) dump_atomic();
    }
}

static int registered = 0;

static void start_sampling(void) {
    struct itimerval tv;
    tv.it_interval.tv_sec = 0; tv.it_interval.tv_usec = 1000; /* 1 ms CPU */
    tv.it_value.tv_sec = 0;    tv.it_value.tv_usec = 1000;
    setitimer(ITIMER_PROF, &tv, NULL);
    if (!registered) {
        registered = 1;
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = (void (*)(int))onprof; /* 3-arg trampoline trick */
        sa.sa_flags = 0;
        sigaction(SIGPROF, &sa, NULL);
    }
}

__attribute__((constructor)) static void shim_init(void) {
    const char *p = getenv("WXPOINT_PROF_PATH");
    if (!p) return; /* shim inert unless path given */
    snprintf(path, sizeof path, "%s", p);
    start_sampling();
}

struct tramp_arg { void *(*fn)(void *); void *arg; };

static void *tramp(void *p) {
    struct tramp_arg *a = (struct tramp_arg *)p;
    start_sampling(); /* new thread gets its own ITIMER_PROF */
    return a->fn(a->arg);
}

int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                   void *(*start_fn)(void *), void *args) {
    static int (*real)(pthread_t *, const pthread_attr_t *, void *(*)(void *), void *);
    if (!real) real = dlsym(RTLD_NEXT, "pthread_create");
    struct tramp_arg *a = (struct tramp_arg *)malloc(sizeof *a);
    if (!a) return real(thread, attr, start_fn, args);
    a->fn = start_fn; a->arg = args;
    return real(thread, attr, tramp, a);
}
