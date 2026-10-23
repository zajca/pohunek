//! A `bind()` interposer that moves overlay-range listeners to loopback.
//!
//! The daemon refuses to bind its remote control listener outside the overlay's
//! address range, and a test host has no overlay address. A scenario loads the
//! library built here with `LD_PRELOAD` into the real daemon: the shim records
//! the original `100.64.0.0/10` bind target, then binds an ephemeral port on
//! `127.0.0.1` instead, so the daemon serves and a client reaches it through a
//! `connect()` redirect to that port. The original port never occupies a real
//! socket, so a scenario can configure any port number without a collision. An
//! explicit IPv4 bind outside the range is recorded as `<ip>:<port> -> unredirected`
//! and proceeds. Nothing is linked into or enabled by a production binary.

// Rust guideline compliant 2026-10-23

use std::path::{Path, PathBuf};
use std::process::Command;

/// Environment variable naming the file the shim appends each original target to.
pub const CAPTURE_ENV: &str = "POHUNEK_TEST_BIND_CAPTURE";

/// Compiles the interposer into `dir` and returns the library path and the
/// file it appends every original overlay-range bind target to, one
/// `<ip>:<port> -> 127.0.0.1:<local port>` line per bind (local port `0` when
/// the bind failed). The shim reads the file name from
/// [`CAPTURE_ENV`].
///
/// # Panics
///
/// Panics when the source cannot be written or the C compiler fails.
#[must_use]
pub fn build(dir: &Path) -> (PathBuf, PathBuf) {
    let source = dir.join("bind-redirect.c");
    let library = dir.join("bind-redirect.so");
    let capture = dir.join("bind-targets.log");
    std::fs::write(
        &source,
        r#"#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

typedef int (*bind_fn)(int, const struct sockaddr *, socklen_t);

int bind(int fd, const struct sockaddr *address, socklen_t length) {
    static bind_fn real_bind = NULL;
    if (real_bind == NULL) {
        real_bind = (bind_fn)dlsym(RTLD_NEXT, "bind");
    }
    if (address != NULL && address->sa_family == AF_INET &&
        length >= sizeof(struct sockaddr_in)) {
        const struct sockaddr_in *original = (const struct sockaddr_in *)address;
        unsigned int host = ntohl(original->sin_addr.s_addr);
        if ((host & 0xffc00000U) == 0x64400000U) {
            struct sockaddr_in redirected = *original;
            redirected.sin_addr.s_addr = htonl(0x7f000001U);
            redirected.sin_port = 0;
            int result = real_bind(fd, (const struct sockaddr *)&redirected, sizeof(redirected));
            const char *capture = getenv("POHUNEK_TEST_BIND_CAPTURE");
            if (capture != NULL) {
                struct sockaddr_in bound;
                socklen_t bound_length = sizeof(bound);
                unsigned int local_port = 0;
                if (result == 0 &&
                    getsockname(fd, (struct sockaddr *)&bound, &bound_length) == 0) {
                    local_port = ntohs(bound.sin_port);
                }
                char ip[INET_ADDRSTRLEN];
                char line[128];
                inet_ntop(AF_INET, &original->sin_addr, ip, sizeof(ip));
                int count = snprintf(line, sizeof(line), "%s:%u -> 127.0.0.1:%u\n", ip,
                                     ntohs(original->sin_port), local_port);
                int output = open(capture, O_WRONLY | O_CREAT | O_APPEND, 0600);
                if (output >= 0) {
                    (void)write(output, line, (size_t)count);
                    (void)close(output);
                }
            }
            return result;
        }
        if (original->sin_port != 0) {
            /* An explicit IPv4 bind outside the overlay range proceeds as asked
               and is recorded, so a scenario sees a listener on a real address. */
            const char *capture = getenv("POHUNEK_TEST_BIND_CAPTURE");
            if (capture != NULL) {
                char ip[INET_ADDRSTRLEN];
                char line[128];
                inet_ntop(AF_INET, &original->sin_addr, ip, sizeof(ip));
                int count = snprintf(line, sizeof(line), "%s:%u -> unredirected\n", ip,
                                     ntohs(original->sin_port));
                int output = open(capture, O_WRONLY | O_CREAT | O_APPEND, 0600);
                if (output >= 0) {
                    (void)write(output, line, (size_t)count);
                    (void)close(output);
                }
            }
        }
    }
    return real_bind(fd, address, length);
}
"#,
    )
    .expect("write bind redirect source");
    let compiled = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2"])
        .arg(&source)
        .args(["-o"])
        .arg(&library)
        .arg("-ldl")
        .output()
        .expect("compile bind redirect");
    assert!(
        compiled.status.success(),
        "compile bind redirect: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    (library, capture)
}
