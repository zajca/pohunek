//! A `connect()` interposer that sends overlay-range connections to loopback.
//!
//! A CLI test loads the library with `LD_PRELOAD` to reach a fixture daemon on
//! `127.0.0.1` through the real provider resolver, which refuses a loopback peer.
//! It redirects `100.64.0.0/10` and the Tailscale IPv6 range `fd7a:115c:a1e0::/48`.
//! `POHUNEK_TEST_CONNECT_REDIRECT_PORT` replaces the redirected port when the
//! fixture listens elsewhere than the dialed port. `POHUNEK_TEST_CONNECT_REFUSE`
//! makes the connection fail with `ECONNREFUSED` instead, as a peer whose daemon
//! is not listening does.

// Rust guideline compliant 2026-10-14

use std::path::{Path, PathBuf};
use std::process::Command;

/// Compiles the interposer into `dir` and returns the library path and the
/// file it appends every original overlay-range target to (`<ip>:<port>`, or
/// `[<ip>]:<port>` for IPv6).
pub(crate) fn build(dir: &Path) -> (PathBuf, PathBuf) {
    // The production resolver must reject loopback as a NetBird peer. This
    // process-local interposer preserves that validation: the CLI resolves and
    // attempts the fixed CGNAT target, the shim records that original target,
    // and only then redirects the syscall to the loopback fixture. Nothing is
    // linked into or enabled by the production binary.
    let source = dir.join("connect-redirect.c");
    let library = dir.join("connect-redirect.so");
    let capture = dir.join("connect-targets.log");
    std::fs::write(
        &source,
        r#"#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

typedef int (*connect_fn)(int, const struct sockaddr *, socklen_t);

int connect(int fd, const struct sockaddr *address, socklen_t length) {
    static connect_fn real_connect = NULL;
    if (real_connect == NULL) {
        real_connect = (connect_fn)dlsym(RTLD_NEXT, "connect");
    }
    if (address != NULL && address->sa_family == AF_INET &&
        length >= sizeof(struct sockaddr_in)) {
        const struct sockaddr_in *original = (const struct sockaddr_in *)address;
        unsigned int host = ntohl(original->sin_addr.s_addr);
        if ((host & 0xffc00000U) == 0x64400000U) {
            const char *capture = getenv("POHUNEK_TEST_CONNECT_CAPTURE");
            if (capture != NULL) {
                char ip[INET_ADDRSTRLEN];
                char line[96];
                inet_ntop(AF_INET, &original->sin_addr, ip, sizeof(ip));
                int count = snprintf(line, sizeof(line), "%s:%u\n", ip, ntohs(original->sin_port));
                int output = open(capture, O_WRONLY | O_CREAT | O_APPEND, 0600);
                if (output >= 0) {
                    (void)write(output, line, (size_t)count);
                    (void)close(output);
                }
            }
            if (getenv("POHUNEK_TEST_CONNECT_REFUSE") != NULL) {
                errno = ECONNREFUSED;
                return -1;
            }
            struct sockaddr_in redirected = *original;
            redirected.sin_addr.s_addr = htonl(0x7f000001U);
            const char *port = getenv("POHUNEK_TEST_CONNECT_REDIRECT_PORT");
            if (port != NULL && atoi(port) > 0 && atoi(port) < 65536) {
                redirected.sin_port = htons((unsigned short)atoi(port));
            }
            return real_connect(fd, (const struct sockaddr *)&redirected, sizeof(redirected));
        }
    }
    if (address != NULL && address->sa_family == AF_INET6 &&
        length >= sizeof(struct sockaddr_in6)) {
        const struct sockaddr_in6 *original = (const struct sockaddr_in6 *)address;
        static const unsigned char ula[6] = {0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0};
        if (memcmp(original->sin6_addr.s6_addr, ula, sizeof(ula)) == 0) {
            const char *capture = getenv("POHUNEK_TEST_CONNECT_CAPTURE");
            if (capture != NULL) {
                char ip[INET6_ADDRSTRLEN];
                char line[128];
                inet_ntop(AF_INET6, &original->sin6_addr, ip, sizeof(ip));
                int count = snprintf(line, sizeof(line), "[%s]:%u\n", ip, ntohs(original->sin6_port));
                int output = open(capture, O_WRONLY | O_CREAT | O_APPEND, 0600);
                if (output >= 0) {
                    (void)write(output, line, (size_t)count);
                    (void)close(output);
                }
            }
            /* The fixture listens on IPv4 loopback; a dual-stack socket reaches it
               through the IPv4-mapped form. */
            struct sockaddr_in6 redirected = *original;
            memset(&redirected.sin6_addr, 0, sizeof(redirected.sin6_addr));
            redirected.sin6_addr.s6_addr[10] = 0xff;
            redirected.sin6_addr.s6_addr[11] = 0xff;
            redirected.sin6_addr.s6_addr[12] = 127;
            redirected.sin6_addr.s6_addr[15] = 1;
            const char *port = getenv("POHUNEK_TEST_CONNECT_REDIRECT_PORT");
            if (port != NULL && atoi(port) > 0 && atoi(port) < 65536) {
                redirected.sin6_port = htons((unsigned short)atoi(port));
            }
            return real_connect(fd, (const struct sockaddr *)&redirected, sizeof(redirected));
        }
    }
    return real_connect(fd, address, length);
}
"#,
    )
    .expect("write connect redirect source");
    let compiled = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2"])
        .arg(&source)
        .args(["-o"])
        .arg(&library)
        .arg("-ldl")
        .output()
        .expect("compile connect redirect");
    assert!(
        compiled.status.success(),
        "compile connect redirect: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    (library, capture)
}
