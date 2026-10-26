// Rosetta compatibility for translated x86_64 programs (preloaded only into them).
//
// Rosetta sends SIGTRAP to every thread of a translated process whenever the
// process forks. A thread blocked in select/poll/epoll_wait returns EINTR even
// though no handler of the program ran. Some vendor clients treat that EINTR as
// fatal (Huawei UniVPN: "service process has exited"). Here a wait that was
// interrupted while none of the program's own signal handlers ran is resumed
// with the remaining timeout; a real signal still returns EINTR as before.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdatomic.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <time.h>

static atomic_uint handled;                 // program handlers that have run
static struct sigaction program[NSIG];      // the program's own actions
static atomic_int wrapped[NSIG];

static void trampoline(int sig, siginfo_t *info, void *context) {
    atomic_fetch_add(&handled, 1);
    struct sigaction *action = &program[sig];
    if (action->sa_flags & SA_SIGINFO) action->sa_sigaction(sig, info, context);
    else action->sa_handler(sig);
}

typedef int (*sigaction_fn)(int, const struct sigaction *, struct sigaction *);

int sigaction(int sig, const struct sigaction *act, struct sigaction *old) {
    static sigaction_fn real;
    if (!real) real = (sigaction_fn)dlsym(RTLD_NEXT, "sigaction");
    if (sig <= 0 || sig >= NSIG) return real(sig, act, old);
    struct sigaction previous = program[sig];
    int was_wrapped = atomic_load(&wrapped[sig]);
    int result;
    int custom = act && ((act->sa_flags & SA_SIGINFO) ? act->sa_sigaction != NULL
        : (act->sa_handler != SIG_DFL && act->sa_handler != SIG_IGN));
    if (custom) {
        struct sigaction replacement = *act;
        replacement.sa_flags |= SA_SIGINFO;
        replacement.sa_sigaction = trampoline;
        program[sig] = *act;
        result = real(sig, &replacement, old);
        if (result == 0) atomic_store(&wrapped[sig], 1);
        else program[sig] = previous;
    } else {
        result = real(sig, act, old);
        if (result == 0 && act) atomic_store(&wrapped[sig], 0);
    }
    if (result == 0 && old && was_wrapped && old->sa_sigaction == trampoline) *old = previous;
    return result;
}

static sighandler_t install(int sig, sighandler_t handler, int flags) {
    struct sigaction action, old;
    memset(&action, 0, sizeof action);
    action.sa_handler = handler;
    action.sa_flags = flags;
    sigemptyset(&action.sa_mask);
    sigaddset(&action.sa_mask, sig);
    if (sigaction(sig, &action, &old) < 0) return SIG_ERR;
    return old.sa_handler;
}

sighandler_t signal(int sig, sighandler_t handler) { return install(sig, handler, SA_RESTART); }
sighandler_t bsd_signal(int sig, sighandler_t handler) { return install(sig, handler, SA_RESTART); }
sighandler_t sysv_signal(int sig, sighandler_t handler) { return install(sig, handler, SA_RESETHAND | SA_NODEFER); }

static long long now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000000000LL + t.tv_nsec;
}

// Remaining nanoseconds until deadline (never negative); deadline < 0 = wait forever.
static long long remaining(long long deadline) {
    if (deadline < 0) return -1;
    long long left = deadline - now_ns();
    return left > 0 ? left : 0;
}

static int spurious(unsigned before, int result) {
    return result == -1 && errno == EINTR && atomic_load(&handled) == before;
}

int select(int n, fd_set *r, fd_set *w, fd_set *e, struct timeval *tv) {
    static int (*real)(int, fd_set *, fd_set *, fd_set *, struct timeval *);
    if (!real) real = dlsym(RTLD_NEXT, "select");
    fd_set r0, w0, e0;
    if (r) r0 = *r;
    if (w) w0 = *w;
    if (e) e0 = *e;
    long long deadline = tv ? now_ns() + tv->tv_sec * 1000000000LL + tv->tv_usec * 1000LL : -1;
    for (;;) {
        unsigned before = atomic_load(&handled);
        int result = real(n, r, w, e, tv);
        if (!spurious(before, result)) return result;
        if (r) *r = r0;
        if (w) *w = w0;
        if (e) *e = e0;
        if (tv) { long long left = remaining(deadline); tv->tv_sec = left / 1000000000LL; tv->tv_usec = left % 1000000000LL / 1000; }
        errno = 0;
    }
}

int pselect(int n, fd_set *r, fd_set *w, fd_set *e, const struct timespec *ts, const sigset_t *mask) {
    static int (*real)(int, fd_set *, fd_set *, fd_set *, const struct timespec *, const sigset_t *);
    if (!real) real = dlsym(RTLD_NEXT, "pselect");
    fd_set r0, w0, e0;
    if (r) r0 = *r;
    if (w) w0 = *w;
    if (e) e0 = *e;
    long long deadline = ts ? now_ns() + ts->tv_sec * 1000000000LL + ts->tv_nsec : -1;
    struct timespec left_ts, *wait = ts ? &left_ts : NULL;
    if (ts) left_ts = *ts;
    for (;;) {
        unsigned before = atomic_load(&handled);
        int result = real(n, r, w, e, wait, mask);
        if (!spurious(before, result)) return result;
        if (r) *r = r0;
        if (w) *w = w0;
        if (e) *e = e0;
        if (ts) { long long left = remaining(deadline); left_ts.tv_sec = left / 1000000000LL; left_ts.tv_nsec = left % 1000000000LL; }
        errno = 0;
    }
}

static int timeout_ms(long long deadline, int original) {
    if (original < 0) return original;
    long long left = remaining(deadline);
    return (int)((left + 999999) / 1000000);
}

int poll(struct pollfd *fds, nfds_t n, int timeout) {
    static int (*real)(struct pollfd *, nfds_t, int);
    if (!real) real = dlsym(RTLD_NEXT, "poll");
    long long deadline = timeout >= 0 ? now_ns() + timeout * 1000000LL : -1;
    int wait = timeout;
    for (;;) {
        unsigned before = atomic_load(&handled);
        int result = real(fds, n, wait);
        if (!spurious(before, result)) return result;
        wait = timeout_ms(deadline, timeout);
        errno = 0;
    }
}

int epoll_wait(int fd, struct epoll_event *events, int max, int timeout) {
    static int (*real)(int, struct epoll_event *, int, int);
    if (!real) real = dlsym(RTLD_NEXT, "epoll_wait");
    long long deadline = timeout >= 0 ? now_ns() + timeout * 1000000LL : -1;
    int wait = timeout;
    for (;;) {
        unsigned before = atomic_load(&handled);
        int result = real(fd, events, max, wait);
        if (!spurious(before, result)) return result;
        wait = timeout_ms(deadline, timeout);
        errno = 0;
    }
}
