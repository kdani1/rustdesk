// WORK ITEM A: protect the JSON-RPC stdout stream from stray writes.
//
// The agent speaks newline-delimited JSON-RPC 2.0 on stdout: exactly one JSON
// object per line. The reused RustDesk client core contains a few `println!`
// calls -- notably src/client/io_loop.rs' "error receiving digest" on the
// file-write (download) path, and an RDP debug line in src/port_forward.rs --
// that would inject a non-JSON line into that stream and break the protocol.
//
// Mechanism (Windows):
//   * At startup (`install`, called first thing in run()) we capture the
//     ORIGINAL STD_OUTPUT_HANDLE and keep it as the ONE AND ONLY sink the MCP
//     writer uses, writing to it with WriteFile (never through Rust's
//     io::stdout(), which resolves STD_OUTPUT_HANDLE and would follow the
//     redirect below).
//   * We then point the process STD_OUTPUT_HANDLE at the stderr handle (or a
//     handle on NUL as a fallback) with SetStdHandle. Any `print!`/`println!`
//     anywhere in the reused core resolves GetStdHandle(STD_OUTPUT_HANDLE) to
//     that redirected sink and can never reach the protocol pipe.
//
// Why this is robust against std caching: Rust's Windows stdio implementation
// resolves the stdout HANDLE fresh on every write (it calls
// GetStdHandle(STD_OUTPUT_HANDLE) per write and does not cache it), so a
// `println!` issued AFTER this redirect goes to the redirected sink, not to the
// saved pipe. The unit test `println_after_redirect_does_not_reach_mcp_handle`
// pins exactly this property. We also install before any other startup code
// runs, so nothing writes to stdout before the redirect is in place.
//
// Non-Windows: `install` is a no-op and `write_all` falls back to io::stdout();
// the agent targets Windows.

use std::sync::atomic::{AtomicUsize, Ordering};

// The saved ORIGINAL stdout handle (as usize; 0 = not installed / unavailable,
// in which case `write_all` falls back to std stdout). HANDLE is just a pointer,
// and the process stdout handle stays valid for the life of the process, so a
// plain atomic is enough.
static MCP_HANDLE: AtomicUsize = AtomicUsize::new(0);

#[cfg(windows)]
mod sys {
    use std::os::raw::c_void;
    pub type HANDLE = *mut c_void;
    pub type BOOL = i32;
    pub type DWORD = u32;
    // (DWORD)-11 and (DWORD)-12.
    pub const STD_OUTPUT_HANDLE: DWORD = 0xFFFF_FFF5;
    pub const STD_ERROR_HANDLE: DWORD = 0xFFFF_FFF4;

    extern "system" {
        pub fn GetStdHandle(n_std_handle: DWORD) -> HANDLE;
        pub fn SetStdHandle(n_std_handle: DWORD, h_handle: HANDLE) -> BOOL;
        pub fn WriteFile(
            h_file: HANDLE,
            lp_buffer: *const c_void,
            n_bytes_to_write: DWORD,
            lp_bytes_written: *mut DWORD,
            lp_overlapped: *mut c_void,
        ) -> BOOL;
    }

    #[inline]
    pub fn invalid() -> HANDLE {
        -1isize as HANDLE
    }

    #[inline]
    pub fn is_bad(h: HANDLE) -> bool {
        h.is_null() || h == invalid()
    }

    /// Open the NUL device for writing, used as a last-resort redirect sink when
    /// there is no usable stderr handle.
    pub unsafe fn open_nul() -> HANDLE {
        type LPCWSTR = *const u16;
        extern "system" {
            fn CreateFileW(
                name: LPCWSTR,
                access: DWORD,
                share: DWORD,
                sa: *mut c_void,
                disposition: DWORD,
                flags: DWORD,
                template: HANDLE,
            ) -> HANDLE;
        }
        const GENERIC_WRITE: DWORD = 0x4000_0000;
        const FILE_SHARE_READ: DWORD = 0x1;
        const FILE_SHARE_WRITE: DWORD = 0x2;
        const OPEN_EXISTING: DWORD = 3;
        const FILE_ATTRIBUTE_NORMAL: DWORD = 0x80;
        let name: Vec<u16> = "NUL\u{0}".encode_utf16().collect();
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    }
}

/// Install the stdout guard. Call this FIRST in run(), before anything can write
/// to stdout. Idempotent-ish: calling twice would re-redirect, but run() calls
/// it once.
#[cfg(windows)]
pub fn install() {
    use sys::*;
    unsafe {
        let orig = GetStdHandle(STD_OUTPUT_HANDLE);
        if is_bad(orig) {
            // No stdout to protect; leave MCP_HANDLE = 0 so write_all falls back
            // to std stdout (which is also unavailable, but harmless).
            return;
        }
        MCP_HANDLE.store(orig as usize, Ordering::SeqCst);

        // Redirect the process stdout to stderr (fallback: NUL) so stray
        // print!/println! in the reused core cannot reach the saved pipe.
        let mut sink = GetStdHandle(STD_ERROR_HANDLE);
        if is_bad(sink) {
            sink = open_nul();
        }
        if !is_bad(sink) {
            SetStdHandle(STD_OUTPUT_HANDLE, sink);
        }
    }
}

#[cfg(not(windows))]
pub fn install() {
    // No-op on non-Windows; the MCP writer uses io::stdout() directly.
}

/// Write the given bytes to the protocol sink (the saved original stdout on
/// Windows; io::stdout() otherwise). Used by the MCP writer for every message.
pub fn write_all(bytes: &[u8]) {
    #[cfg(windows)]
    {
        let h = MCP_HANDLE.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                write_handle(h as sys::HANDLE, bytes);
            }
            return;
        }
    }
    // Fallback: non-Windows, or Windows with no saved handle.
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(bytes);
    let _ = lock.flush();
}

#[cfg(windows)]
unsafe fn write_handle(h: sys::HANDLE, bytes: &[u8]) {
    use sys::*;
    // A console/pipe/file HANDLE is written directly (no user-space buffering),
    // so there is nothing to flush afterwards. WriteFile may write fewer bytes
    // than requested, so loop until the whole buffer is out.
    let mut off = 0usize;
    while off < bytes.len() {
        let remaining = (bytes.len() - off).min(u32::MAX as usize) as DWORD;
        let mut written: DWORD = 0;
        let ok = WriteFile(
            h,
            bytes[off..].as_ptr() as *const _,
            remaining,
            &mut written,
            std::ptr::null_mut(),
        );
        if ok == 0 || written == 0 {
            break; // broken pipe / closed handle: give up, the client is gone.
        }
        off += written as usize;
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::windows::io::AsRawHandle;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Proves the core property of WORK ITEM A: after the stdout redirect is in
    // place, a write that goes through the process STD_OUTPUT_HANDLE (the path a
    // `println!` takes) lands in the redirected sink and does NOT reach the
    // saved MCP handle.
    //
    // Note on the test harness: `cargo test` installs a thread-local
    // OUTPUT_CAPTURE that the `println!` macro itself checks, so under capture a
    // bare `println!` is intercepted before it ever reaches GetStdHandle. We
    // therefore also write through `io::stdout()` directly, which does NOT
    // consult OUTPUT_CAPTURE and exercises the exact same OS handle resolution
    // (GetStdHandle(STD_OUTPUT_HANDLE), fresh per write) that `println!` uses in
    // production. Both forms are issued; only the handle-level one is asserted.
    #[test]
    fn println_after_redirect_does_not_reach_mcp_handle() {
        use sys::*;

        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir();
        let mcp_path = dir.join(format!("rd-agent-mcp-sink-{uniq}.txt"));
        let divert_path = dir.join(format!("rd-agent-divert-sink-{uniq}.txt"));

        let mcp_file = std::fs::File::create(&mcp_path).unwrap();
        let divert_file = std::fs::File::create(&divert_path).unwrap();
        let mcp_h = mcp_file.as_raw_handle() as HANDLE;
        let divert_h = divert_file.as_raw_handle() as HANDLE;

        const MCP_LINE: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1}\n";
        const STRAY: &str = "error receiving digest: boom";

        unsafe {
            let orig = GetStdHandle(STD_OUTPUT_HANDLE);

            // Point the MCP writer at the private mcp sink...
            MCP_HANDLE.store(mcp_h as usize, Ordering::SeqCst);
            // ...and redirect the process stdout at the divert sink.
            SetStdHandle(STD_OUTPUT_HANDLE, divert_h);

            // MCP protocol write -> must go ONLY to the mcp sink.
            write_all(MCP_LINE);

            // Stray output issued AFTER the redirect.
            println!("{}", STRAY); // intercepted by harness capture; harmless
            let _ = writeln!(std::io::stdout(), "{}", STRAY); // real OS handle
            let _ = std::io::stdout().flush();

            // Restore the process stdout BEFORE any assertion so the test
            // harness keeps its own stdout intact.
            SetStdHandle(STD_OUTPUT_HANDLE, orig);
            MCP_HANDLE.store(0, Ordering::SeqCst);
        }

        // Close the files so their contents are fully visible to the reader.
        drop(mcp_file);
        drop(divert_file);

        let mut mcp_contents = String::new();
        std::fs::File::open(&mcp_path)
            .unwrap()
            .read_to_string(&mut mcp_contents)
            .unwrap();
        let mut divert_contents = String::new();
        std::fs::File::open(&divert_path)
            .unwrap()
            .read_to_string(&mut divert_contents)
            .unwrap();

        let _ = std::fs::remove_file(&mcp_path);
        let _ = std::fs::remove_file(&divert_path);

        // The saved MCP handle received the protocol line and NOTHING else.
        assert_eq!(
            mcp_contents,
            String::from_utf8_lossy(MCP_LINE),
            "stray stdout must not reach the saved MCP handle"
        );
        assert!(
            !mcp_contents.contains(STRAY),
            "the MCP handle must not contain stray stdout output"
        );
        // The redirected process stdout captured the stray write.
        assert!(
            divert_contents.contains(STRAY),
            "redirected stdout should capture stray writes, got: {:?}",
            divert_contents
        );
    }
}
