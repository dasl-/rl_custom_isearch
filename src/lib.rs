#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]

use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::os::raw::{c_char, c_int, c_void};
use std::process::{Command, Stdio};
use std::sync::{Once, OnceLock};

type rl_command_func_t = extern "C" fn(c_int, c_int) -> c_int;
type rl_vcpfunc_t = extern "C" fn(*mut c_char);

const CTRL_R: c_int = 0x12;
const CTRL_S: c_int = 0x13;

// Opaque pointee for HIST_ENTRY*. Layout differs between libreadline
// (line, timestamp, data) and libedit (line, data), so we cannot pick one
// `#[repr(C)] struct` definition that's correct on both. The zero-variant-enum
// idiom is Rust's standard shape for an opaque FFI type: addresses are valid
// but the value is uninhabited, so a `*const HistEntry` can be held and
// offset but never dereferenced. `collect_history` reads just the first
// pointer (the `line` field, common to both layouts) via raw projection.
enum HistEntry {}

unsafe fn dlsym_named(handle: *mut c_void, name: &str) -> *mut c_void {
    let n = CString::new(name).unwrap();
    libc::dlsym(handle, n.as_ptr())
}

/// Find an already-loaded libreadline or libedit and return a handle to it.
/// Required because some processes (Python's C extension) load it with
/// RTLD_LOCAL, which hides its symbols from RTLD_DEFAULT / RTLD_NEXT.
fn line_lib() -> *mut c_void {
    static LIB: OnceLock<usize> = OnceLock::new();
    let h = *LIB.get_or_init(|| {
        extern "C" fn cb(
            info: *mut libc::dl_phdr_info,
            _size: usize,
            data: *mut c_void,
        ) -> c_int {
            unsafe {
                let name = CStr::from_ptr((*info).dlpi_name);
                let s = name.to_string_lossy();
                if s.contains("libreadline.so") || s.contains("libedit.so") {
                    let h = libc::dlopen(name.as_ptr(), libc::RTLD_NOLOAD | libc::RTLD_LAZY);
                    if !h.is_null() {
                        *(data as *mut *mut c_void) = h;
                        return 1;
                    }
                }
            }
            0
        }
        let mut handle: *mut c_void = std::ptr::null_mut();
        unsafe {
            libc::dl_iterate_phdr(
                Some(cb),
                &mut handle as *mut *mut c_void as *mut c_void,
            );
        }
        handle as usize
    });
    h as *mut c_void
}

/// Look up `name` starting from `primary`; if NULL, fall back to a private
/// handle on the loaded libreadline/libedit (covers RTLD_LOCAL hosts).
unsafe fn lookup(name: &str, primary: *mut c_void) -> *mut c_void {
    let p = dlsym_named(primary, name);
    if !p.is_null() {
        return p;
    }
    let h = line_lib();
    if h.is_null() {
        return std::ptr::null_mut();
    }
    dlsym_named(h, name)
}

unsafe fn resolve_fn<T: Copy>(name: &str) -> Option<T> {
    let p = lookup(name, libc::RTLD_DEFAULT);
    if p.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&p))
    }
}

unsafe fn resolve_var<T>(name: &str) -> *mut T {
    lookup(name, libc::RTLD_DEFAULT) as *mut T
}

/// Like `resolve_fn` but starts the lookup from RTLD_NEXT — for interposed
/// symbols where we want the *real* implementation underneath our wrapper.
unsafe fn resolve_next<T: Copy>(name: &str) -> Option<T> {
    let p = lookup(name, libc::RTLD_NEXT);
    if p.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&p))
    }
}

struct Symbols {
    rl_add_defun:
        Option<unsafe extern "C" fn(*const c_char, rl_command_func_t, c_int) -> c_int>,
    rl_kill_text: Option<unsafe extern "C" fn(c_int, c_int) -> c_int>,
    rl_insert_text: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    rl_forced_update_display: Option<unsafe extern "C" fn() -> c_int>,
    rl_on_new_line: Option<unsafe extern "C" fn() -> c_int>,
    rl_reverse_search_history: Option<unsafe extern "C" fn(c_int, c_int) -> c_int>,
    history_list: Option<unsafe extern "C" fn() -> *const *const HistEntry>,
    rl_line_buffer: *mut *mut c_char,
    rl_point: *mut c_int,
    rl_end: *mut c_int,
}

unsafe impl Sync for Symbols {}
unsafe impl Send for Symbols {}

fn syms() -> &'static Symbols {
    static S: OnceLock<Symbols> = OnceLock::new();
    S.get_or_init(|| unsafe {
        Symbols {
            rl_add_defun: resolve_fn("rl_add_defun"),
            rl_kill_text: resolve_fn("rl_kill_text"),
            rl_insert_text: resolve_fn("rl_insert_text"),
            rl_forced_update_display: resolve_fn("rl_forced_update_display"),
            rl_on_new_line: resolve_fn("rl_on_new_line"),
            rl_reverse_search_history: resolve_fn("rl_reverse_search_history"),
            history_list: resolve_fn("history_list"),
            rl_line_buffer: resolve_var("rl_line_buffer"),
            rl_point: resolve_var("rl_point"),
            rl_end: resolve_var("rl_end"),
        }
    })
}

fn debug() -> bool {
    std::env::var_os("RL_FZF_DEBUG").is_some()
}

fn dbg(msg: &str) {
    if debug() {
        eprintln!("[readline_fzf] {}", msg);
    }
}

fn warn_once(msg: &str) {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        eprintln!("[rl_custom_isearch] {}", msg);
    });
}

static REGISTERED: Once = Once::new();

fn register_keys() {
    REGISTERED.call_once(|| {
        let s = syms();
        let Some(add_defun) = s.rl_add_defun else {
            dbg("rl_add_defun not found; cannot register");
            return;
        };
        // Readline stores the name pointer without copying, so the lifetime
        // must outlive the process. Static byte literal is the simplest way.
        // Name matches the legacy hack so old .inputrc setups still work for
        // users mid-migration.
        let name = b"rl_custom_isearch\0".as_ptr() as *const c_char;
        unsafe {
            // rl_add_defun(name, fn, key) registers the name AND binds the key
            // in one call. Works on both GNU readline and libedit's emul layer.
            let r1 = add_defun(name, custom_isearch, CTRL_R);
            let r2 = add_defun(name, custom_isearch, CTRL_S);
            dbg(&format!("rl_add_defun ^R={} ^S={}", r1, r2));
        }
    });
}

extern "C" fn custom_isearch(count: c_int, key: c_int) -> c_int {
    dbg("custom_isearch invoked");
    if let Err(e) = run_fzf() {
        dbg(&format!("fzf error: {}", e));
        warn_once(&format!(
            "fzf invocation failed ({}); falling back to readline reverse-search",
            e
        ));
        let s = syms();
        if let Some(fallback) = s.rl_reverse_search_history {
            unsafe { fallback(count, key) };
        }
    }
    0
}

fn collect_history(s: &Symbols) -> Vec<Vec<u8>> {
    let Some(history_list) = s.history_list else {
        return Vec::new();
    };
    let mut out = Vec::new();
    unsafe {
        let mut p = history_list();
        if p.is_null() {
            return out;
        }
        while !(*p).is_null() {
            // Read just the line pointer (the first field on every HIST_ENTRY
            // variant) without forming a reference that would commit to a
            // particular struct size.
            let line_ptr: *const c_char = std::ptr::read(*p as *const *const c_char);
            if !line_ptr.is_null() {
                out.push(CStr::from_ptr(line_ptr).to_bytes().to_vec());
            }
            p = p.offset(1);
        }
    }
    out
}

fn current_line(s: &Symbols) -> Vec<u8> {
    if s.rl_line_buffer.is_null() {
        return Vec::new();
    }
    unsafe {
        let buf = *s.rl_line_buffer;
        if buf.is_null() {
            return Vec::new();
        }
        CStr::from_ptr(buf).to_bytes().to_vec()
    }
}

fn run_fzf() -> Result<(), String> {
    let s = syms();
    let history = collect_history(s);
    let initial = current_line(s);

    let height = std::env::var("FZF_TMUX_HEIGHT").unwrap_or_else(|_| "40%".to_string());
    let extra_opts = std::env::var("FZF_CTRL_R_OPTS").unwrap_or_default();

    let mut cmd = Command::new("fzf");
    cmd.arg(format!("--height={}", height))
        .arg("--tac")
        .arg("--tiebreak=index")
        .arg("+m")
        .arg("--bind=ctrl-r:toggle-sort");
    if !extra_opts.is_empty() {
        for tok in shell_split(&extra_opts) {
            cmd.arg(tok);
        }
    }
    if !initial.is_empty() {
        cmd.arg("--query")
            .arg(String::from_utf8_lossy(&initial).to_string());
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn: {}", e))?;
    {
        let mut stdin = child.stdin.take().unwrap();
        // Send history in natural order (oldest -> newest); fzf's --tac
        // displays it newest-first.
        for line in history.iter() {
            if line.contains(&b'\n') {
                continue;
            }
            if stdin.write_all(line).is_err() {
                break;
            }
            if stdin.write_all(b"\n").is_err() {
                break;
            }
        }
    }

    let mut output = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_end(&mut output);
    }
    let status = child.wait().map_err(|e| format!("wait: {}", e))?;

    // Reset display state. fzf may have left the cursor anywhere on the line
    // (and the original prompt is still drawn there). \r puts us at column 0
    // of the same line; \033[K clears to end of line. Then rl_on_new_line
    // tells readline its display tracking is reset, so the next redraw paints
    // the prompt+buffer starting at column 0 instead of appending after the
    // already-drawn prompt.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\r\x1b[K");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    unsafe {
        if let Some(onn) = s.rl_on_new_line {
            onn();
        }
    }

    if !status.success() {
        // user cancelled: redraw the original (unchanged) line
        if let Some(refresh) = s.rl_forced_update_display {
            unsafe { refresh(); }
        }
        return Ok(());
    }

    while output.last() == Some(&b'\n') {
        output.pop();
    }
    if !output.is_empty() {
        replace_line(s, &output)?;
    }
    if let Some(refresh) = s.rl_forced_update_display {
        unsafe { refresh(); }
    }
    Ok(())
}

/// Minimal shell-style splitter: handles whitespace, single + double quotes.
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (c, quote) {
            (q, None) if q == '\'' || q == '"' => quote = Some(q),
            (q, Some(q2)) if q == q2 => quote = None,
            (c, None) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (c, _) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn replace_line(s: &Symbols, text: &[u8]) -> Result<(), String> {
    let c_text = CString::new(text).map_err(|e| format!("CString: {}", e))?;
    unsafe {
        // Preferred: kill the existing line, then insert. rl_insert_text
        // advances the cursor, leaving it at end-of-line on both libreadline
        // and libedit. (libedit's rl_replace_line exists but doesn't sync
        // rl_point writes back to its internal cursor, so we don't use it.)
        if let (Some(kill), Some(insert)) = (s.rl_kill_text, s.rl_insert_text) {
            if !s.rl_end.is_null() && *s.rl_end > 0 {
                kill(0, *s.rl_end);
            }
            if !s.rl_point.is_null() {
                *s.rl_point = 0;
            }
            insert(c_text.as_ptr());
        } else if let Some(insert) = s.rl_insert_text {
            // Fallback for older libedit without rl_kill_text: poke the buffer
            // empty, then insert.
            if !s.rl_line_buffer.is_null() && !(*s.rl_line_buffer).is_null() {
                **s.rl_line_buffer = 0;
            }
            if !s.rl_point.is_null() {
                *s.rl_point = 0;
            }
            if !s.rl_end.is_null() {
                *s.rl_end = 0;
            }
            insert(c_text.as_ptr());
        } else {
            return Err("no line-replace API".into());
        }
    }
    Ok(())
}

#[no_mangle]
pub unsafe extern "C" fn readline(prompt: *const c_char) -> *mut c_char {
    // Register BEFORE the real call: real readline blocks until the user
    // submits a line, so registering after would miss the very first prompt.
    // rl_add_defun is safe pre-init on libreadline (the static keymap exists
    // at process start) and on libedit's emul layer (the funmap is created
    // lazily inside the first add_defun call).
    register_keys();
    static REAL: OnceLock<Option<unsafe extern "C" fn(*const c_char) -> *mut c_char>> =
        OnceLock::new();
    let real = REAL.get_or_init(|| resolve_next("readline"));
    match real {
        Some(f) => f(prompt),
        None => {
            // No real readline reachable. Returning NULL is the readline API
            // signal for EOF (Ctrl-D), so the host treats the prompt as
            // cleanly ended instead of crashing on a bogus pointer.
            dbg("real readline not found");
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rl_callback_handler_install(
    prompt: *const c_char,
    lhandler: rl_vcpfunc_t,
) {
    static REAL: OnceLock<Option<unsafe extern "C" fn(*const c_char, rl_vcpfunc_t)>> =
        OnceLock::new();
    let real = REAL.get_or_init(|| resolve_next("rl_callback_handler_install"));
    if let Some(f) = real {
        f(prompt, lhandler);
    } else {
        dbg("real rl_callback_handler_install not found");
    }
    // Register AFTER the real installer. The installer is non-blocking
    // (it just wires up callback state for the eventless main loop), so we
    // can run our binding setup once it returns. Doing it after also matches
    // the order the original inputrc-driven flow had — readline init runs,
    // then keymap modifications stick on the freshly-prepared keymap.
    register_keys();
}

#[cfg(test)]
mod tests {
    use super::shell_split;

    #[test]
    fn splits_unquoted() {
        assert_eq!(
            shell_split("--height 40% --tac"),
            vec!["--height", "40%", "--tac"]
        );
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(shell_split("  a   b\tc "), vec!["a", "b", "c"]);
    }

    #[test]
    fn double_quotes_keep_spaces() {
        assert_eq!(
            shell_split(r#"--bind "ctrl-d:execute(echo hi)""#),
            vec!["--bind", "ctrl-d:execute(echo hi)"]
        );
    }

    #[test]
    fn single_quotes_keep_spaces() {
        assert_eq!(
            shell_split("--preview 'cat -A'"),
            vec!["--preview", "cat -A"]
        );
    }

    #[test]
    fn nested_quotes_are_literal() {
        // Inner ' inside "..." is preserved as-is.
        assert_eq!(shell_split(r#""it's""#), vec!["it's"]);
    }

    #[test]
    fn empty_input() {
        assert!(shell_split("").is_empty());
        assert!(shell_split("   ").is_empty());
    }
}
