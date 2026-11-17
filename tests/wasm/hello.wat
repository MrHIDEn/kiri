;; Preview 1 module: write "hello-wasi\n" to stdout and exit 0.
;; Shared by `cargo test` and `tests/wasm.sh` (compiled with the `wat` crate).
(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "hello-wasi\n")
  (func (export "_start")
    ;; iovec { buf: 8, len: 11 } at address 0; nwritten at 20.
    (i32.store (i32.const 0) (i32.const 8))
    (i32.store (i32.const 4) (i32.const 11))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 20)))
    (call $proc_exit (i32.const 0)))
)
