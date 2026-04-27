# rl_custom_isearch

Hack to add fzf-style history search (`Ctrl+R`) to programs that link
GNU readline or libedit's readline emulation layer as a shared library
(e.g. `php -a`, `mysql`, `python -i`, `irb --legacy`).

## What's different from upstream

This fork is self-contained: a single `.so` does the whole job. The
upstream design splits the work across two repos
([`lincheney/rl_custom_isearch`](https://github.com/lincheney/rl_custom_isearch)
and [`lincheney/rl_custom_function`](https://github.com/lincheney/rl_custom_function))
plus an `~/.inputrc` snippet plus a separate shell-script binary; this
fork has no second-repo equivalent and no inputrc requirement.

Upstream [`rl_custom_isearch`](https://github.com/lincheney/rl_custom_isearch) works by:

1. Loading the companion library
   [`rl_custom_function`](https://github.com/lincheney/rl_custom_function)
   via `LD_PRELOAD`. That library interposes `rl_parse_and_bind` to recognize
   a custom `$include function NAME PATH` directive in `~/.inputrc`.
2. The `.inputrc` directive then dlopens this library at the path given,
   registers `rl_custom_function` as a named readline function, and a
   subsequent `"\C-r": rl_custom_isearch` line in `.inputrc` binds it.
3. At runtime, the function shells out to a separate `rl_custom_isearch`
   binary that runs `fzf`.

This branch removes the indirection. The library:

- Registers itself programmatically on first call to `readline()` or
  `rl_callback_handler_install()` via `rl_add_defun()`. **No `.inputrc`
  config and no companion `rl_custom_function` loader required.**
- Inlines the `fzf` invocation — no separate `rl_custom_isearch` binary.
- Resolves readline/libedit symbols via `dlsym(RTLD_DEFAULT)` with a
  `dl_iterate_phdr` fallback for hosts that load readline with `RTLD_LOCAL`
  (e.g. CPython's `readline` C extension).
- **Works against both GNU readline and libedit's readline-emul layer**,
  detecting available line-replacement primitives (`rl_kill_text +
  rl_insert_text`, with a buffer-poke fallback for older libedit) at
  runtime. This means it works for libedit-linked binaries like
  Homebrew/Linuxbrew `mysql` and `python3` (with `PYTHON_BASIC_REPL=1`).
- Survives `-Bsymbolic-functions`-built libreadline (Debian/Ubuntu) by not
  relying on intra-libreadline call interposition.

## Build

```bash
cargo build --release
```

Output: `./target/release/librl_custom_isearch.so` (~380 KB).

## Use

```bash
LD_PRELOAD=/path/to/librl_custom_isearch.so php -a
LD_PRELOAD=/path/to/librl_custom_isearch.so mysql
LD_PRELOAD=/path/to/librl_custom_isearch.so PYTHON_BASIC_REPL=1 PYTHON_HISTORY=$HOME/.python_history_basic python3 -i
LD_PRELOAD=/path/to/librl_custom_isearch.so irb --legacy
```

`PYTHON_BASIC_REPL=1` is required on Python 3.13+ to bypass `_pyrepl`
(the pure-Python line editor that doesn't go through readline). On older
Python it's harmless.

`PYTHON_HISTORY` (Python 3.13+) routes the basic-repl history into its own
file, separate from the default `~/.python_history` that `_pyrepl`
sessions write to. Without this, alternating between
`PYTHON_BASIC_REPL=1` and a plain `python3` can silently wipe your
history: the two modes write subtly different formats, and any session
whose history *load* fails starts with an empty in-memory list and
truncates the file on clean exit (Python registers
`readline.write_history_file` as an `atexit` hook, which rewrites the
whole file, not appends).

`--legacy` is required on modern `irb` to bypass `reline` (the pure-Ruby
line editor) and use the `readline` extension instead, which is what this
shim hooks. On older `irb` it's harmless.

Press `Ctrl+R` (or `Ctrl+S`) to invoke fzf over the current history. The
selected entry replaces the current line and the cursor lands at
end-of-line.

Set `RL_FZF_DEBUG=1` to log registration and invocation events to stderr.

### Loading globally

To get the shim on every interactive REPL without prefixing each command,
export `LD_PRELOAD` from your shell rc. Add to `~/.zshrc` (or
`~/.bashrc`):

```bash
export LD_PRELOAD=/path/to/librl_custom_isearch.so
```

`LD_PRELOAD` is per-process and inherited by children, so anything
launched from an interactive shell will pick it up. Programs that don't
use readline simply ignore the shim. Setting it globally is safe for
day-to-day shells but **don't** export it from anything that runs setuid
or under unusual privileges — the dynamic linker strips `LD_PRELOAD` for
setuid binaries anyway, but exporting from a privileged context is bad
hygiene.

## Configuration

Same env vars as the upstream `bin/rl_custom_isearch` script:

- `FZF_TMUX_HEIGHT` — fzf overlay size (default `40%`).
- `FZF_CTRL_R_OPTS` — extra fzf args (e.g. `--preview '...'`, `--color=...`).

## Targets that don't work

REPLs that implement their own pure-language line editor and bypass
readline/libedit entirely cannot be intercepted by this shim:

- **Python 3.13+ default REPL** uses `_pyrepl` (pure Python). Set
  `PYTHON_BASIC_REPL=1` to fall back to readline, where the shim works.
- **Modern `irb`** uses `reline` (pure Ruby). Pass `--legacy` to use the
  `readline` extension instead.

Programs that statically embed readline rather than linking it as a shared
library also can't be intercepted, because their internal calls to
`readline()` don't go through the dynamic linker:

- **`bash` on Debian/Ubuntu** is built with bundled readline sources
  compiled into the binary. `ldd $(which bash)` shows no `libreadline.so`
  dependency. (Some older distros and custom builds dynamically link
  libreadline; those would work.)

If a user `.inputrc` rebinds `Ctrl+R` to something else, that binding wins
— readline's init reads `~/.inputrc` after the first `readline()` call,
which is after we register, so an explicit `"\C-r": ...` line in your
inputrc takes precedence over the shim.

## Requires

- [`fzf`](https://github.com/junegunn/fzf) on `$PATH` for the fzf UI. If
  `fzf` is missing or fails to spawn, the shim logs a one-time warning to
  stderr and falls back to readline's built-in reverse-search-history, so
  `Ctrl+R` still does *something* useful instead of being a dead key.

## License

GPL-3.0-or-later. Originally authored by [Cheney
Lin](https://github.com/lincheney) (upstream
[`lincheney/rl_custom_isearch`](https://github.com/lincheney/rl_custom_isearch));
rewritten with libedit support and programmatic registration on the
`libedit-support` branch.
