/* Prints the size and alignment of the libghostty-vt types the Rust FFI mirrors.
 * scripts/build-libghostty-vt compiles it with `zig cc` for the build target and
 * records its output in abi-layout.txt; build.rs turns that file into the
 * constants src/ffi.rs asserts its repr(C) mirrors against. */
#include <stdio.h>
#include <stddef.h>
#include <ghostty/vt.h>

#define LAYOUT(name, type) \
    printf(name "_SIZE=%zu\n" name "_ALIGN=%zu\n", sizeof(type), _Alignof(type))
#define OFFSET(name, type, field) printf(name "_OFFSET=%zu\n", offsetof(type, field))

int main(void) {
    LAYOUT("POINT", GhosttyPoint);
    OFFSET("POINT_VALUE", GhosttyPoint, value);
    LAYOUT("GRID_REF", GhosttyGridRef);
    LAYOUT("STRING", GhosttyString);
    LAYOUT("CLIPBOARD_CONTENT", GhosttyClipboardContent);
    LAYOUT("CLIPBOARD_WRITE", GhosttyClipboardWrite);
    OFFSET("CLIPBOARD_WRITE_CONTENTS", GhosttyClipboardWrite, contents);
    OFFSET("CLIPBOARD_WRITE_CONTENTS_LEN", GhosttyClipboardWrite, contents_len);
    OFFSET("CLIPBOARD_WRITE_NAME", GhosttyClipboardWrite, name);
    OFFSET("CLIPBOARD_WRITE_GRANTED", GhosttyClipboardWrite, granted);
    OFFSET("CLIPBOARD_WRITE_CAN_REMEMBER", GhosttyClipboardWrite, can_remember);
    OFFSET("CLIPBOARD_WRITE_CTX", GhosttyClipboardWrite, ctx);
    OFFSET("CLIPBOARD_WRITE_REPLY", GhosttyClipboardWrite, reply);
    LAYOUT("CLIPBOARD_WRITE_REPLY", GhosttyClipboardWriteReply);
    OFFSET("CLIPBOARD_WRITE_REPLY_RESULT", GhosttyClipboardWriteReply, result);
    OFFSET("CLIPBOARD_WRITE_REPLY_REMEMBER", GhosttyClipboardWriteReply, remember);
    LAYOUT("PROGRESS_REPORT", GhosttyTerminalProgressReport);
    OFFSET("PROGRESS_REPORT_PROGRESS", GhosttyTerminalProgressReport, progress);
    LAYOUT("ENUM", GhosttyPointTag);
    return 0;
}
