# rl_custom_isearch

Hack to add fzf-style history search (`Ctrl+R`) to programs that use GNU
readline or libedit's readline emulation layer (e.g. `php -a`, `mysql`,
`python -i`, `irb --legacy`, `bash`).

## What's different from upstream

Upstream `rl_custom_isearch` works by:

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
  detecting available capabilities (`rl_replace_line`, `rl_kill_text`,
  `rl_insert_text`) at runtime. This means it works for libedit-linked
  binaries like Homebrew/Linuxbrew `mysql` and `python3` (with
  `PYTHON_BASIC_REPL=1`).
- Survives `-Bsymbolic-functions`-built libreadline (Debian/Ubuntu) by not
  relying on intra-libreadline call interposition.

## Build

```bash
cargo build --release
```

Output: `./target/release/librl_custom_isearch.so` (~370 KB).

## Use

```bash
LD_PRELOAD=/path/to/librl_custom_isearch.so php -a
LD_PRELOAD=/path/to/librl_custom_isearch.so mysql
LD_PRELOAD=/path/to/librl_custom_isearch.so python3 -i   # apt python; or set PYTHON_BASIC_REPL=1 on Python 3.13+
LD_PRELOAD=/path/to/librl_custom_isearch.so bash
LD_PRELOAD=/path/to/librl_custom_isearch.so irb --legacy
```

Press `Ctrl+R` (or `Ctrl+S`) to invoke fzf over the current history. The
selected entry replaces the current line and the cursor lands at
end-of-line.

Set `RL_FZF_DEBUG=1` to log registration and invocation events to stderr.

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

## Requires

- [`fzf`](https://github.com/junegunn/fzf) on `$PATH`.

## License

GPL-3.0-or-later. Originally authored by Cheney Lin (upstream
`lincheney/rl_custom_isearch`); rewritten with libedit support and
programmatic registration on the `libedit-support` branch.
