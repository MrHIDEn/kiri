;; Preview 1 module: write the argument buffer, a newline, then the environment
;; buffer. Both are NUL-separated; a shell still sees the text.
(module
  (import "wasi_snapshot_preview1" "args_sizes_get" (func $args_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "args_get" (func $args_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_sizes_get" (func $environ_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_get" (func $environ_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    ;; argc at 0, argv buf size at 4; pointers at 64, bytes at 256.
    (drop (call $args_sizes_get (i32.const 0) (i32.const 4)))
    (drop (call $args_get (i32.const 64) (i32.const 256)))
    (i32.store (i32.const 32) (i32.const 256))
    (i32.store (i32.const 36) (i32.load (i32.const 4)))
    (drop (call $fd_write (i32.const 1) (i32.const 32) (i32.const 1) (i32.const 48)))
    (i32.store8 (i32.const 200) (i32.const 10))
    (i32.store (i32.const 32) (i32.const 200))
    (i32.store (i32.const 36) (i32.const 1))
    (drop (call $fd_write (i32.const 1) (i32.const 32) (i32.const 1) (i32.const 48)))
    ;; env count at 8, buf size at 12; pointers at 512, bytes at 1024.
    (drop (call $environ_sizes_get (i32.const 8) (i32.const 12)))
    (drop (call $environ_get (i32.const 512) (i32.const 1024)))
    (i32.store (i32.const 32) (i32.const 1024))
    (i32.store (i32.const 36) (i32.load (i32.const 12)))
    (drop (call $fd_write (i32.const 1) (i32.const 32) (i32.const 1) (i32.const 48)))
    (call $proc_exit (i32.const 0)))
)
